mod cli_common;

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::time::{Duration, Instant};

use cli_common::Rig;
use serde_json::json;

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
fn auto_restart_accepts_verified_missing_boot_without_restarting() {
    let rig = Rig::new();
    let mut fixture = cli_common::MissingBootSession::new(&rig, "unknown", serde_json::json!({}));
    // --auto-restart is this CLI's noninteractive restart policy (-r).
    // Legacy token verification admits the record, but the independent
    // nesting guard still refuses a daemon that cannot answer STATUS.
    let out = rig.run(&["attach", "--auto-restart", "unknown"]);
    assert_eq!(out.code, 1, "{out:?}");
    assert!(out.stderr.contains("cannot verify the current session"), "{out:?}");
    assert!(!out.stdout.contains("Restarting"), "{out:?}");
    fixture.assert_preserved(&rig);
}

#[test]
fn legacy_attach_reaches_transport_without_upgrading_metadata() {
    for sidecar in [false, true] {
        let rig = Rig::new();
        let listener = UnixListener::bind(rig.path("legacy.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let pid = std::process::id() as i32;
        rig.write_meta("legacy", json!({
            "generation": "063ff7f9-style",
            "daemonPid": pid,
            "daemonStartToken": pty_core::registry::read_process_start_token(pid).unwrap(),
        }));
        if sidecar {
            std::fs::write(rig.path("legacy.pid"), pid.to_string()).unwrap();
        }
        let before = std::fs::read(rig.path("legacy.json")).unwrap();
        // Closing each accepted connection lets attach finish without a real
        // terminal or daemon. A connection proves admission reached do_attach.
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = done.clone();
        let server = std::thread::spawn(move || {
            let mut accepted = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((socket, _)) => { accepted += 1; drop(socket); }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept legacy attach: {error}"),
                }
            }
            accepted
        });
        let out = rig.run(&["attach", "--force", "--no-restart", "legacy"]);
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(server.join().unwrap() > 0, "{out:?}");
        assert!(!out.stderr.contains("identity is unknown"), "{out:?}");
        assert_eq!(std::fs::read(rig.path("legacy.json")).unwrap(), before);
    }
}

#[test]
fn legacy_attach_without_matching_token_never_reaches_transport_even_with_force() {
    for token in [None, Some("mismatching:token")] {
        for force in [false, true] {
            let rig = Rig::new();
            let listener = UnixListener::bind(rig.path("unverified.sock")).unwrap();
            listener.set_nonblocking(true).unwrap();
            let pid = std::process::id() as i32;
            rig.write_meta("unverified", json!({
                "daemonPid": pid,
                "daemonStartToken": token,
            }));
            std::fs::write(rig.path("unverified.pid"), pid.to_string()).unwrap();
            let before = std::fs::read(rig.path("unverified.json")).unwrap();
            let args = if force {
                vec!["attach", "--force", "--no-restart", "unverified"]
            } else {
                vec!["attach", "--no-restart", "unverified"]
            };
            let out = rig.run(&args);
            assert_eq!(out.code, 1, "{out:?}");
            if token.is_none() {
                assert!(out.stderr.contains("identity is unknown"), "{out:?}");
            }
            assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
            assert_eq!(std::fs::read(rig.path("unverified.json")).unwrap(), before);
        }
    }
}

#[test]
fn auto_restart_revoked_binding_preserves_retained_artifacts_without_running_child() {
    for retained_socket in [false, true] {
        let rig = Rig::new();
        std::fs::set_permissions(&rig.root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = rig.ok(&["publication-revocation", "identity", "--json"]).json()["root_id"]
            .as_str().unwrap().to_owned();
        let operation = "revoked-attach";
        let digest = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let intent = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        rig.ok(&[
            "publication-revocation", "install", "--id", "ended",
            "--operation", operation, "--binding", digest, "--intent", intent,
            "--root-id", &root, "--json",
        ]);
        let marker = rig.scratch.join("child-ran");
        rig.write_meta("ended", json!({
            "generation": "retained-generation",
            "command": "/bin/sh",
            "args": ["-c", "printf ran > \"$1\"", "sh", marker],
            "displayCommand": "retained managed command",
            "publicationBinding": {"operation": operation, "digest": digest},
            "publicationRootId": root,
            "publicationCapability": 1,
            "tags": {"keep": "true", "st3.start-operation": operation},
            "exitedAt": cli_common::iso_now(-1000),
            "exitCode": 0
        }));
        std::fs::write(rig.path("ended.events.jsonl"), "{\"type\":\"retained\"}\n").unwrap();
        // A closed listener leaves an unconnectable socket without making the
        // row running. Refusal must preserve this artifact too, when present.
        let socket_before = if retained_socket {
            drop(UnixListener::bind(rig.path("ended.sock")).unwrap());
            let socket = std::fs::metadata(rig.path("ended.sock")).unwrap();
            Some((socket.dev(), socket.ino()))
        } else {
            None
        };
        let metadata = std::fs::read(rig.path("ended.json")).unwrap();
        let events = std::fs::read(rig.path("ended.events.jsonl")).unwrap();

        // Rig supplies null stdin: the fence must refuse without a real
        // terminal, a prompt, or ever reaching the stored command.
        let out = rig.run(&["attach", "--auto-restart", "--force", "ended"]);
        assert_eq!(out.code, 1, "{out:?}");
        // A retained socket with no recorded PID cannot prove the daemon
        // gone, so identity refuses before the revocation fence is reached.
        let reason = if retained_socket { "identity is unknown" } else { "operation-revoked" };
        assert!(out.stderr.contains(reason), "{out:?}");
        assert_eq!(std::fs::read(rig.path("ended.json")).unwrap(), metadata);
        assert_eq!(std::fs::read(rig.path("ended.events.jsonl")).unwrap(), events);
        match socket_before {
            Some(before) => {
                let socket = std::fs::metadata(rig.path("ended.sock")).unwrap();
                assert_eq!((socket.dev(), socket.ino()), before);
            }
            None => assert!(!rig.exists("ended.sock")),
        }
        assert!(!rig.exists("ended.pid"));
        assert!(!marker.exists(), "revoked attach executed the stored command");
    }
}
