mod cli_common;

use std::os::unix::net::UnixListener;
use std::time::{Duration, Instant};

use cli_common::Rig;

#[test]
fn delayed_status_does_not_allow_unforced_attach() {
    let rig = Rig::new();
    // The listener makes the session visible to registry discovery, but it
    // never answers STATUS. The guard must refuse before ATTACH is sent.
    let _listener = UnixListener::bind(rig.path("slow.sock")).unwrap();
    let pid = std::process::id() as i32;
    rig.write_meta(
        "slow",
        serde_json::json!({
            "daemonPid": pid,
            "daemonStartToken": pty_core::registry::read_process_start_token(pid).unwrap(),
            "daemonBootId": pty_core::registry::read_boot_id().unwrap(),
        }),
    );
    let started = Instant::now();
    let out = rig.run(&["attach", "slow"]);
    assert_eq!(out.code, 1, "{out:?}");
    assert!(
        out.stderr.contains("cannot verify the current session"),
        "{out:?}"
    );
    assert!(started.elapsed() >= Duration::from_millis(500));
}

#[test]
fn auto_restart_preserves_missing_boot_live_child() {
    let rig = Rig::new();
    let mut fixture = cli_common::MissingBootSession::new(&rig, "unknown", serde_json::json!({}));
    // --auto-restart is this CLI's noninteractive restart policy (-r).
    // Unknown projects as vanished, so this reaches the destructive restart
    // guard without needing a terminal or a daemon answering STATUS.
    let out = rig.run(&["attach", "--auto-restart", "unknown"]);
    assert_eq!(out.code, 1, "{out:?}");
    assert!(out.stderr.contains("identity is unknown"), "{out:?}");
    assert!(!out.stdout.contains("Restarting"), "{out:?}");
    fixture.assert_preserved(&rig);
}
