//! `PTY_RETAIN_FINISHED`: a daemon's opt-in sweep of finished records.

mod cli_common;

use std::time::Duration;

use cli_common::{DEAD_PID, Rig, iso_now, wait_until};
use serde_json::{Value, json};

const DAY_MS: i64 = 86_400_000;

/// A finished record whose daemon is proven gone.
fn write_gone_meta(rig: &Rig, name: &str, mut metadata: Value) {
    assert!(!pty_core::registry::pid_alive(DEAD_PID));
    metadata["generation"] = json!(format!("gen-{name}"));
    metadata["daemonPid"] = json!(DEAD_PID);
    metadata["daemonStartToken"] = json!("fixture:dead");
    metadata["daemonBootId"] = json!(pty_core::registry::read_boot_id().unwrap());
    rig.write_meta(name, metadata);
}

fn seed(rig: &Rig) {
    write_gone_meta(
        rig,
        "old-exit",
        json!({"exitCode": 0, "exitedAt": iso_now(-2 * DAY_MS), "createdAt": iso_now(-3 * DAY_MS)}),
    );
    write_gone_meta(
        rig,
        "new-exit",
        json!({"exitCode": 0, "exitedAt": iso_now(-60_000)}),
    );
    write_gone_meta(
        rig,
        "old-vanished",
        json!({"createdAt": iso_now(-30 * DAY_MS)}),
    );
    std::fs::write(rig.path("old-exit.events.jsonl"), "{}\n").unwrap();
}

fn stamp(rig: &Rig, name: &str) -> Option<String> {
    rig.read_meta(name)?["tags"]["retention.lost-at"]
        .as_str()
        .map(str::to_owned)
}

#[test]
fn a_daemon_started_with_a_window_removes_old_finished_records_only() {
    let rig = Rig::new();
    seed(&rig);
    let out = rig.run_env(
        &["run", "-d", "--id", "live", "--", "cat"],
        &[("PTY_RETAIN_FINISHED", "1d")],
    );
    assert_eq!(out.code, 0, "run -d live: {}", out.stderr);
    wait_until("the old exited record is removed", || {
        !rig.exists("old-exit.json")
    });
    wait_until("the vanished record is stamped", || {
        stamp(&rig, "old-vanished").is_some()
    });
    assert!(!rig.exists("old-exit.events.jsonl"));
    assert!(rig.exists("new-exit.json"));
    assert!(rig.exists("old-vanished.json"));
    assert!(rig.exists("live.json"));
    rig.kill_all();
}

#[test]
fn a_daemon_without_a_window_removes_nothing() {
    let rig = Rig::new();
    seed(&rig);
    rig.spawn_cat("live", &[]);
    // A sweep of three records takes milliseconds; give one ample time.
    std::thread::sleep(Duration::from_secs(2));
    assert!(rig.exists("old-exit.json"));
    assert!(rig.exists("new-exit.json"));
    assert_eq!(stamp(&rig, "old-vanished"), None);
    rig.kill_all();
}
