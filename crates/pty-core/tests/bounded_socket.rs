use std::os::unix::net::UnixListener;
use std::path::PathBuf;

use pty_core::registry::{
    ListOptions, SessionMetadata, cleanup_socket, list_sessions_strict_in,
    observe_session_strict_in, read_boot_id, read_process_start_token, socket_path_in, with_root,
};

/// Whether this machine cannot read a process start token, said aloud: the
/// darwin build sandbox has no `ps`, which a Mac reads it from. Linux reads
/// `/proc` and never skips.
fn skip_without_start_token() -> bool {
    if cfg!(target_os = "linux") || read_process_start_token(std::process::id() as i32).is_some() {
        return false;
    }
    eprintln!("skipped: no process start token here (`/bin/ps -o lstart=` cannot answer)");
    true
}

struct Root(PathBuf);

impl Root {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let seed = format!(
            "/tmp/pty-bounded-{}-{}-{}",
            std::process::id(),
            sequence,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = PathBuf::from(format!("{seed}{}", "r".repeat(63 - seed.len())));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn bounded_address_preserves_logical_id_in_inventory_lookup_and_cleanup() {
    if skip_without_start_token() {
        return;
    }
    let root = Root::new();
    let id = "n".repeat(63);
    let socket = socket_path_in(&root.0, &id);
    let listener = UnixListener::bind(&socket).unwrap();
    let metadata = SessionMetadata {
        daemon_pid: Some(std::process::id() as i32),
        daemon_start_token: read_process_start_token(std::process::id() as i32),
        daemon_boot_id: read_boot_id(),
        ..Default::default()
    };
    std::fs::write(
        root.0.join(format!("{id}.json")),
        serde_json::to_vec(&metadata).unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.0.join(format!("{id}.pid")),
        std::process::id().to_string(),
    )
    .unwrap();

    let inventory = list_sessions_strict_in(&root.0, &ListOptions::default());
    assert!(inventory.complete, "{:?}", inventory.errors);
    assert_eq!(inventory.entries.len(), 1);
    assert_eq!(inventory.entries[0].name, id);
    assert_eq!(inventory.entries[0].socket_path, socket);
    assert_eq!(
        observe_session_strict_in(&root.0, &id, &ListOptions::default()).entries,
        inventory.entries,
    );

    with_root(&root.0, || cleanup_socket(&id));
    assert!(!socket.exists());
    assert!(!root.0.join(format!("{id}.pid")).exists());
    assert!(root.0.join(format!("{id}.json")).exists());
    drop(listener);
}

#[test]
fn an_unmapped_bounded_socket_is_an_incomplete_observation_not_a_session_id() {
    let root = Root::new();
    let socket = socket_path_in(&root.0, &"n".repeat(63));
    let _listener = UnixListener::bind(&socket).unwrap();
    let inventory = list_sessions_strict_in(&root.0, &ListOptions::default());
    assert!(!inventory.complete);
    assert!(inventory.entries.is_empty());
    assert_eq!(inventory.errors.len(), 1);
    assert_eq!(inventory.errors[0].path, socket);
}

#[test]
fn noncanonical_socket_debris_does_not_alias_the_live_session() {
    if skip_without_start_token() {
        return;
    }
    let root = Root::new();
    let id = "d".repeat(63);
    let socket = socket_path_in(&root.0, &id);
    let _listener = UnixListener::bind(&socket).unwrap();
    let metadata = SessionMetadata {
        daemon_pid: Some(std::process::id() as i32),
        daemon_start_token: read_process_start_token(std::process::id() as i32),
        daemon_boot_id: read_boot_id(),
        ..Default::default()
    };
    std::fs::write(
        root.0.join(format!("{id}.json")),
        serde_json::to_vec(&metadata).unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.0.join(format!("{id}.pid")),
        std::process::id().to_string(),
    )
    .unwrap();
    let debris = root.0.join(format!("{id}.sock"));
    std::fs::write(&debris, b"stale debris").unwrap();
    let inventory = list_sessions_strict_in(&root.0, &ListOptions::default());
    assert!(!inventory.complete);
    assert_eq!(inventory.errors.len(), 1);
    assert_eq!(inventory.errors[0].path, debris);
    assert_eq!(inventory.entries.len(), 1);
    assert_eq!(inventory.entries[0].name, id);
    assert_eq!(inventory.entries[0].socket_path, socket);
    assert_eq!(
        pty_core::registry::list_sessions_in(&root.0, &ListOptions::default()).len(),
        1
    );
}

#[test]
fn reserved_digest_companions_cannot_claim_a_socket_as_a_logical_id() {
    let root = Root::new();
    let socket = socket_path_in(&root.0, &"x".repeat(63));
    let _listener = UnixListener::bind(&socket).unwrap();
    let digest_id = socket.file_stem().unwrap().to_str().unwrap();
    std::fs::write(root.0.join(format!("{digest_id}.json")), b"{}").unwrap();
    let inventory = list_sessions_strict_in(&root.0, &ListOptions::default());
    assert!(!inventory.complete);
    assert!(inventory.entries.is_empty());
    assert!(
        inventory
            .errors
            .iter()
            .any(|error| error.path == root.0.join(format!("{digest_id}.json")))
    );
}
