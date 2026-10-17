//! Permanent revocations use a private real filesystem, never process signals.

mod registry_support;

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use pty_core::registry::{self, PublicationBinding, SessionMetadata, revocation};
use sha2::{Digest, Sha256};

struct Root(PathBuf);

impl Root {
    fn new() -> Self {
        let path = registry_support::root().join(registry_support::unique_name("rev"));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path.canonicalize().unwrap())
    }

    fn run<T>(&self, f: impl FnOnce() -> T) -> T {
        registry::with_root(&self.0, f)
    }

    fn record(&self, name: &str, operation: &str) -> PathBuf {
        self.0.join(".publication-revocations/v1")
            .join(format!("{:x}", Sha256::digest(name.as_bytes())))
            .join(format!("{:x}.json", Sha256::digest(operation.as_bytes())))
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn binding(operation: &str) -> PublicationBinding {
    PublicationBinding { operation: operation.to_string(), digest: "a".repeat(64) }
}

fn intent() -> String { "b".repeat(64) }

fn install(name: &str, binding: &PublicationBinding, intent_digest: &str) -> std::io::Result<registry::RevocationInstall> {
    let root_id = revocation::root_id()?;
    revocation::install(name, binding, intent_digest, &root_id)
}

fn private_write(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn legacy_tag_only_checks_allow_0755_roots_only_without_a_store() {
    let root = Root::new();
    fs::set_permissions(&root.0, fs::Permissions::from_mode(0o755)).unwrap();
    root.run(|| {
        assert!(revocation::read("seat", "op").unwrap().is_none());
        revocation::check("seat", None, Some("op")).unwrap();
        assert!(revocation::check("seat", Some(&binding("op")), Some("op")).is_err());
        assert!(revocation::root_id().is_err());
        assert_eq!(fs::metadata(&root.0).unwrap().mode() & 0o7777, 0o755);
        assert!(!root.0.join(".publication-revocations").exists());
    });
}

#[test]
fn symlink_ancestor_enrollment_and_checks_use_the_same_canonical_root() {
    let base = Root::new();
    let real_parent = base.0.join("real");
    let real_root = real_parent.join("registry");
    fs::create_dir(&real_parent).unwrap();
    fs::create_dir(&real_root).unwrap();
    fs::set_permissions(&real_root, fs::Permissions::from_mode(0o700)).unwrap();
    let alias = base.0.join("alias");
    symlink(&real_parent, &alias).unwrap();
    let selected = alias.join("registry");
    registry::with_root(&selected, || {
        assert!(revocation::read("seat", "op").unwrap().is_none());
        revocation::check("seat", None, Some("op")).unwrap();
        revocation::check("seat", Some(&binding("op")), Some("op")).unwrap();
        let identity = revocation::root_id().unwrap();
        install("seat", &binding("op"), &intent()).unwrap();
        assert_eq!(revocation::check("seat", Some(&binding("op")), Some("op")).unwrap_err().to_string(), "operation-revoked");
        registry::with_root(&real_root, || {
            assert_eq!(revocation::root_id().unwrap(), identity);
            assert!(revocation::read("seat", "op").unwrap().is_some());
        });
    });
}

#[test]
fn legacy_0755_roots_refuse_present_malformed_and_symlink_stores() {
    for attack in ["enrolled", "empty", "file", "symlink", "dangling-symlink"] {
        let root = Root::new();
        root.run(|| {
            let store = root.0.join(".publication-revocations");
            match attack {
                "enrolled" => { revocation::root_id().unwrap(); }
                "empty" => fs::create_dir(&store).unwrap(),
                "file" => private_write(&store, b"{malformed"),
                "symlink" => symlink(&root.0, &store).unwrap(),
                "dangling-symlink" => symlink(root.0.join("missing"), &store).unwrap(),
                _ => unreachable!(),
            }
            fs::set_permissions(&root.0, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(revocation::read("seat", "op").is_err(), "{attack}");
            assert!(revocation::check("seat", None, Some("op")).is_err(), "{attack}");
            assert!(revocation::check("seat", Some(&binding("op")), Some("op")).is_err(), "{attack}");
            assert_eq!(fs::metadata(&root.0).unwrap().mode() & 0o7777, 0o755);
        });
    }
}

#[test]
fn before_publication_exact_fence_rejects_without_taking_creation_lock() {
    let root = Root::new();
    root.run(|| {
        let original = binding("old-operation");
        assert!(revocation::read("seat", &original.operation).unwrap().is_none());
        revocation::check("seat", Some(&original), Some(&original.operation)).unwrap();
        let installed = install("seat", &original, &intent()).unwrap();
        assert_eq!(installed.occupancy["entries"], serde_json::json!([]));
        let _lock = registry::acquire_lock("seat").unwrap();
        let error = revocation::check("seat", Some(&original), Some(&original.operation)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "operation-revoked");
        let changed_digest = PublicationBinding { digest: "c".repeat(64), ..original.clone() };
        assert_eq!(revocation::check("seat", Some(&changed_digest), Some(&original.operation)).unwrap_err().to_string(), "operation-revoked");
        assert_eq!(revocation::check("seat", None, Some(&original.operation)).unwrap_err().to_string(), "operation-revoked");
        assert_eq!(revocation::check("seat", Some(&original), None).unwrap_err().to_string(), "operation-revoked");
        revocation::check("other", Some(&original), Some(&original.operation)).unwrap();
        let replacement = binding("replacement");
        revocation::check("seat", Some(&replacement), Some(&replacement.operation)).unwrap();
        revocation::check("seat", None, None).unwrap();
    });
}

#[test]
fn binding_validation_and_tag_agreement_fail_closed() {
    let root = Root::new();
    root.run(|| {
        for operation in ["", "line\nfeed", &"x".repeat(1025)] {
            assert!(install("seat", &binding(operation), &intent()).is_err());
        }
        for digest in ["A".repeat(64), "a".repeat(63), "g".repeat(64)] {
            assert!(install("seat", &PublicationBinding { operation: "op".into(), digest }, &intent()).is_err());
        }
        assert!(install("../escape", &binding("op"), &intent()).is_err());
        assert!(install("seat", &binding("op"), "not-a-digest").is_err());
        revocation::root_id().unwrap();
        revocation::check("seat", None, Some("op")).unwrap();
        assert!(revocation::check("seat", Some(&binding("op")), None).is_err());
        assert!(revocation::check("seat", Some(&binding("op")), Some("other")).is_err());
        // Operation strings are hashed, not interpolated into a path.
        let operation = "../../elsewhere/operation";
        install("seat", &binding(operation), &intent()).unwrap();
        assert!(root.record("seat", operation).is_file());
    });
}

#[test]
fn durable_replay_never_replaces_record_and_conflicts_do_not_clear_it() {
    let root = Root::new();
    root.run(|| {
        let original = binding("op");
        let first = install("seat", &original, &intent()).unwrap();
        assert!(!first.already_installed);
        let path = root.record("seat", "op");
        let bytes = fs::read(&path).unwrap();
        let inode = fs::metadata(&path).unwrap().ino();
        let second = install("seat", &original, &intent()).unwrap();
        assert!(second.already_installed);
        assert_eq!(first.record, second.record);
        assert_eq!(second.record.capability_version, 1);
        assert!(second.record.revoked);
        assert_eq!(second.record.attestation_receipt_or_intent_id, intent());
        // SAFETY: geteuid has no preconditions.
        assert_eq!(second.record.writer_uid, unsafe { libc::geteuid() });
        assert!(registry::parse_iso8601_ms(&second.record.recorded_at).is_some());
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o600);
        assert_eq!(fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o7777, 0o700);
        assert_eq!(install("seat", &original, &"c".repeat(64)).unwrap_err().kind(), ErrorKind::AlreadyExists);
        let conflicting = PublicationBinding { digest: "d".repeat(64), ..original };
        assert_eq!(install("seat", &conflicting, &intent()).unwrap_err().kind(), ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(revocation::read("seat", "op").unwrap(), Some(first.record));
        assert_eq!(revocation::root_id().unwrap(), second.record.root_id);
    });
}

#[test]
fn install_reports_existing_live_occupancy_without_mutating_any_session_file() {
    let root = Root::new();
    root.run(|| {
        let metadata = SessionMetadata {
            generation: Some("survivor-generation".into()),
            daemon_pid: Some(std::process::id() as i32),
            ..Default::default()
        };
        registry::write_metadata_publication("seat", &metadata).unwrap();
        registry::write_pid("seat", std::process::id()).unwrap();
        fs::write(registry::events_path("seat"), "survivor events\n").unwrap();
        let socket = std::os::unix::net::UnixListener::bind(registry::socket_path("seat")).unwrap();
        let paths = [registry::metadata_path("seat"), registry::pid_path("seat"), registry::events_path("seat")];
        let before: Vec<_> = paths.iter().map(|path| fs::read(path).unwrap()).collect();
        let socket_inode = fs::metadata(registry::socket_path("seat")).unwrap().ino();
        let installed = install("seat", &binding("old"), &intent()).unwrap();
        assert_eq!(installed.occupancy["entries"][0]["status"], "running");
        assert_eq!(installed.occupancy["entries"][0]["metadata"]["generation"], "survivor-generation");
        assert!(revocation::check("seat", Some(&binding("old")), Some("old")).is_err());
        for (path, bytes) in paths.iter().zip(before) { assert_eq!(fs::read(path).unwrap(), bytes); }
        assert_eq!(fs::metadata(registry::socket_path("seat")).unwrap().ino(), socket_inode);
        drop(socket);
    });
}

#[test]
fn incomplete_occupancy_is_not_a_successful_install_acknowledgment() {
    let root = Root::new();
    root.run(|| {
        fs::write(registry::metadata_path("seat"), "{broken").unwrap();
        assert!(install("seat", &binding("op"), &intent()).is_err());
        // The durable fence remains installed even though admission cannot use
        // this failed observation as evidence that the name is safe.
        assert!(revocation::read("seat", "op").unwrap().is_some());
        assert_eq!(fs::read_to_string(registry::metadata_path("seat")).unwrap(), "{broken");
    });
}

#[test]
fn records_survive_cleanup_recreate_and_display_name_changes() {
    let root = Root::new();
    root.run(|| {
        install("seat", &binding("old"), &intent()).unwrap();
        let path = root.record("seat", "old");
        let bytes = fs::read(&path).unwrap();
        registry::write_metadata_publication("seat", &SessionMetadata::default()).unwrap();
        registry::set_display_name("seat", Some("renamed display")).unwrap();
        registry::cleanup_all("seat").unwrap();
        registry::write_metadata_publication("seat", &SessionMetadata { generation: Some("replacement".into()), ..Default::default() }).unwrap();
        registry::cleanup("seat");
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(revocation::check("seat", Some(&binding("old")), Some("old")).unwrap_err().to_string(), "operation-revoked");
        revocation::check("seat", Some(&binding("replacement")), Some("replacement")).unwrap();
    });
}

#[test]
fn malformed_and_forged_exact_records_refuse_without_session_mutation() {
    for field in ["schema", "rootId", "name", "operation", "digest", "intentDigest", "capabilityVersion", "revoked", "attestationReceiptOrIntentId", "writerUid", "recordedAt", "unknown"] {
        let root = Root::new();
        root.run(|| {
            let installed = install("seat", &binding("op"), &intent()).unwrap();
            let path = root.record("seat", "op");
            let mut value = serde_json::to_value(installed.record).unwrap();
            match field {
                "schema" => value[field] = serde_json::json!(2),
                "operation" => value["binding"][field] = serde_json::json!("different"),
                "digest" => value["binding"][field] = serde_json::json!("INVALID"),
                "capabilityVersion" => value[field] = serde_json::json!(2),
                "revoked" => value[field] = serde_json::json!(false),
                // SAFETY: geteuid has no preconditions.
                "writerUid" => value[field] = serde_json::json!(u64::from(unsafe { libc::geteuid() }) + 1),
                _ => value[field] = serde_json::json!("forged"),
            }
            private_write(&path, &serde_json::to_vec(&value).unwrap());
            assert!(revocation::read("seat", "op").is_err(), "{field}");
            assert!(revocation::check("seat", Some(&binding("op")), Some("op")).is_err(), "{field}");
            private_write(&path, b"{malformed");
            assert!(revocation::read("seat", "op").is_err());
            private_write(&path, &vec![b'x'; 16 * 1024 + 1]);
            assert!(revocation::read("seat", "op").is_err());
        });
    }
}

#[test]
fn unsafe_record_type_mode_and_symlink_are_refused() {
    for attack in ["symlink", "mode", "directory", "hardlink"] {
        let root = Root::new();
        root.run(|| {
            install("seat", &binding("op"), &intent()).unwrap();
            let path = root.record("seat", "op");
            let saved = root.0.join("saved");
            fs::rename(&path, &saved).unwrap();
            match attack {
                "symlink" => symlink(&saved, &path).unwrap(),
                "mode" => { fs::rename(&saved, &path).unwrap(); fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap(); }
                "directory" => fs::create_dir(&path).unwrap(),
                "hardlink" => fs::hard_link(&saved, &path).unwrap(),
                _ => unreachable!(),
            }
            assert!(revocation::read("seat", "op").is_err(), "{attack}");
            assert!(install("seat", &binding("op"), &intent()).is_err(), "{attack}");
        });
    }
}

#[test]
fn unsafe_root_store_and_identity_fail_closed() {
    for target in ["root", "store", "version", "name", "identity"] {
        for symlink_attack in [false, true] {
            let root = Root::new();
            root.run(|| {
                install("seat", &binding("op"), &intent()).unwrap();
                let store = root.0.join(".publication-revocations");
                let version = store.join("v1");
                let path = match target {
                    "root" => root.0.clone(),
                    "store" => store,
                    "version" => version.clone(),
                    "name" => root.record("seat", "op").parent().unwrap().to_path_buf(),
                    "identity" => version.join("root.json"),
                    _ => unreachable!(),
                };
                if symlink_attack {
                    let saved = root.0.parent().unwrap().join(registry_support::unique_name("saved"));
                    fs::rename(&path, &saved).unwrap();
                    symlink(&saved, &path).unwrap();
                    assert!(revocation::read("seat", "op").is_err(), "{target}");
                    fs::remove_file(&path).unwrap();
                    fs::rename(saved, path).unwrap();
                } else {
                    let mode = if target == "identity" { 0o644 } else { 0o755 };
                    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
                    assert!(revocation::read("seat", "op").is_err(), "{target}");
                }
            });
        }
    }
}

#[test]
fn copied_or_forged_root_identity_cannot_retarget_a_record() {
    let first = Root::new();
    let second = Root::new();
    first.run(|| { install("seat", &binding("op"), &intent()).unwrap(); });
    second.run(|| { revocation::root_id().unwrap(); });
    let original = first.0.join(".publication-revocations/v1/root.json");
    let destination = second.0.join(".publication-revocations/v1/root.json");
    private_write(&destination, &fs::read(&original).unwrap());
    second.run(|| {
        assert!(revocation::read("seat", "op").is_err());
        assert!(install("seat", &binding("op"), &intent()).is_err());
    });
    first.run(|| {
        let mut identity: serde_json::Value = serde_json::from_slice(&fs::read(&original).unwrap()).unwrap();
        identity["inode"] = serde_json::json!(0);
        private_write(&original, &serde_json::to_vec(&identity).unwrap());
        assert!(revocation::check("seat", Some(&binding("op")), Some("op")).is_err());
    });
}

#[test]
fn busy_name_does_not_install_a_fence() {
    let root = Root::new();
    root.run(|| {
        let _lock = registry::acquire_lock("seat").unwrap();
        assert_eq!(install("seat", &binding("op"), &intent()).unwrap_err().kind(), ErrorKind::WouldBlock);
        assert!(revocation::read("seat", "op").unwrap().is_none());
    });
}

#[test]
fn partial_store_or_malformed_root_identity_never_means_unrevoked() {
    for damage in ["missing-version", "missing-identity", "malformed-identity"] {
        let root = Root::new();
        root.run(|| {
            install("seat", &binding("op"), &intent()).unwrap();
            let version = root.0.join(".publication-revocations/v1");
            let identity = version.join("root.json");
            match damage {
                "missing-version" => fs::rename(&version, root.0.join("retained-version")).unwrap(),
                "missing-identity" => fs::remove_file(identity).unwrap(),
                "malformed-identity" => private_write(&identity, b"{"),
                _ => unreachable!(),
            }
            assert!(revocation::check("seat", Some(&binding("op")), Some("op")).is_err(), "{damage}");
            assert!(revocation::read("seat", "op").is_err(), "{damage}");
            assert!(revocation::check("seat", None, Some("op")).is_err(), "{damage}");
        });
    }
}

#[test]
fn root_creation_resolves_symlink_ancestors_but_refuses_a_final_root_symlink() {
    let root = Root::new();
    let destination = root.0.join("destination");
    fs::create_dir(&destination).unwrap();
    let alias = root.0.join("alias");
    symlink(&destination, &alias).unwrap();
    let selected = alias.join("new-parent/new-root");
    registry::with_root(&selected, || {
        install("seat", &binding("op"), &intent()).unwrap();
        assert_eq!(revocation::check("seat", Some(&binding("op")), Some("op")).unwrap_err().to_string(), "operation-revoked");
    });
    assert!(destination.join("new-parent/new-root").is_dir());
    let final_alias = root.0.join("root-alias");
    symlink(destination.join("new-parent/new-root"), &final_alias).unwrap();
    registry::with_root(&final_alias, || {
        assert!(revocation::root_id().is_err());
        assert!(revocation::read("seat", "op").is_err());
    });
}

#[test]
fn creating_a_new_root_enrolls_private_directories_without_chmodding_existing_roots() {
    let parent = Root::new();
    let path = parent.0.join("new-root");
    registry::with_root(&path, || {
        install("seat", &binding("op"), &intent()).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o700);
    });
    let shared = parent.0.join("shared");
    fs::create_dir(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
    registry::with_root(&shared, || {
        assert!(install("seat", &binding("op"), &intent()).is_err());
    });
    assert_eq!(fs::metadata(&shared).unwrap().mode() & 0o7777, 0o755);
    assert!(!shared.join(".publication-revocations").exists());
}

#[test]
fn install_requires_exact_previously_enrolled_root_identity() {
    let root = Root::new();
    root.run(|| {
        let expected = "0".repeat(64);
        assert!(revocation::install("seat", &binding("op"), &intent(), &expected).is_err());
        assert!(!root.0.join(".publication-revocations").exists());
        let actual = revocation::root_id().unwrap();
        let wrong = if actual == expected { "1".repeat(64) } else { expected };
        assert_eq!(revocation::install("seat", &binding("op"), &intent(), &wrong).unwrap_err().kind(), ErrorKind::PermissionDenied);
        assert!(!root.record("seat", "op").exists());
        let installed = revocation::install("seat", &binding("op"), &intent(), &actual).unwrap();
        let bytes = fs::read(root.record("seat", "op")).unwrap();
        assert!(revocation::install("seat", &binding("op"), &intent(), &wrong).is_err());
        assert_eq!(fs::read(root.record("seat", "op")).unwrap(), bytes);
        assert_eq!(installed.record.root_id, actual);
    });
}

#[test]
fn crash_leftover_temporary_files_do_not_poison_enrollment_or_fences() {
    let root = Root::new();
    root.run(|| {
        let version = root.0.join(".publication-revocations/v1");
        fs::create_dir_all(&version).unwrap();
        fs::set_permissions(version.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&version, fs::Permissions::from_mode(0o700)).unwrap();
        // A crash before atomic rename leaves only an unpublished, single-link
        // temporary. It must not become an identity or prevent a fresh install.
        private_write(&version.join(".tmp.crashed.identity"), b"{partial");
        let identity = revocation::root_id().unwrap();
        let directory = root.record("seat", "op").parent().unwrap().to_path_buf();
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        private_write(&directory.join(".tmp.crashed.record"), b"{partial");
        let first = revocation::install("seat", &binding("op"), &intent(), &identity).unwrap();
        assert_eq!(fs::metadata(version.join("root.json")).unwrap().nlink(), 1);
        assert_eq!(fs::metadata(root.record("seat", "op")).unwrap().nlink(), 1);
        // A crash after rename needs no cleanup: exact replay uses the canonical
        // record unchanged, even with unrelated abandoned temporaries present.
        let retry = revocation::install("seat", &binding("op"), &intent(), &identity).unwrap();
        assert!(retry.already_installed);
        assert_eq!(retry.record, first.record);
        assert_eq!(revocation::root_id().unwrap(), identity);
        assert_eq!(revocation::check("seat", Some(&binding("op")), Some("op")).unwrap_err().to_string(), "operation-revoked");
    });
}

#[test]
fn device_change_requires_explicit_reenrollment_without_resetting_fences() {
    let root = Root::new();
    root.run(|| {
        let installed = install("seat", &binding("op"), &intent()).unwrap();
        let path = root.record("seat", "op");
        let fence = fs::read(&path).unwrap();
        let identity_path = root.0.join(".publication-revocations/v1/root.json");
        let mut identity: serde_json::Value = serde_json::from_slice(&fs::read(&identity_path).unwrap()).unwrap();
        let device = fs::metadata(&root.0).unwrap().dev();
        identity["device"] = serde_json::json!(device.wrapping_add(1));
        private_write(&identity_path, &serde_json::to_vec(&identity).unwrap());
        let error = revocation::root_id().unwrap_err();
        assert!(error.to_string().contains("re-enrollment"));
        assert!(revocation::check("seat", Some(&binding("op")), Some("op")).is_err());
        assert!(install("seat", &binding("other"), &intent()).is_err());
        assert_eq!(fs::read(&path).unwrap(), fence);
        // Simulate the documented offline operator procedure: verify this is
        // the same root and change only its device observation, not its ID.
        identity["device"] = serde_json::json!(device);
        private_write(&identity_path, &serde_json::to_vec(&identity).unwrap());
        assert_eq!(revocation::root_id().unwrap(), installed.record.root_id);
        assert_eq!(fs::read(&path).unwrap(), fence);
        assert_eq!(revocation::read("seat", "op").unwrap(), Some(installed.record));
        assert_eq!(revocation::check("seat", Some(&binding("op")), Some("op")).unwrap_err().to_string(), "operation-revoked");
    });
}
