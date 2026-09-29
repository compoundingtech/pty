//! Port of tests/process-tree.test.ts. Node unit-tests `process-tree.ts`
//! with injected process lists and signal hooks (snapshot order, start-token
//! identity, exact TERM-then-KILL); none of that is reachable through the
//! binary. What is observable is the contract those helpers serve: after
//! `pty kill`, every descendant of the session's child is dead — including a
//! leaf that ignores SIGHUP and SIGTERM, which only the exact SIGKILL step
//! can end. That one case is pinned here; the rest is library-only.

use pty_conformance::*;
use std::time::Duration;

/// node: tests/process-tree.test.ts:55
#[test]
fn kill_ends_a_descendant_that_ignores_hup_and_term() {
    let rig = Rig::new();
    let leaf_pid_file = rig.tmp().join("leaf.pid");
    // daemon -> sh -> sh (leaf; traps HUP/TERM) -> sleep
    let leaf = format!(
        "trap '' HUP TERM; echo $$ > '{}'; while :; do sleep 3600; done",
        leaf_pid_file.display()
    );
    let tree = format!("sh -c \"{}\" & wait", leaf.replace('"', "\\\""));
    let d = rig.daemon("tree", &["sh", "-c", &tree], DaemonOpts::no_display_name());
    let daemon_pid = d.pid();
    wait_until("leaf pid file", || leaf_pid_file.exists());
    let leaf_pid: i32 = std::fs::read_to_string(&leaf_pid_file).unwrap().trim().parse().unwrap();
    assert!(pid_alive(leaf_pid));
    let sleeper = poll_child(leaf_pid);
    assert!(sleeper > 0, "leaf has no sleep child");

    let out = rig.pty(&["kill", "tree"]);
    expect_status(&out, 0);
    assert!(process_exited(daemon_pid), "daemon {daemon_pid} survived kill");
    assert!(process_exited(leaf_pid), "signal-ignoring leaf {leaf_pid} survived kill");
    assert!(
        poll_for(Duration::from_secs(2), || process_exited(sleeper)),
        "sleep {sleeper} under the leaf survived kill"
    );
}

/// The first child of `pid` (via pgrep), waiting briefly for it to appear.
fn poll_child(pid: i32) -> i32 {
    let mut found = 0;
    let _ = poll_for(Duration::from_secs(5), || {
        let out = std::process::Command::new("pgrep").arg("-P").arg(pid.to_string()).output().unwrap();
        found = String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .and_then(|l| l.trim().parse().ok())
            .unwrap_or(0);
        found > 0
    });
    found
}
