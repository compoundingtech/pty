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
    let started = Instant::now();
    let out = rig.run(&["attach", "slow"]);
    assert_eq!(out.code, 1, "{out:?}");
    assert!(out.stderr.contains("cannot verify the current session"), "{out:?}");
    assert!(started.elapsed() >= Duration::from_millis(500));
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
        assert!(out.stderr.contains("operation-revoked"), "{out:?}");
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
