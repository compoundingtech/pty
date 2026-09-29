//! Port of tests/spawner-pid-watchdog.test.ts: a daemon started with
//! `PTY_SPAWNER_PID` in its environment (inherited from `pty run -d`) shuts
//! down when that process dies, exits at once when it is already dead, and
//! ignores an unparsable value.

use pty_conformance::*;
use std::process::{Command, Stdio};
use std::time::Duration;

/// node: tests/spawner-pid-watchdog.test.ts:93
#[test]
fn daemon_shuts_down_when_the_spawner_dies() {
    let rig = Rig::new();
    let mut spawner = Command::new("sleep")
        .arg("1000000")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let spawner_pid = spawner.id() as i32;
    let d = rig.daemon(
        "wd",
        &["sleep", "3600"],
        DaemonOpts::no_display_name().invoke_env("PTY_SPAWNER_PID", &spawner_pid.to_string()),
    );
    let daemon_pid = d.pid();
    assert!(pid_alive(daemon_pid));
    let _ = spawner.kill();
    let _ = spawner.wait();
    assert!(!pid_alive(spawner_pid));
    // The watchdog polls every 5 s.
    let died = poll_for(Duration::from_secs(12), || process_exited(daemon_pid));
    if !died {
        kill_pid(daemon_pid, libc::SIGTERM);
    }
    assert!(died, "daemon {daemon_pid} outlived its spawner");
}

/// node: tests/spawner-pid-watchdog.test.ts:119
#[test]
fn daemon_exits_at_once_when_the_spawner_is_already_dead() {
    let rig = Rig::new();
    // A pid that is guaranteed dead: a spawned-and-reaped `true`.
    let mut c = Command::new("true").spawn().unwrap();
    let dead_pid = c.id() as i32;
    let _ = c.wait();
    assert!(!pid_alive(dead_pid));
    let d = rig.daemon_try(
        "wd-dead",
        &["sleep", "3600"],
        DaemonOpts::no_display_name().invoke_env("PTY_SPAWNER_PID", &dead_pid.to_string()),
    );
    // Either the launch already reports the daemon gone, or the published
    // daemon leaves within 8 s.
    let died = poll_for(Duration::from_secs(8), || match rig.pid("wd-dead") {
        Some(pid) => process_exited(pid),
        None => d.launch.status != 0 || !rig.socket_path("wd-dead").exists(),
    });
    if let Some(pid) = rig.pid("wd-dead")
        && !died
    {
        kill_pid(pid, libc::SIGTERM);
    }
    assert!(died, "daemon kept running with a dead spawner: {}", d.launch.summary());
}

/// node: tests/spawner-pid-watchdog.test.ts:157
#[test]
fn invalid_spawner_pid_disables_the_watchdog() {
    let rig = Rig::new();
    let d = rig.daemon(
        "wd-bad",
        &["sleep", "3600"],
        DaemonOpts::no_display_name().invoke_env("PTY_SPAWNER_PID", "not-a-pid"),
    );
    let daemon_pid = d.pid();
    assert!(pid_alive(daemon_pid));
    std::thread::sleep(Duration::from_millis(500));
    assert!(pid_alive(daemon_pid), "daemon exited on an unparsable PTY_SPAWNER_PID");
    kill_pid(daemon_pid, libc::SIGTERM);
    assert!(poll_for(Duration::from_secs(3), || process_exited(daemon_pid)));
}
