//! Port of tests/list-liveness-budget.test.ts through the binary. A present
//! PID (including one whose kill-zero probe returns EPERM) is not daemon
//! identity evidence. Unidentified socket fleets remain listed in name order
//! as vanished, and still share one probe budget.

use pty_conformance::*;
use std::time::Instant;

/// node: tests/list-liveness-budget.test.ts:31
#[test]
fn eperm_on_kill_zero_does_not_prove_daemon_identity() {
    let rig = Rig::new();
    std::fs::write(rig.socket_path("seat"), "").unwrap();
    std::fs::write(rig.pid_path("seat"), "1").unwrap();
    let found = rig.list_entry("seat").expect("listed");
    assert_eq!(found["status"], if is_rust() { "vanished" } else { "running" });
}

/// node: tests/list-liveness-budget.test.ts:39
#[test]
fn permission_denied_fleet_without_identity_is_listed_in_name_order() {
    let rig = Rig::new();
    let mut names: Vec<String> = (0..80).map(|i| format!("seat-{:03}", 79 - i)).collect();
    for n in &names {
        std::fs::write(rig.socket_path(n), "").unwrap();
        std::fs::write(rig.pid_path(n), "1").unwrap();
    }
    let list = rig.list_json();
    let got: Vec<String> = list.iter().map(|e| e["name"].as_str().unwrap().to_string()).collect();
    names.sort();
    assert_eq!(got, names);
    let status = if is_rust() { "vanished" } else { "running" };
    assert!(list.iter().all(|e| e["status"] == status), "{list:?}");
}

/// node: tests/list-liveness-budget.test.ts:61
#[test]
fn unreachable_fleet_waits_one_shared_deadline() {
    let rig = Rig::new();
    let mut names: Vec<String> = (0..100).map(|i| format!("unreachable-{:03}", 99 - i)).collect();
    for n in &names {
        std::fs::write(rig.socket_path(n), "").unwrap();
    }
    let started = Instant::now();
    let list = rig.list_json();
    let elapsed = started.elapsed();
    // Node's shared probe budget is 500 ms; a serialized 100 × 500 ms would
    // take 50 s. Allow process start-up on top of one budget.
    assert!(elapsed.as_millis() < 3000, "list took {elapsed:?}");
    let got: Vec<String> = list.iter().map(|e| e["name"].as_str().unwrap().to_string()).collect();
    names.sort();
    assert_eq!(got, names);
    let status = if is_rust() { "vanished" } else { "running" };
    assert!(list.iter().all(|e| e["status"] == status), "{list:?}");
}
