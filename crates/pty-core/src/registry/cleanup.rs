//! Removing a session's files: socket-only cleanup, full cleanup under both
//! locks, and the generation-checked variants a daemon uses to reap itself
//! without touching a replacement that is starting up.
//!
//! node: src/sessions.ts:754-767, 2178-2266

use super::list::read_session_pid;
use super::lock::{LockBusy, acquire_lock, with_both_locks};
use super::metadata::read_metadata;
use super::root::{
    events_path, metadata_path, output_activity_path, pid_path, recovery_revision_path, socket_path,
};

fn unlink(path: std::path::PathBuf) {
    let _ = std::fs::remove_file(path);
}

/// Remove `<name>.sock` and `<name>.pid`, keeping the metadata.
///
/// node: src/sessions.ts:2178-2186
pub fn cleanup_socket(name: &str) {
    unlink(socket_path(name));
    unlink(pid_path(name));
}

/// Remove every artifact while the caller already owns both locks:
/// socket, pid, metadata, events, recovery revision, output-activity sidecar.
///
/// node: src/sessions.ts:755-767 (Node has no activity sidecar; decision 0015)
pub fn cleanup_all_while_locked(name: &str) {
    cleanup_socket(name);
    unlink(metadata_path(name));
    unlink(events_path(name));
    unlink(recovery_revision_path(name));
    unlink(output_activity_path(name));
}

/// Remove everything including metadata, under the event lock then the
/// creation lock. A live holder of either refuses and nothing is touched.
///
/// node: src/sessions.ts:2188-2202
pub fn cleanup_all(name: &str) -> Result<(), LockBusy> {
    with_both_locks(name, || cleanup_all_while_locked(name))
}

/// The identity a daemon proves before removing files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionGenerationOwner {
    pub generation: String,
    pub pid: i32,
}

/// Does the registry entry still belong to `owner`? Metadata generation is
/// primary; the pidfile closes the window before metadata is published;
/// legacy records without a generation fall back to pid ownership. Call
/// only while holding the creation lock.
///
/// node: src/sessions.ts:2214-2224
pub fn is_current_generation_owner(name: &str, owner: &SessionGenerationOwner) -> bool {
    if let Some(metadata) = read_metadata(name)
        && let Some(generation) = &metadata.generation
        && generation != &owner.generation
    {
        return false;
    }
    if let Some(pid) = read_session_pid(name)
        && pid != owner.pid
    {
        return false;
    }
    true
}

/// Generation-safe socket/pid cleanup: skipped (`false`) when the lock is
/// held or a replacement now owns the name.
///
/// node: src/sessions.ts:2231-2241
pub fn cleanup_owned_socket(name: &str, owner: &SessionGenerationOwner) -> bool {
    let Some(_lock) = acquire_lock(name) else {
        return false;
    };
    if !is_current_generation_owner(name, owner) {
        return false;
    }
    cleanup_socket(name);
    true
}

/// Generation-safe full cleanup used by a daemon reaping its own session:
/// socket, pid, metadata, events, recovery revision, under both locks.
///
/// node: src/sessions.ts:2243-2266
pub fn cleanup_owned_all(name: &str, owner: &SessionGenerationOwner) -> bool {
    with_both_locks(name, || {
        if !is_current_generation_owner(name, owner) {
            return false;
        }
        cleanup_all_while_locked(name);
        true
    })
    .unwrap_or(false)
}

/// Best-effort removal of every on-disk file of a session with no locking
/// and no ownership check. Kept for existing callers; new code should use
/// [`cleanup_all`] or [`cleanup_owned_all`].
pub fn cleanup(name: &str) {
    cleanup_all_while_locked(name);
}
