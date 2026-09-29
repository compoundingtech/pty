//! Port of tests/kill-wait.test.ts: `pty kill` returns only after the
//! daemon has fully exited, so a follow-up `rm` leaves no stray files, and a
//! shutting-down daemon never resurrects metadata that was already removed.

use pty_conformance::*;
use std::time::Duration;

fn create_session(rig: &Rig, name: &str) -> i32 {
    let d = rig.daemon(name, &["cat"], DaemonOpts::no_display_name());
    d.pid()
}

/// node: tests/kill-wait.test.ts:39
#[test]
fn daemon_is_gone_when_kill_returns() {
    let rig = Rig::new();
    let pid = create_session(&rig, "kw");
    assert!(pid_alive(pid));
    let out = rig.pty(&["kill", "kw"]);
    expect_status(&out, 0);
    expect_contains(&out.stdout(), "killed");
    assert!(process_exited(pid), "daemon {pid} still alive after kill returned");
}

/// node: tests/kill-wait.test.ts:54
#[test]
fn rm_after_kill_leaves_no_stray_files() {
    let rig = Rig::new();
    create_session(&rig, "kw2");
    expect_status(&rig.pty(&["kill", "kw2"]), 0);
    expect_status(&rig.pty(&["rm", "kw2"]), 0);
    let leftovers: Vec<String> = std::fs::read_dir(rig.root())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|f| f.starts_with("kw2"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

/// node: tests/kill-wait.test.ts:68
#[test]
fn shutdown_does_not_resurrect_removed_metadata() {
    let rig = Rig::new();
    let pid = create_session(&rig, "kw3");
    let meta = rig.meta_path("kw3");
    assert!(meta.exists());
    std::fs::remove_file(&meta).unwrap();
    kill_pid(pid, libc::SIGTERM);
    let _ = poll_for(Duration::from_secs(6), || process_exited(pid));
    std::thread::sleep(Duration::from_millis(400));
    assert!(process_exited(pid), "daemon {pid} did not exit on SIGTERM");
    assert!(!meta.exists(), "metadata was resurrected");
    let stray: Vec<String> = std::fs::read_dir(rig.root())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|f| f.starts_with("kw3.json"))
        .collect();
    assert!(stray.is_empty(), "{stray:?}");
}
