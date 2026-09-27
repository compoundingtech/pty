//! Roles: command, writable (ATTACH), readonly (PEEK); the transitions on
//! one socket; what each role may do; STATUS shapes and counts.
//!
//! node: tests/integration.test.ts:854-1214, 2009-2238

mod daemon_support;

use std::time::Duration;

use daemon_support::*;
use pty_core::protocol::MessageType::*;
use pty_core::protocol::{MessageType, encode_packet};
use serde_json::json;

const T: Duration = Duration::from_secs(5);

fn cat_daemon() -> Daemon {
    let root = short_root();
    Daemon::start(&root, config(&unique_name("role"), "cat", &[]))
}

/// node: tests/integration.test.ts:854-899
#[test]
fn peek_then_attach_makes_the_socket_writable() {
    skip_without_a_real_machine!();
    let _s = serial();
    let d = cat_daemon();
    let mut c = d.connect();
    c.peek();
    assert!(c.wait_type(Screen, T));
    c.attach(20, 70);
    assert!(c.wait_count(Screen, 2, T));
    c.data("writable-again\n");
    assert!(c.wait_text("writable-again", T));

    let mut stats = d.connect();
    let st = stats.query_status();
    assert_eq!(st["terminal"]["rows"], 20);
    assert_eq!(st["terminal"]["cols"], 70);
    assert_eq!(st["clients"]["attached"], 1);
    assert_eq!(st["clients"]["readOnly"], 0);

    c.resize(18, 60);
    assert!(c.wait_for(T, |p| p
        .iter()
        .any(|x| x.type_ == Geometry && pty_core::protocol::decode_geometry(&x.payload) == (18, 60))));
}

/// node: tests/integration.test.ts:901-959
#[test]
fn attach_then_peek_makes_the_socket_readonly() {
    skip_without_a_real_machine!();
    let _s = serial();
    let d = cat_daemon();
    let mut c = d.connect();
    c.attach(20, 70);
    assert!(c.wait_type(Screen, T));
    c.peek();
    assert!(c.wait_count(Screen, 2, T));

    c.resize(18, 60);
    c.data("must-not-reach-cat\n");
    c.status();
    let st = c.wait_status(T).unwrap();
    assert_eq!(st["clients"]["attached"], 0);
    assert_eq!(st["clients"]["readOnly"], 1);
    // The size is not reverted when the last writable leaves.
    assert_eq!(st["terminal"]["rows"], 20);
    assert_eq!(st["terminal"]["cols"], 70);

    let mut observer = d.connect();
    observer.attach(20, 70);
    assert!(observer.wait_type(Screen, T));
    observer.data("accepted-by-cat\n");
    assert!(observer.wait_text("accepted-by-cat", T));
    assert!(!observer.output().contains("must-not-reach-cat"));
}

/// node: tests/integration.test.ts:961-1019
#[test]
fn malformed_attach_changes_no_role() {
    skip_without_a_real_machine!();
    let _s = serial();
    let d = cat_daemon();
    let mut peeker = d.connect();
    peeker.peek();
    assert!(peeker.wait_type(Screen, T));
    peeker.send(&encode_packet(MessageType::Attach, &[0, 0]));
    peeker.status();
    let st = peeker.wait_status(T).unwrap();
    assert_eq!(st["clients"]["attached"], 0);
    assert_eq!(st["clients"]["readOnly"], 1);

    let mut attached = d.connect();
    attached.attach(20, 70);
    assert!(attached.wait_type(Screen, T));
    attached.send(&encode_packet(MessageType::Attach, &[0, 0]));
    attached.status();
    let st = attached.wait_status(T).unwrap();
    assert_eq!(st["clients"]["attached"], 1);
    assert_eq!(st["clients"]["readOnly"], 1);
    assert_eq!(st["terminal"]["rows"], 20);
    assert_eq!(st["terminal"]["cols"], 70);
}

fn winch_reporter(root: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let marker = root.join("winch");
    let body = format!(
        "#!/bin/bash\ntrap 'echo WINCH >> {}' WINCH\necho READY\nwhile :; do sleep 0.02; done\n",
        marker.display()
    );
    (script(root, "reporter.sh", &body), marker)
}

/// node: tests/integration.test.ts:1021-1043
#[test]
fn attach_at_the_current_size_sends_no_sigwinch() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let (reporter, marker) = winch_reporter(&root);
    let mut cfg = config(&unique_name("role"), reporter.to_str().unwrap(), &[]);
    cfg["rows"] = json!(40);
    cfg["cols"] = json!(120);
    let d = Daemon::start(&root, cfg);
    let mut c = d.connect();
    c.attach(40, 120);
    assert!(c.wait_type(Screen, T));
    assert!(c.wait_text("READY", T));
    std::thread::sleep(Duration::from_millis(150));
    assert!(!marker.exists(), "SIGWINCH was sent on a size-matched attach");
}

/// node: tests/integration.test.ts:1045-1065
#[test]
fn attach_at_a_different_size_nudges_a_redraw() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let (reporter, marker) = winch_reporter(&root);
    let mut cfg = config(&unique_name("role"), reporter.to_str().unwrap(), &[]);
    cfg["rows"] = json!(40);
    cfg["cols"] = json!(120);
    let d = Daemon::start(&root, cfg);
    let mut c = d.connect();
    c.wait_for(Duration::from_millis(100), |_| false);
    c.attach(24, 80);
    assert!(c.wait_type(Screen, T));
    assert!(
        wait_until(Duration::from_millis(250) + T, || std::fs::read_to_string(&marker)
            .map(|s| s.contains("WINCH"))
            .unwrap_or(false)),
        "no SIGWINCH after a resizing attach"
    );
}

/// node: tests/integration.test.ts:1103-1132
#[test]
fn peeker_data_is_ignored() {
    skip_without_a_real_machine!();
    let _s = serial();
    let d = cat_daemon();
    let mut writer = d.connect();
    writer.attach(24, 80);
    assert!(writer.wait_type(Screen, T));
    let mut peeker = d.connect();
    peeker.peek();
    assert!(peeker.wait_type(Screen, T));
    peeker.data("from-peeker\n");
    writer.data("from-writer\n");
    assert!(writer.wait_text("from-writer", T));
    assert!(!writer.output().contains("from-peeker"));
}

/// node: tests/integration.test.ts:1134-1159
#[test]
fn peeker_resize_is_ignored() {
    skip_without_a_real_machine!();
    let _s = serial();
    let d = cat_daemon();
    let mut writer = d.connect();
    writer.attach(24, 80);
    assert!(writer.wait_type(Screen, T));
    let mut peeker = d.connect();
    peeker.peek();
    assert!(peeker.wait_type(Screen, T));
    peeker.resize(10, 30);
    peeker.settle(Duration::from_millis(100));
    let st = writer.query_status();
    assert_eq!(st["terminal"]["rows"], 24);
    assert_eq!(st["terminal"]["cols"], 80);
    assert!(writer.geometries().iter().all(|g| *g == (24, 80)));
}

/// node: tests/integration.test.ts:1161-1184
#[test]
fn peeker_receives_live_data() {
    skip_without_a_real_machine!();
    let _s = serial();
    let d = cat_daemon();
    let mut writer = d.connect();
    writer.attach(24, 80);
    assert!(writer.wait_type(Screen, T));
    let mut peeker = d.connect();
    peeker.peek();
    assert!(peeker.wait_type(Screen, T));
    writer.data("seen-by-peeker\n");
    assert!(peeker.wait_for(T, |p| p
        .iter()
        .any(|x| x.type_ == Data && String::from_utf8_lossy(&x.payload).contains("seen-by-peeker"))));
}

/// PEEK replays the alternate screen's content without the `?1049h`
/// prefix; ATTACH starts at byte 0 with it.
///
/// node: tests/integration.test.ts:1186-1214; tests/screen-replay-altscreen.test.ts:62-83
#[test]
fn peek_captures_alt_screen_content_and_attach_prefixes_the_mode() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let d = Daemon::start(
        &root,
        config(
            &unique_name("role"),
            "sh",
            &[
                "-c",
                "printf '\\033[?1049h\\033[?1000h\\033[?1003h\\033[?1h'; echo ALT-MARKER; sleep 30",
            ],
        ),
    );
    let mut peeker = d.connect();
    assert!(wait_until(T, || {
        peeker.clear();
        peeker.peek();
        peeker.wait_type(Screen, T) && peeker.screen().unwrap().contains("ALT-MARKER")
    }));
    let screen = peeker.screen().unwrap();
    assert!(!screen.starts_with("\x1b[?1049h"), "{screen:?}");
    assert!(screen.contains("\x1b[?1000h"), "{screen:?}");

    let mut attacher = d.connect();
    attacher.attach(24, 80);
    assert!(attacher.wait_type(Screen, T));
    let screen = attacher.screen().unwrap();
    assert!(screen.starts_with("\x1b[?1049h"), "{screen:?}");
    assert!(screen.contains("ALT-MARKER"));
}

/// node: tests/integration.test.ts:2009-2066, 2120-2173; tests/stats-cli.test.ts:127-151
#[test]
fn status_counts_and_connection_shapes() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let d = Daemon::start(
        &root,
        config(&unique_name("role"), "sh", &["-c", "i=0; while [ $i -lt 30 ]; do echo line$i; i=$((i+1)); done; cat"]),
    );
    let mut writer = d.connect();
    writer.attach(20, 70);
    assert!(writer.wait_type(Screen, T));
    let mut peeker = d.connect();
    peeker.peek();
    assert!(peeker.wait_type(Screen, T));

    let mut stats = d.connect();
    let st = stats.query_status();
    assert_eq!(st["name"], d.name);
    assert_eq!(st["clients"]["total"], 2);
    assert_eq!(st["clients"]["attached"], 1);
    assert_eq!(st["clients"]["readOnly"], 1);
    assert_eq!(
        st["clients"]["connections"],
        json!([
            {"role": "writable", "rows": 20, "cols": 70, "lastRequestSequence": 1,
             "constrains": {"rows": true, "cols": true}},
            {"role": "readonly", "constrains": {"rows": false, "cols": false}}
        ])
    );
    assert_eq!(st["terminal"]["rows"], 20);
    assert_eq!(st["terminal"]["cols"], 70);
    assert_eq!(st["terminal"]["scrollbackCapacity"], 20 + 10000);
    assert!(st["terminal"]["scrollbackUsed"].as_u64().unwrap() > 20);
    assert_eq!(st["process"]["alive"], true);
    assert_eq!(st["process"]["exitCode"], json!(null));
    assert!(st["process"]["pid"].is_number());
    assert_ne!(st["process"]["pid"], st["daemon"]["pid"]);
    assert_eq!(st["daemon"]["pid"], d.pid);
    assert!(st["process"]["resources"]["rssKb"].is_number());
    assert!(st["daemon"]["resources"]["cpuPercent"].is_number());
    assert_eq!(st["modes"], json!({"sgrMouse": false, "cursorHidden": false, "kittyKeyboard": false, "kittyKeyboardFlags": []}));
    assert!(st["uptimeSeconds"].is_number());
    assert_eq!(st["createdAt"], d.meta().unwrap()["createdAt"]);
    let keys: Vec<&str> = st.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        ["name", "terminal", "process", "daemon", "clients", "modes", "uptimeSeconds", "createdAt"]
    );
}

/// node: tests/integration.test.ts:2222-2238, 2240-2259
#[test]
fn status_after_exit_and_mode_flags() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let d = Daemon::start(
        &root,
        config(&unique_name("role"), "sh", &["-c", "printf '\\033[?1006h\\033[?25l\\033[>1u'; sleep 0.3; exit 7"]),
    );
    let mut c = d.connect();
    c.attach(24, 80);
    assert!(c.wait_type(Screen, T));
    // **A screen frame does not mean the child has spoken.** Attach publishes
    // the current screen, which can be empty and can arrive before the child's
    // escapes have been read and parsed. Asserting straight after it is a race
    // that only loses when the child is slow.
    //
    // Silber.pty hit it on a Mac under suite load, on this exact line, with
    // `sgrMouse` still false. Reproduced on Linux by giving the child a
    // `sleep 0.4` before its `printf`, which is what the load was doing.
    assert!(
        wait_until(T, || c.query_status()["modes"]["sgrMouse"] == true),
        "the child's mode escapes were never parsed"
    );
    let st = c.query_status();
    assert_eq!(st["modes"]["sgrMouse"], true);
    assert_eq!(st["modes"]["cursorHidden"], true);
    assert_eq!(st["modes"]["kittyKeyboard"], true);
    assert_eq!(st["modes"]["kittyKeyboardFlags"], json!([1]));
    assert!(c.wait_type(Exit, T));
    let st = c.query_status();
    assert_eq!(st["process"]["alive"], false);
    assert_eq!(st["process"]["exitCode"], 7);
    assert_eq!(st["process"]["pid"], json!(null));
    assert_eq!(st["process"]["resources"], json!(null));
}

/// node: tests/integration.test.ts:2068-2118
#[test]
fn command_socket_writes_without_negotiating_or_counting() {
    skip_without_a_real_machine!();
    let _s = serial();
    let d = cat_daemon();
    let mut watcher = d.connect();
    watcher.attach(24, 80);
    assert!(watcher.wait_type(Screen, T));

    let mut cmd = d.connect();
    cmd.data("from-command-socket\n");
    cmd.resize(13, 37);
    cmd.status();
    let st = cmd.wait_status(T).unwrap();
    assert!(watcher.wait_text("from-command-socket", T));
    assert_eq!(st["terminal"]["rows"], 24);
    assert_eq!(st["terminal"]["cols"], 80);
    assert_eq!(st["clients"], json!({"total": 1, "attached": 1, "readOnly": 0,
        "connections": [{"role": "writable", "rows": 24, "cols": 80, "lastRequestSequence": 1,
                         "constrains": {"rows": true, "cols": true}}]}));
    // A command socket is live: it receives DATA broadcasts, but never a
    // GEOMETRY or a SCREEN.
    cmd.settle(Duration::from_millis(100));
    assert!(cmd.types().iter().all(|t| matches!(t, Status | Data)), "{:?}", cmd.types());
}

/// node: tests/integration.test.ts:2175-2220
#[test]
fn a_writable_that_switches_to_peek_relinquishes_its_constraints() {
    skip_without_a_real_machine!();
    let _s = serial();
    let d = cat_daemon();
    let mut big = d.connect();
    big.attach(50, 120);
    assert!(big.wait_type(Screen, T));
    let mut small = d.connect();
    small.attach(24, 80);
    assert!(small.wait_type(Screen, T));
    assert!(big.wait_for(T, |p| p.iter().any(|x| x.type_ == Geometry
        && pty_core::protocol::decode_geometry(&x.payload) == (24, 80))));

    small.peek();
    assert!(small.wait_count(Screen, 2, T));
    assert!(big.wait_for(T, |p| p.iter().any(|x| x.type_ == Geometry
        && pty_core::protocol::decode_geometry(&x.payload) == (50, 120))));
    let st = big.query_status();
    assert_eq!(st["terminal"]["rows"], 50);
    assert_eq!(st["terminal"]["cols"], 120);
    assert_eq!(
        st["clients"]["connections"][0],
        json!({"role": "writable", "rows": 50, "cols": 120, "lastRequestSequence": 1,
               "constrains": {"rows": true, "cols": true}})
    );
    assert_eq!(st["clients"]["connections"][1]["role"], "readonly");
}

/// DETACH ends the socket; a re-attach on a new socket gets the earlier
/// output in its SCREEN.
///
/// node: tests/integration.test.ts:318-354
#[test]
fn detach_ends_the_socket_and_reattach_replays() {
    skip_without_a_real_machine!();
    let _s = serial();
    let d = cat_daemon();
    let mut c = d.connect();
    c.attach(24, 80);
    assert!(c.wait_type(Screen, T));
    c.data("before-detach\n");
    assert!(c.wait_text("before-detach", T));
    c.detach();
    assert!(c.wait_closed(T));

    let mut again = d.connect();
    again.attach(24, 80);
    assert!(again.wait_type(Screen, T));
    assert!(again.screen().unwrap().contains("before-detach"));
    let st = again.query_status();
    assert_eq!(st["clients"]["attached"], 1);
}

/// A non-readonly ATTACH stamps `lastAttachAt`.
///
/// node: src/server.ts:951-956; tests/metadata-events.test.ts:204-227
#[test]
fn attach_stamps_last_attach_at() {
    skip_without_a_real_machine!();
    let _s = serial();
    let d = cat_daemon();
    assert!(d.meta().unwrap().get("lastAttachAt").is_none());
    let mut peeker = d.connect();
    peeker.peek();
    assert!(peeker.wait_type(Screen, T));
    assert!(d.meta().unwrap().get("lastAttachAt").is_none());
    let mut c = d.connect();
    c.attach(24, 80);
    assert!(c.wait_type(Screen, T));
    assert!(wait_until(T, || d.meta().unwrap()["lastAttachAt"].is_string()));
}

/// `peek --full` returns scrollback; the viewport peek does not.
///
/// node: tests/peek-wait.test.ts:91-107; tests/integration.test.ts:1812-1862
#[test]
fn peek_full_returns_scrollback_and_plain_peek_has_no_escapes() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let d = Daemon::start(
        &root,
        config(&unique_name("role"), "sh", &["-c", "i=1; while [ $i -le 120 ]; do echo line-$i; i=$((i+1)); done; sleep 30"]),
    );
    let mut c = d.connect();
    assert!(wait_until(T, || {
        c.clear();
        c.peek_flags(true, false);
        c.wait_type(Screen, T) && c.screen().unwrap().contains("line-120")
    }));
    let viewport = c.screen().unwrap();
    assert!(viewport.lines().count() <= 24, "{viewport}");
    assert!(!viewport.contains("\x1b["));
    assert!(!viewport.contains("line-1\n"));

    let mut full = d.connect();
    full.peek_flags(true, true);
    assert!(full.wait_type(Screen, T));
    let text = full.screen().unwrap();
    assert!(text.lines().count() >= 100, "{}", text.lines().count());
    assert!(text.starts_with("line-1\n"), "{text:?}");

    let mut ansi = d.connect();
    ansi.peek_flags(false, false);
    assert!(ansi.wait_type(Screen, T));
    assert!(ansi.screen().unwrap().contains("\x1b["));
}
