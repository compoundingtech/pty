//! Port of tests/shutdown-backstop.test.ts: `PTY_SHUTDOWN_DEADLINE_MS` in the
//! daemon's environment (inherited from `pty run -d`) bounds a wedged
//! graceful shutdown — the daemon force-exits and SIGKILLs a child that traps
//! SIGHUP — and a prompt shutdown is untouched by the default deadline.

use pty_conformance::*;
use std::time::Duration;

/// node: tests/shutdown-backstop.test.ts:80
#[test]
fn backstop_force_exits_and_reaps_a_frozen_child() {
    let rig = Rig::new();
    let child_pid_file = rig.tmp().join("child.pid");
    let script = format!(
        "echo $$ > '{}'; trap \"\" HUP; while true; do sleep 1; done",
        child_pid_file.display()
    );
    let d = rig.daemon(
        "bs",
        &["sh", "-c", &script],
        DaemonOpts::no_display_name().invoke_env("PTY_SHUTDOWN_DEADLINE_MS", "300"),
    );
    wait_until("child pid file", || child_pid_file.exists());
    let child_pid: i32 = std::fs::read_to_string(&child_pid_file).unwrap().trim().parse().unwrap();
    assert!(pid_alive(child_pid));
    let daemon_pid = d.pid();
    kill_pid(daemon_pid, libc::SIGTERM);
    assert!(poll_for(Duration::from_secs(4), || process_exited(daemon_pid)), "daemon did not force-exit");
    assert!(poll_for(Duration::from_secs(4), || process_exited(child_pid)), "frozen child was not reaped");
    kill_pid(child_pid, libc::SIGKILL);
}

/// node: tests/shutdown-backstop.test.ts:111
#[test]
fn prompt_shutdown_is_undisturbed() {
    let rig = Rig::new();
    let d = rig.daemon("bs-ok", &["sleep", "3600"], DaemonOpts::no_display_name());
    let daemon_pid = d.pid();
    assert!(pid_alive(daemon_pid));
    kill_pid(daemon_pid, libc::SIGTERM);
    assert!(poll_for(Duration::from_secs(3), || process_exited(daemon_pid)), "daemon did not exit promptly");
}
