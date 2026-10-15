//! CLI half of tests/list-purity.test.ts: `pty list` is observational (it
//! creates nothing and deletes nothing), `pty gc --dry-run` is non-mutating,
//! and `pty gc` owns cleanup of stale sessions and raw debris, guarded by a
//! live creation lock or a reachable socket. The
//! `inventoryRawCleanupCandidates` / `cleanupRawCandidateGuarded` revalidation
//! case is library-only and stays in Node.

use pty_conformance::*;
use std::os::unix::net::UnixListener;
use std::path::Path;

fn write_stale_session(root: &Path, name: &str) {
    std::fs::write(root.join(format!("{name}.sock")), "").unwrap();
    std::fs::write(root.join(format!("{name}.pid")), "2147483647").unwrap();
    let mut meta = FakeMeta::created(0);
    meta.command = Some("true".into());
    write_fake_metadata(root, name, meta);
}

fn write_raw_debris(root: &Path, name: &str, corrupt_metadata: bool) {
    std::fs::write(root.join(format!("{name}.sock")), "").unwrap();
    std::fs::write(root.join(format!("{name}.pid")), "2147483647").unwrap();
    if corrupt_metadata {
        std::fs::write(root.join(format!("{name}.json")), "{").unwrap();
    }
}

fn exists(root: &Path, file: &str) -> bool {
    root.join(file).exists()
}

/// node: tests/list-purity.test.ts:49
#[test]
fn list_does_not_create_an_absent_registry() {
    let rig = Rig::new();
    let root = rig.tmp().join("absent");
    let out = rig.pty_env(&[("PTY_ROOT", &root.to_string_lossy())], &["list", "--json"]);
    assert_eq!(expect_json(&out), serde_json::json!([]));
    assert!(!root.exists(), "list created the registry");
}

/// node: tests/list-purity.test.ts:57
#[test]
fn list_reports_a_stale_socket_session_without_deleting_it() {
    let rig = Rig::new();
    write_stale_session(rig.root(), "stale");
    let sessions = rig.list_json();
    let shape: Vec<(String, String)> = sessions
        .iter()
        .map(|s| (s["name"].as_str().unwrap().into(), s["status"].as_str().unwrap().into()))
        .collect();
    assert_eq!(shape, vec![("stale".to_string(), "vanished".to_string())]);
    assert!(exists(rig.root(), "stale.sock"));
    assert!(exists(rig.root(), "stale.pid"));
}

/// node: tests/list-purity.test.ts:67
#[test]
fn list_does_not_delete_corrupt_metadata() {
    let rig = Rig::new();
    std::fs::write(rig.root().join("corrupt.json"), "{").unwrap();
    assert_eq!(rig.list_json(), Vec::<serde_json::Value>::new());
    assert!(exists(rig.root(), "corrupt.json"));
}

/// node: tests/list-purity.test.ts:74
#[test]
fn gc_dry_run_is_non_mutating_and_gc_cleans_up() {
    let rig = Rig::new();
    write_stale_session(rig.root(), "dry");
    let preview = rig.pty(&["gc", "--dry-run"]);
    expect_status(&preview, 0);
    expect_contains(&preview.stdout(), "Would remove: dry");
    assert!(exists(rig.root(), "dry.sock"));
    assert!(exists(rig.root(), "dry.pid"));
    assert!(exists(rig.root(), "dry.json"));
    let applied = rig.pty(&["gc"]);
    expect_status(&applied, 0);
    expect_contains(&applied.stdout(), "Removed: dry");
    assert!(!exists(rig.root(), "dry.sock"));
    assert!(!exists(rig.root(), "dry.pid"));
    assert!(!exists(rig.root(), "dry.json"));
}

/// node: tests/list-purity.test.ts:89
#[test]
fn gc_previews_and_applies_guarded_cleanup_of_raw_debris() {
    for corrupt_metadata in [true, false] {
        let rig = Rig::new();
        write_raw_debris(rig.root(), "debris", corrupt_metadata);
        let listed = rig.list_json();
        if is_rust() {
            // A raw runtime binding is visible, but is not cleanup authority
            // when its metadata is absent or unreadable.
            assert_eq!(listed.len(), 1, "corrupt={corrupt_metadata}");
            assert_eq!(listed[0]["name"], "debris");
            assert_eq!(listed[0]["status"], "vanished");
        } else {
            assert_eq!(listed, Vec::<serde_json::Value>::new(), "corrupt={corrupt_metadata}");
        }
        let preview = rig.pty(&["gc", "--dry-run"]);
        expect_status(&preview, 0);
        if is_rust() {
            expect_not_contains(&preview.stdout(), "Would remove: debris");
        } else {
            expect_contains(&preview.stdout(), "Would remove: debris");
        }
        assert!(exists(rig.root(), "debris.sock"));
        assert!(exists(rig.root(), "debris.pid"));
        assert_eq!(exists(rig.root(), "debris.json"), corrupt_metadata);
        let applied = rig.pty(&["gc"]);
        expect_status(&applied, 0);
        if is_rust() {
            expect_not_contains(&applied.stdout(), "Removed: debris");
            assert!(exists(rig.root(), "debris.sock"));
            assert!(exists(rig.root(), "debris.pid"));
            assert_eq!(exists(rig.root(), "debris.json"), corrupt_metadata);
        } else {
            expect_contains(&applied.stdout(), "Removed: debris");
            assert!(!exists(rig.root(), "debris.sock"));
            assert!(!exists(rig.root(), "debris.pid"));
            assert!(!exists(rig.root(), "debris.json"));
        }
    }
}

/// node: tests/list-purity.test.ts:113
#[test]
fn gc_does_not_clean_raw_debris_while_another_creator_owns_the_name() {
    let rig = Rig::new();
    write_raw_debris(rig.root(), "locked", true);
    std::fs::write(rig.root().join("locked.lock"), std::process::id().to_string()).unwrap();
    let applied = rig.pty(&["gc"]);
    expect_status(&applied, 0);
    expect_not_contains(&applied.stdout(), "Removed: locked");
    assert!(exists(rig.root(), "locked.sock"));
    assert!(exists(rig.root(), "locked.pid"));
    assert!(exists(rig.root(), "locked.json"));
}

/// node: tests/list-purity.test.ts:148
#[test]
fn gc_preserves_dead_pid_debris_when_its_socket_is_reachable() {
    let rig = Rig::new();
    std::fs::write(rig.root().join("reachable.pid"), "2147483647").unwrap();
    let socket_path = rig.socket_path("reachable");
    let listener = UnixListener::bind(&socket_path).expect("bind reachable.sock");
    listener.set_nonblocking(true).unwrap();
    let preview = rig.pty(&["gc", "--dry-run"]);
    let applied = rig.pty(&["gc"]);
    drop(listener);
    expect_status(&preview, 0);
    expect_status(&applied, 0);
    expect_not_contains(&preview.stdout(), "Would remove: reachable");
    expect_not_contains(&applied.stdout(), "Removed: reachable");
    assert!(socket_path.exists());
    assert!(exists(rig.root(), "reachable.pid"));
}
