//! Real CLI coverage, always against isolated owner-private roots.

mod cli_common;

use std::os::unix::fs::{MetadataExt, PermissionsExt};

use cli_common::{Rig, wait_until};
use serde_json::{Value, json};

const OPERATION: &str = "start-operation-exact";
const BINDING: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const INTENT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn rig() -> Rig {
    let rig = Rig::new();
    std::fs::set_permissions(&rig.root, std::fs::Permissions::from_mode(0o700)).unwrap();
    rig
}

fn root_id(rig: &Rig) -> String {
    rig.ok(&["publication-revocation", "identity", "--json"]).json()["root_id"]
        .as_str().unwrap().to_owned()
}

fn install_args<'a>(id: &'a str, root: &'a str) -> Vec<&'a str> {
    vec![
        "publication-revocation", "install", "--id", id, "--operation", OPERATION,
        "--binding", BINDING, "--intent", INTENT, "--root-id", root, "--json",
    ]
}

fn read(rig: &Rig, id: &str, operation: &str) -> Value {
    rig.ok(&[
        "publication-revocation", "read", "--id", id, "--operation", operation, "--json",
    ]).json()
}

#[test]
fn identity_is_durable_root_scoped_and_strict() {
    let rig = rig();
    let identity = rig.ok(&["publication-revocation", "identity", "--json"]).json();
    assert_eq!(identity.as_object().unwrap().len(), 1);
    assert!(identity["root_id"].as_str().is_some_and(|id| !id.is_empty()));
    assert_eq!(
        rig.ok(&["publication-revocation", "identity", "--json"]).json(),
        identity
    );
    assert!(!rig.exists("worker.json"));
    let record = rig.ok(&install_args("worker", identity["root_id"].as_str().unwrap())).json()["record"].clone();
    assert_eq!(record["rootId"], identity["root_id"]);
    let other = self::rig();
    assert_ne!(
        other.ok(&["publication-revocation", "identity", "--json"]).json(),
        identity
    );
    for flags in [
        vec![],
        vec!["--json", "--json"],
        vec!["--json", "--id", "worker"],
        vec!["--json", "--operation", OPERATION],
        vec!["--json", "--binding", BINDING],
        vec!["--json", "--intent", INTENT],
        vec!["--json", "--unknown"],
        vec!["--json", "extra"],
    ] {
        let mut args = vec!["publication-revocation", "identity"];
        args.extend(flags);
        let out = rig.run(&args);
        assert_eq!(out.code, 1, "accepted {args:?}");
        assert!(out.stdout.is_empty());
    }
    assert_eq!(
        rig.ok(&["publication-revocation", "identity", "--json"]).json(),
        identity
    );
}

#[test]
fn exact_install_replay_read_and_conflict() {
    let rig = rig();
    let root = root_id(&rig);
    let installed = rig.ok(&install_args("worker", &root));
    assert_eq!(installed.stdout.lines().count(), 1);
    let installed = installed.json();
    assert_eq!(installed["alreadyInstalled"], false);
    assert_eq!(installed["occupancy"], json!({"complete": true, "entries": []}));
    let record = &installed["record"];
    assert_eq!(record["schema"], 1);
    assert_eq!(record["name"], "worker");
    assert!(record["rootId"].as_str().is_some_and(|id| !id.is_empty()));
    assert_eq!(record["binding"], json!({"operation": OPERATION, "digest": BINDING}));
    assert_eq!(record["intentDigest"], INTENT);
    assert_eq!(record["capabilityVersion"], 1);
    assert_eq!(record["revoked"], true);
    assert_eq!(record["attestationReceiptOrIntentId"], INTENT);
    assert_eq!(record["writerUid"], unsafe { libc::geteuid() });
    assert!(record["recordedAt"].as_str().is_some_and(|value| !value.is_empty()));
    assert_eq!(read(&rig, "worker", OPERATION), *record);
    let replay = rig.ok(&install_args("worker", &root)).json();
    assert_eq!(replay["alreadyInstalled"], true);
    assert_eq!(replay["record"], *record);

    for (index, value) in [(7, INTENT), (9, BINDING)] {
        let mut args = install_args("worker", &root);
        args[index] = value;
        let out = rig.run(&args);
        assert_eq!(out.code, 1, "conflict unexpectedly accepted: {args:?}");
        assert!(out.stdout.is_empty());
        assert_eq!(read(&rig, "worker", OPERATION), *record);
    }
    for (id, operation) in [("worker", "start-operation"), ("other", OPERATION)] {
        let out = rig.run(&[
            "publication-revocation", "read", "--id", id, "--operation", operation, "--json",
        ]);
        assert_eq!(out.code, 1);
        assert!(out.stderr.contains("not found"), "{}", out.stderr);
        assert!(out.stdout.is_empty());
    }
}

#[test]
fn strict_flags_and_invalid_values_do_not_install() {
    let rig = rig();
    let root = root_id(&rig);
    let mut cases = vec![
        vec!["publication-revocation"],
        vec!["publication-revocation", "clear"],
        vec!["publication-revocation", "read", "--id", "worker", "--operation", OPERATION],
        vec!["publication-revocation", "read", "--id", "worker", "--operation", OPERATION, "--binding", BINDING, "--json"],
    ];
    for suffix in [vec!["--json"], vec!["--id", "other"], vec!["--operation", "other"], vec!["--binding", BINDING], vec!["--intent", INTENT], vec!["--root-id", "other"], vec!["--force"], vec!["extra"], vec!["--unknown"], vec!["--"]] {
        let mut args = install_args("worker", &root);
        args.extend(suffix);
        cases.push(args);
    }
    for index in [2, 4, 6, 8, 10] {
        let mut args = install_args("worker", &root);
        args.truncate(index + 1);
        cases.push(args);
    }
    for (index, value) in [
        (3, "../escape"), (5, ""), (5, "bad\noperation"),
        (7, "abc"), (7, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
        (9, "not-a-digest"), (11, "wrong-root"),
    ] {
        let mut args = install_args("worker", &root);
        args[index] = value;
        cases.push(args);
    }
    for args in cases {
        let out = rig.run(&args);
        assert_eq!(out.code, 1, "accepted {args:?}: {}", out.stdout);
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(!out.stderr.is_empty(), "{args:?}");
    }
    // A failed request must not silently install a valid fence.
    let out = rig.run(&[
        "publication-revocation", "read", "--id", "worker", "--operation", OPERATION, "--json",
    ]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("not found"), "{}", out.stderr);
}

#[test]
fn installing_against_live_session_preserves_identity_and_metadata() {
    let rig = rig();
    let root = root_id(&rig);
    // The existing survivor is deliberately unmanaged, not a revoked launch.
    rig.spawn_cat("survivor", &["--tag", "keep=true"]);
    let pid = std::fs::read(rig.path("survivor.pid")).unwrap();
    let metadata = std::fs::read(rig.path("survivor.json")).unwrap();
    let generation = rig.read_meta("survivor").unwrap()["generation"].clone();
    assert!(generation.as_str().is_some_and(|value| !value.is_empty()));
    let socket = std::fs::metadata(rig.path("survivor.sock")).unwrap();
    let installed = rig.ok(&install_args("survivor", &root)).json();
    let occupancy = &installed["occupancy"];
    assert_eq!(occupancy["complete"], true);
    assert_eq!(occupancy["entries"].as_array().unwrap().len(), 1);
    assert_eq!(occupancy["entries"][0]["name"], "survivor");
    assert_eq!(occupancy["entries"][0]["status"], "running");
    assert_eq!(occupancy["entries"][0]["pid"], String::from_utf8(pid.clone()).unwrap().trim().parse::<i64>().unwrap());
    assert_eq!(std::fs::read(rig.path("survivor.pid")).unwrap(), pid);
    assert_eq!(std::fs::read(rig.path("survivor.json")).unwrap(), metadata);
    assert_eq!(rig.read_meta("survivor").unwrap()["generation"], generation);
    let after = std::fs::metadata(rig.path("survivor.sock")).unwrap();
    assert_eq!((after.dev(), after.ino()), (socket.dev(), socket.ino()));
    assert!(std::os::unix::net::UnixStream::connect(rig.path("survivor.sock")).is_ok());
    assert_eq!(read(&rig, "survivor", OPERATION), installed["record"]);
}

#[test]
fn fences_survive_remove_gc_recreation_and_display_rename() {
    let rig = rig();
    let root = root_id(&rig);
    let record = rig.ok(&install_args("worker", &root)).json()["record"].clone();
    // These retained rows exercise ordinary cleanup, not launches carrying
    // the revoked operation/binding. No test revives a revoked producer.
    rig.write_meta("worker", json!({"generation": "ended-a"}));
    rig.ok(&["rm", "worker"]);
    assert!(!rig.exists("worker.json"));
    assert_eq!(read(&rig, "worker", OPERATION), record);
    rig.write_meta("worker", json!({"generation": "ended-b"}));
    rig.ok(&["gc"]);
    assert!(!rig.exists("worker.json"));
    assert_eq!(read(&rig, "worker", OPERATION), record);

    // A new unmanaged session is distinct from the revoked operation.
    rig.spawn_cat("worker", &["--tag", "keep=true"]);
    rig.ok(&["rename", "worker", "replacement-label"]);
    assert_eq!(rig.read_meta("worker").unwrap()["displayName"], "replacement-label");
    assert_eq!(read(&rig, "worker", OPERATION), record);
    let alias = rig.run(&[
        "publication-revocation", "read", "--id", "replacement-label", "--operation", OPERATION, "--json",
    ]);
    assert_eq!(alias.code, 1, "display name must not resolve to stable id");
    rig.ok(&["kill", "worker"]);
    wait_until("session stopped", || {
        !rig.exists("worker.pid")
            || rig.read_meta("worker").is_some_and(|meta| meta.get("exitedAt").is_some())
    });
    rig.ok(&["rm", "worker"]);
    assert_eq!(read(&rig, "worker", OPERATION), record);
    assert_eq!(rig.ok(&install_args("worker", &root)).json()["alreadyInstalled"], true);
}
