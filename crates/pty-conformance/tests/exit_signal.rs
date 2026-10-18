//! Port of tests/exit-signal.test.ts: a signal death of the session's child
//! is recorded as 128+signal in the metadata and, with the raw signal, on
//! the `session_exit` event; a clean exit keeps the raw code and no signal.
//! Node reads the metadata inside the daemon's 500 ms post-exit grace
//! window before the default reap; here the sessions carry `keep=true` so
//! the record stays on disk and the read cannot race the reap.

use pty_conformance::*;
use serde_json::Value;

fn events(rig: &Rig, id: &str) -> Vec<Value> {
    let raw = std::fs::read_to_string(rig.root().join(format!("{id}.events.jsonl"))).unwrap();
    raw.lines().filter(|l| !l.trim().is_empty()).map(|l| serde_json::from_str(l).unwrap()).collect()
}

/// **Wait for the event; one read after `wait_for_exit` races it.** The exit
/// metadata and the `session_exit` event are two writes, and nothing orders
/// the event first. Both daemons queue the event to an async writer and then
/// save the metadata synchronously (`finalize_exit`; node:
/// src/server.ts:593-610). Measured 2026-10-14 with the test sharing one busy
/// core: the single read found no `session_exit` in 29 runs of 30 against
/// Node and 20 of 30 against Rust, so either side can lose and the gate
/// reports whichever one did as a divergence. Waiting: 0 of 30 on each.
fn session_exit(rig: &Rig, id: &str) -> Value {
    let path = rig.root().join(format!("{id}.events.jsonl"));
    wait_until(&format!("{id} session_exit event"), || {
        std::fs::read_to_string(&path).is_ok_and(|raw| {
            raw.lines().any(|l| {
                serde_json::from_str::<Value>(l).is_ok_and(|e| e["type"] == "session_exit")
            })
        })
    });
    events(rig, id)
        .into_iter()
        .find(|e| e["type"] == "session_exit")
        .expect("session_exit event")
}

fn first_child(pid: i32) -> i32 {
    let out = std::process::Command::new("pgrep").arg("-P").arg(pid.to_string()).output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .and_then(|l| l.trim().parse().ok())
        .expect("daemon has a child")
}

/// node: tests/exit-signal.test.ts:49
#[test]
fn sigkilled_child_is_recorded_as_137() {
    let rig = Rig::new();
    let d = rig.daemon("sk", &["sh", "-c", "exec sleep 300"], DaemonOpts::keep());
    // The leaf is the daemon's direct child (exec replaces the sh).
    let leaf = first_child(d.pid());
    kill_pid(leaf, libc::SIGKILL);
    rig.wait_for_exit("sk");
    let meta = rig.meta("sk").unwrap();
    assert_eq!(meta["exitCode"], 137, "{meta}");
    let exit = session_exit(&rig, "sk");
    assert_eq!(exit["exitCode"], 137, "{exit}");
    assert_eq!(exit["signal"], 9, "{exit}");
}

/// node: tests/exit-signal.test.ts:74
#[test]
fn clean_exit_keeps_the_raw_code_and_no_signal() {
    let rig = Rig::new();
    rig.daemon("ce", &["sh", "-c", "exit 5"], DaemonOpts::keep());
    rig.wait_for_exit("ce");
    let meta = rig.meta("ce").unwrap();
    assert_eq!(meta["exitCode"], 5, "{meta}");
    assert!(meta.get("signal").is_none(), "{meta}");
    let exit = session_exit(&rig, "ce");
    assert_eq!(exit["exitCode"], 5, "{exit}");
    assert!(exit.get("signal").is_none(), "{exit}");
}
