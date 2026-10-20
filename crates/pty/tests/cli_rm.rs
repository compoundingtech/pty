//! `pty rm` / `remove`: removes every file of a gone session, refuses a
//! running one, and reports a missing one.
//!
//! node: tests/rm-kill-ephemeral.test.ts:178-211, tests/display-name.test.ts:340-367

mod cli_common;

use cli_common::{Rig, file_names, iso_now};
use serde_json::json;

/// node: tests/rm-kill-ephemeral.test.ts:178-211
#[test]
fn removes_a_gone_session_and_refuses_a_running_one() {
    let rig = Rig::new();
    rig.write_meta(
        "gone",
        json!({
            "daemonPid": cli_common::DEAD_PID,
            "daemonStartToken": "fixture:dead",
            "daemonBootId": pty_core::registry::read_boot_id().unwrap(),
            "exitCode": 0, "exitedAt": iso_now(0), "tags": {"keep": "true"}
        }),
    );
    std::fs::write(rig.path("gone.events.jsonl"), "").unwrap();
    std::fs::write(rig.path("gone.pid"), cli_common::DEAD_PID.to_string()).unwrap();
    let out = rig.ok(&["rm", "gone"]);
    assert_eq!(out.stdout, "Session \"gone\" removed.\n");
    assert!(
        file_names(&rig.root).iter().all(|f| !f.starts_with("gone")),
        "{:?}",
        file_names(&rig.root)
    );

    let out = rig.run(&["rm", "nope"]);
    assert_eq!(out.code, 1);
    assert_eq!(out.stderr, "Session \"nope\" not found.\n");
    let out = rig.run(&["remove"]);
    assert_eq!(out.code, 1);
    assert_eq!(out.stderr, "Usage: pty rm <name>\n");

    rig.spawn_cat("live", &["--name", "Live One"]);
    let out = rig.run(&["rm", "Live One"]);
    assert_eq!(out.code, 1);
    assert_eq!(
        out.stderr,
        "Session \"live\" is still running. Use \"pty kill live\" first.\n"
    );
    assert!(rig.exists("live.json"));
    rig.ok(&["kill", "live"]);
    assert!(rig.exists("live.json"), "kill keeps the record");
    assert!(!rig.exists("live.sock"));
    let out = rig.ok(&["remove", "live"]);
    assert_eq!(out.stdout, "Session \"live\" removed.\n");
    assert!(
        file_names(&rig.root).iter().all(|f| !f.starts_with("live")),
        "{:?}",
        file_names(&rig.root)
    );
}

/// node: tests/display-name.test.ts:340-367
#[test]
fn ambiguous_reference() {
    let rig = Rig::new();
    rig.write_meta("alpha", json!({"displayName": "shared"}));
    rig.write_meta("beta", json!({"displayName": "shared"}));
    let out = rig.run(&["rm", "shared"]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr
            .contains("Session reference \"shared\" is ambiguous.")
    );
    assert!(out.stderr.contains("alpha") && out.stderr.contains("beta"));
    assert!(rig.exists("alpha.json") && rig.exists("beta.json"));
}
