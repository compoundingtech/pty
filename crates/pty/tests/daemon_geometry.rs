//! Effective geometry: the per-axis minimum over writable clients, GEOMETRY
//! before any output drawn for the new size, readonly viewers informed but
//! never constraining.
//!
//! node: tests/effective-geometry.test.ts; tests/integration.test.ts:1330-1475

mod daemon_support;

use std::time::Duration;

use daemon_support::*;
use pty_core::protocol::MessageType::*;
use serde_json::json;

const T: Duration = Duration::from_secs(5);

/// A child that clears and prints `X` × (2 × cols) on every SIGWINCH.
///
/// Until the script runs `trap`, SIGWINCH is ignored, and these tests resize
/// within milliseconds of attaching. So wait for the script to say the trap
/// is set: under load on macOS a resize that beat it left the child never
/// redrawing.
///
/// node: tests/effective-geometry.test.ts:28-34
fn width_daemon() -> Daemon {
    let root = short_root();
    let body = "#!/bin/bash\n\
        redraw() { c=$(stty size < /dev/tty | cut -d' ' -f2); printf '\\033[2J\\033[H'; i=0; while [ $i -lt $((c*2)) ]; do printf X; i=$((i+1)); done; }\n\
        trap redraw WINCH\n\
        touch \"${0%/*}/trapped\"\n\
        while :; do sleep 0.02; done\n";
    let path = script(&root, "width.sh", body);
    let mut cfg = config(&unique_name("geo"), path.to_str().unwrap(), &[]);
    cfg["cols"] = json!(20);
    let d = Daemon::start(&root, cfg);
    assert!(
        wait_until(T, || root.join("trapped").exists()),
        "the child never set its WINCH trap"
    );
    d
}

/// Wait for GEOMETRY(rows, cols) followed by at least one DATA.
fn wait_geometry_and_data(c: &mut Conn, rows: u16, cols: u16) -> bool {
    c.wait_for(T, |p| {
        let g = p.iter().position(|x| x.type_ == Geometry
            && pty_core::protocol::decode_geometry(&x.payload) == (rows, cols));
        g.is_some_and(|g| p[g..].iter().any(|x| x.type_ == Data))
    })
}

/// Wait for GEOMETRY(rows, cols).
fn wait_geometry(c: &mut Conn, rows: u16, cols: u16) -> bool {
    c.wait_for(T, |p| p.iter().any(|x| x.type_ == Geometry
        && pty_core::protocol::decode_geometry(&x.payload) == (rows, cols)))
}

/// Let the child finish its redraws.
fn settle(conns: &mut [&mut Conn]) {
    for c in conns.iter_mut() {
        c.quiesce(Duration::from_millis(300), Duration::from_secs(3));
    }
}

/// node: tests/effective-geometry.test.ts:152-176
#[test]
fn geometry_precedes_output_for_existing_and_smaller_attaching_clients() {
    skip_without_a_real_machine!();
    let _s = serial();
    let d = width_daemon();
    let mut large = d.connect();
    large.attach(24, 20);
    assert!(large.wait_type(Screen, T));
    large.assert_geometry_before_all_output(24, 20);
    settle(&mut [&mut large]);
    large.clear();

    let mut small = d.connect();
    small.attach(24, 10);
    assert!(wait_geometry_and_data(&mut large, 24, 10));
    assert!(small.wait_type(Screen, T));
    assert!(small.geometry_index(24, 10).is_some());
    large.assert_geometry_before_all_output(24, 10);
    small.assert_geometry_before_all_output(24, 10);
}

/// node: tests/effective-geometry.test.ts:178-214
#[test]
fn geometry_precedes_resize_and_disconnect_output() {
    skip_without_a_real_machine!();
    let _s = serial();
    let d = width_daemon();
    let mut large = d.connect();
    large.attach(24, 20);
    assert!(large.wait_type(Screen, T));
    let mut small = d.connect();
    small.attach(24, 10);
    assert!(wait_geometry_and_data(&mut large, 24, 10));
    assert!(small.wait_type(Screen, T));
    settle(&mut [&mut large, &mut small]);
    large.clear();
    small.clear();

    small.resize(24, 8);
    assert!(wait_geometry_and_data(&mut large, 24, 8));
    assert!(wait_geometry_and_data(&mut small, 24, 8));
    large.assert_geometry_before_all_output(24, 8);
    small.assert_geometry_before_all_output(24, 8);
    settle(&mut [&mut large, &mut small]);

    large.clear();
    small.shutdown();
    assert!(wait_geometry_and_data(&mut large, 24, 20));
    large.assert_geometry_before_all_output(24, 20);
}

/// node: tests/effective-geometry.test.ts:217-250
#[test]
fn readonly_viewers_get_geometry_but_never_select_size() {
    skip_without_a_real_machine!();
    let _s = serial();
    let d = width_daemon();
    let mut writable = d.connect();
    writable.attach(24, 20);
    assert!(writable.wait_type(Screen, T));

    let mut readonly = d.connect();
    readonly.peek();
    assert!(readonly.wait_type(Screen, T));
    readonly.assert_geometry_before_all_output(24, 20);
    assert_eq!(d.connect().query_status()["terminal"]["cols"], 20);
    settle(&mut [&mut writable, &mut readonly]);

    readonly.clear();
    writable.resize(24, 10);
    assert!(wait_geometry_and_data(&mut readonly, 24, 10));
    readonly.assert_geometry_before_all_output(24, 10);

    writable.shutdown();
    readonly.shutdown();
    assert!(wait_until(T, || d.connect().query_status()["terminal"]["cols"] == 10));
}

/// The shell's prompt, set so the tests can tell when it is printed.
///
/// Before each prompt, interactive bash reads the window size and writes the
/// same value straight back (readline's `set_winsize`). A resize that lands
/// between that read and that write is undone, and the child keeps the old
/// size for good. The tests below change the size the moment they have read
/// what they wait for, so every wait runs until the next prompt, by which
/// time bash has written the size back.
const PROMPT: &str = "geo> ";

fn stty_daemon() -> Daemon {
    let root = short_root();
    // Not PS1 itself: the child starts under `/bin/sh -c 'exec "$@"'`, and a
    // bash that is not interactive (macOS /bin/sh) drops PS1 from what it
    // passes on. An interactive POSIX shell runs the file named by ENV.
    let prompt = script(&root, "prompt.sh", &format!("PS1='{PROMPT}'\n"));
    let d = Daemon::start_env(
        &root,
        config(&unique_name("geo"), "sh", &[]),
        &[("ENV", prompt.to_str().unwrap())],
    );
    // The first prompt, before any client sets a size.
    let mut screen = None;
    let prompted = wait_until(T, || {
        let mut peek = d.connect();
        peek.peek_flags(true, false);
        screen = peek.wait_type(Screen, T).then(|| peek.screen()).flatten();
        screen.as_deref().is_some_and(|s| s.contains(PROMPT))
    });
    assert!(prompted, "the shell never printed its prompt: {screen:?}");
    d
}

fn stty_size(c: &mut Conn, expect: &str) {
    c.clear();
    c.data("stty size\n");
    assert!(
        c.wait_for(T, |p| {
            let out: String = p
                .iter()
                .filter(|x| x.type_ == Data)
                .map(|x| String::from_utf8_lossy(&x.payload).into_owned())
                .collect();
            out.find(expect).is_some_and(|i| out[i..].contains(PROMPT))
        }),
        "expected `{expect}` and then the prompt in {:?}",
        c.output()
    );
}

/// node: tests/integration.test.ts:1330-1352
#[test]
fn uses_the_smallest_connected_client_size() {
    skip_without_a_real_machine!();
    let _s = serial();
    let d = stty_daemon();
    let mut c1 = d.connect();
    c1.attach(50, 200);
    assert!(c1.wait_type(Screen, T));
    stty_size(&mut c1, "50 200");
    let mut c2 = d.connect();
    c2.attach(30, 100);
    assert!(c2.wait_type(Screen, T));
    stty_size(&mut c1, "30 100");
}

/// node: tests/integration.test.ts:1354-1375
#[test]
fn per_axis_minimum() {
    skip_without_a_real_machine!();
    let _s = serial();
    let d = stty_daemon();
    let mut c1 = d.connect();
    c1.attach(60, 80);
    assert!(c1.wait_type(Screen, T));
    let mut c2 = d.connect();
    c2.attach(30, 200);
    assert!(c2.wait_type(Screen, T));
    stty_size(&mut c1, "30 80");
}

/// node: tests/integration.test.ts:1377-1475
#[test]
fn size_recovers_on_disconnect_detach_and_resize() {
    skip_without_a_real_machine!();
    let _s = serial();
    let d = stty_daemon();
    let mut c1 = d.connect();
    c1.attach(50, 200);
    assert!(c1.wait_type(Screen, T));

    let mut c2 = d.connect();
    c2.attach(30, 80);
    assert!(c2.wait_type(Screen, T));
    stty_size(&mut c1, "30 80");
    c2.shutdown();
    assert!(wait_geometry(&mut c1, 50, 200));
    stty_size(&mut c1, "50 200");

    let mut c3 = d.connect();
    c3.attach(30, 80);
    assert!(c3.wait_type(Screen, T));
    stty_size(&mut c1, "30 80");
    c3.detach();
    assert!(c3.wait_closed(T));
    assert!(wait_geometry(&mut c1, 50, 200));
    stty_size(&mut c1, "50 200");

    let mut c4 = d.connect();
    c4.attach(30, 80);
    assert!(c4.wait_type(Screen, T));
    stty_size(&mut c1, "30 80");
    c4.resize(60, 250);
    assert!(wait_geometry(&mut c1, 50, 200));
    stty_size(&mut c1, "50 200");
}
