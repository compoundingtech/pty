//! `pty restart`'s `Kill and restart? [Y/n]` for a running session, with no
//! terminal. Only `y` or `yes` restarts there, and EOF never does. Without an
//! answer the session is left running and the command exits 1, which departs
//! from Node (decision 0020). `-y` restarts without asking, as before.
//!
//! Every case sets `PTY_SESSION`, so a restart that goes ahead skips the
//! attach that would follow it. The prompt comes before that choice, and a
//! binary that restarts by mistake then exits instead of attaching.

use pty_conformance::*;
use std::path::Path;

const OUTER: (&str, &str) = ("PTY_SESSION", "outer");

fn start_count(marker: &Path) -> usize {
    std::fs::read_to_string(marker)
        .map(|s| s.lines().filter(|l| !l.is_empty()).count())
        .unwrap_or(0)
}

/// A running session that appends a line to `marker` each time it starts.
/// Returns its daemon pid.
fn spawn_running(rig: &Rig, id: &str, marker: &Path) -> i32 {
    let script = format!(
        "printf 'started\\n' >> '{}'; exec sleep 300",
        marker.display()
    );
    let d = rig.daemon(id, &["sh", "-c", &script], DaemonOpts::no_display_name());
    wait_until("first start", || start_count(marker) == 1);
    d.pid()
}

/// A restart that the prompt declined for want of an answer: exit 1, a way
/// to restart named, and the same daemon still running.
fn expect_left_running(rig: &Rig, out: &Out, id: &str, marker: &Path, pid: i32) {
    expect_status(out, 1);
    expect_contains(&out.stdout(), "Kill and restart? [Y/n]");
    expect_contains(
        &out.stderr(),
        &format!("Session \"{id}\" was not restarted"),
    );
    expect_contains(&out.stderr(), "pass -y");
    assert!(pid_alive(pid), "daemon {pid} was stopped");
    assert_eq!(rig.pid(id), Some(pid), "the session has a new daemon");
    assert_eq!(start_count(marker), 1);
    let entry = rig.list_entry(id).expect("the session is still listed");
    assert_eq!(entry["status"], "running", "{entry}");
}

/// Decision 0020: stdin at EOF answers nothing, so the running session is
/// not stopped.
#[test]
fn a_running_session_is_left_running_at_eof() {
    let rig = Rig::new();
    let marker = rig.tmp().join("starts");
    let id = "eof-running";
    let pid = spawn_running(&rig, id, &marker);

    let out = rig.pty_env(&[OUTER], &["restart", id]);
    expect_left_running(&rig, &out, id, &marker, pid);
}

/// Decision 0020: off a terminal, an empty line, which says yes at a
/// terminal, does not stop the session, and neither does relayed input.
#[test]
fn a_running_session_is_left_running_without_a_yes() {
    let rig = Rig::new();
    let marker = rig.tmp().join("starts");
    let id = "piped-running";
    let pid = spawn_running(&rig, id, &marker);

    for input in [&b"\n"[..], b"future-relay-input\n"] {
        let out = rig.pty_stdin_env(input, &[OUTER], &["restart", id]);
        expect_left_running(&rig, &out, id, &marker, pid);
    }
}

/// A piped `y` is an answer: the session is stopped and started again.
#[test]
fn a_piped_yes_restarts_a_running_session() {
    let rig = Rig::new();
    let marker = rig.tmp().join("starts");
    let id = "yes-running";
    let pid = spawn_running(&rig, id, &marker);

    let out = rig.pty_stdin_env(b"y\n", &[OUTER], &["restart", id]);
    expect_status(&out, 0);
    expect_contains(&out.stdout(), &format!("Session \"{id}\" restarted."));
    wait_until("second start", || start_count(&marker) == 2);
    assert!(process_exited(pid), "old daemon {pid} still runs");
}

/// `-y` restarts without a terminal and without asking.
#[test]
fn yes_flag_restarts_without_a_terminal() {
    let rig = Rig::new();
    let marker = rig.tmp().join("starts");
    let id = "flag-running";
    spawn_running(&rig, id, &marker);

    let out = rig.pty_env(&[OUTER], &["restart", "-y", id]);
    expect_status(&out, 0);
    expect_not_contains(&out.stdout(), "Kill and restart?");
    expect_contains(&out.stdout(), &format!("Session \"{id}\" restarted."));
    wait_until("second start", || start_count(&marker) == 2);
}
