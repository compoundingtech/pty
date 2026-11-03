//! `peek --wait` against a real session: a marker printed while the wait
//! runs satisfies it even when the same write scrolls it off the screen, and
//! on a timeout a marker from before the wait is told apart from one that
//! never appeared.

mod cli_common;

use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use cli_common::{Rig, wait_until};

/// One write puts the marker and 200 lines after it on the terminal, so no
/// look at the screen, however timed, ever shows the marker.
const MARKER_THEN_SCROLL: &str = r#"printf 'END 004\n%s\n' "$(seq 1 200)""#;

fn start(rig: &Rig, name: &str, script: &str) {
    rig.ok(&["run", "-d", "--id", name, "--", "sh", "-c", script]);
    wait_until("socket up", || {
        UnixStream::connect(rig.path(&format!("{name}.sock"))).is_ok()
    });
}

fn scrolled_away(rig: &Rig, name: &str) -> bool {
    let screen = rig.ok(&["peek", "--plain", name]).stdout;
    let full = rig.ok(&["peek", "--plain", "--full", name]).stdout;
    !screen.contains("END 004") && full.contains("END 004")
}

#[test]
fn a_marker_scrolled_away_while_waiting_satisfies_the_wait() {
    let rig = Rig::new();
    let go = rig.scratch.join("go");
    let script = format!(
        "while [ ! -e '{}' ]; do sleep 0.02; done; {MARKER_THEN_SCROLL}; exec cat",
        go.display()
    );
    start(&rig, "scroll", &script);
    let opener = {
        let go = go.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            std::fs::write(&go, b"").unwrap();
        })
    };
    let started = Instant::now();
    let out = rig.run(&["peek", "--plain", "--wait", "END 004", "-t", "10", "scroll"]);
    opener.join().unwrap();
    assert_eq!(out.code, 0, "{out:?}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert!(scrolled_away(&rig, "scroll"));
}

#[test]
fn a_marker_from_before_the_wait_is_reported_as_missed() {
    let rig = Rig::new();
    start(&rig, "early", &format!("{MARKER_THEN_SCROLL}; exec cat"));
    wait_until("the marker scrolled away", || scrolled_away(&rig, "early"));

    let out = rig.run(&["peek", "--plain", "--wait", "END 004", "-t", "1", "early"]);
    assert_eq!(out.code, 1, "{out:?}");
    assert_eq!(
        out.stderr.trim_end(),
        "Timed out after 1s waiting for \"END 004\".\n\
         It is in the scrollback from before the wait began, no longer on screen."
    );
    let out = rig.run(&["peek", "--plain", "--wait", "NEVER", "-t", "1", "early"]);
    assert_eq!(out.code, 1, "{out:?}");
    assert_eq!(
        out.stderr.trim_end(),
        "Timed out after 1s waiting for \"NEVER\"."
    );
}
