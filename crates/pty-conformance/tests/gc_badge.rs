//! Port of the badge and restart halves of
//! tests/gc-flap-clear-badge-root-len.test.ts: `pty list` renders
//! `[flapping]` instead of `[permanent]` for `strategy.status=flapping`, and
//! `pty restart` drops the gc bookkeeping tags. The root-length half lives
//! in pty_root.rs. The flapping classifier itself (gc-flapping, gc-permanent,
//! gc-abandoned, gc-generation-guard) is dropped in docs/parity.md §12.

use pty_conformance::*;
use serde_json::json;
use std::time::Duration;

/// Metadata of an exited session that gc previously marked flapping, with
/// all four bookkeeping tags.
fn write_flapping_exited(rig: &Rig, id: &str, extra_tags: &[(&str, &str)]) {
    let mut tags = serde_json::Map::new();
    tags.insert("strategy".into(), json!("permanent"));
    tags.insert("strategy.status".into(), json!("flapping"));
    tags.insert("strategy.consecutive-fast-fails".into(), json!("3"));
    tags.insert("strategy.last-respawn-at".into(), json!(iso_timestamp(-5)));
    tags.insert("strategy.command-hash".into(), json!("0123456789abcdef"));
    for (k, v) in extra_tags {
        tags.insert((*k).into(), json!(v));
    }
    let meta = json!({
        "command": "sh", "args": ["-c", "exit 1"], "displayCommand": "sh -c 'exit 1'",
        "cwd": "/tmp",
        "createdAt": iso_timestamp(-600),
        "exitedAt": iso_timestamp(0),
        "exitCode": 1,
        "tags": tags,
    });
    std::fs::write(rig.meta_path(id), meta.to_string()).unwrap();
    let ev = rig.root().join(format!("{id}.events.jsonl"));
    if !ev.exists() {
        std::fs::write(ev, "").unwrap();
    }
}

fn write_running(rig: &Rig, id: &str, tags: serde_json::Value) {
    let meta = json!({
        "command": "sh", "args": [], "displayCommand": "sh",
        "daemonPid": std::process::id(),
        "daemonStartToken": pty_core::registry::read_process_start_token(std::process::id() as i32)
            .expect("live fixture identity"),
        "daemonBootId": pty_core::registry::read_boot_id().expect("live fixture boot identity"),
        "cwd": "/tmp",
        "createdAt": iso_timestamp(0),
        "tags": tags,
    });
    std::fs::write(rig.meta_path(id), meta.to_string()).unwrap();
    std::fs::write(rig.pid_path(id), std::process::id().to_string()).unwrap();
}

/// node: tests/gc-flap-clear-badge-root-len.test.ts:84
#[test]
fn restart_drops_all_four_gc_bookkeeping_tags() {
    let rig = Rig::new();
    write_flapping_exited(&rig, "fc1", &[("role", "test")]);
    // PTY_SESSION makes restart take its "already inside a session, don't
    // attach" branch, so it returns 0 instead of attaching.
    let out = rig.pty_env(&[("PTY_SESSION", "outer")], &["restart", "-y", "fc1"]);
    expect_status(&out, 0);
    std::thread::sleep(Duration::from_millis(300));
    let meta = rig.meta("fc1").expect("metadata");
    let tags = &meta["tags"];
    assert_eq!(tags["strategy"], "permanent", "{meta}");
    assert_eq!(tags["role"], "test", "{meta}");
    for key in [
        "strategy.status",
        "strategy.consecutive-fast-fails",
        "strategy.last-respawn-at",
        "strategy.command-hash",
    ] {
        assert!(tags.get(key).is_none(), "{key} survived restart: {meta}");
    }
}

/// node: tests/gc-flap-clear-badge-root-len.test.ts:118
#[test]
fn running_flapping_session_shows_flapping_badge() {
    let rig = Rig::new();
    write_running(
        &rig,
        "fc2",
        json!({"strategy": "permanent", "strategy.status": "flapping"}),
    );
    let out = rig.pty(&["list"]);
    expect_status(&out, 0);
    expect_contains(&out.stdout(), "[flapping]");
    expect_not_contains(&out.stdout(), "[permanent]");
}

/// node: tests/gc-flap-clear-badge-root-len.test.ts:142
#[test]
fn session_without_flapping_status_shows_permanent_badge() {
    let rig = Rig::new();
    write_running(&rig, "fc3", json!({"strategy": "permanent"}));
    let out = rig.pty(&["list"]);
    expect_status(&out, 0);
    expect_contains(&out.stdout(), "[permanent]");
    expect_not_contains(&out.stdout(), "[flapping]");
}
