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

/// Retire a stale observation only if the complete record and sidecar still
/// agree under event + creation locks. Exit metadata is historical evidence,
/// not a reason to retain a boot-stale generation and its companions forever.
pub fn retire_stale_daemon_record(name: &str, observed: &super::metadata::SessionMetadata) -> bool {
    if super::names::validate_name(name).is_err() {
        return false;
    }
    let Some((current, observed_pid)) =
        super::list::read_identity_record(&super::root::session_dir(), name)
    else {
        return false;
    };
    if current != *observed {
        return false;
    }
    retire_stale_daemon_observation(name, observed, observed_pid)
}

fn current_boot_started_at_ms() -> Option<i64> {
    // Cache only successful observations; unavailable boot evidence must stay
    // retryable rather than permanently authorizing or preventing retirement.
    static BOOT_STARTED: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    if let Some(started) = BOOT_STARTED.get() {
        return Some(*started);
    }
    let started = read_boot_started_at_ms()?;
    let _ = BOOT_STARTED.set(started);
    BOOT_STARTED.get().copied()
}

#[cfg(target_os = "linux")]
fn read_boot_started_at_ms() -> Option<i64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let seconds = stat
        .lines()
        .find_map(|line| line.strip_prefix("btime ")?.trim().parse::<i64>().ok())?;
    seconds.checked_mul(1000)
}

#[cfg(target_os = "macos")]
fn read_boot_started_at_ms() -> Option<i64> {
    let mut oid = [libc::CTL_KERN, libc::KERN_BOOTTIME];
    let mut boot = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut size = std::mem::size_of_val(&boot);
    // SAFETY: oid and boot are valid buffers with the indicated lengths. This
    // read-only sysctl has no new-value buffer and mutates only our output.
    let result = unsafe {
        libc::sysctl(
            oid.as_mut_ptr(),
            oid.len() as libc::c_uint,
            (&mut boot as *mut libc::timeval).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 || size != std::mem::size_of_val(&boot) {
        return None;
    }
    boot.tv_sec.checked_mul(1000)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_boot_started_at_ms() -> Option<i64> {
    None
}

fn record_predates_current_boot(metadata: &super::metadata::SessionMetadata) -> bool {
    if let Some(recorded) = &metadata.daemon_boot_id {
        return super::list::current_boot_id().is_some_and(|current| current != recorded.as_str());
    }
    match (
        super::time::parse_iso8601_ms(&metadata.created_at),
        current_boot_started_at_ms(),
    ) {
        (Some(created), Some(boot_started)) => created < boot_started,
        _ => false,
    }
}

fn retire_stale_daemon_observation(
    name: &str,
    observed: &super::metadata::SessionMetadata,
    observed_pid: Option<i32>,
) -> bool {
    use super::list::{DaemonIdentityCheck, check_record_identity, read_identity_record};
    with_both_locks(name, || {
        let Some((current, current_pid)) = read_identity_record(&super::root::session_dir(), name) else {
            return false;
        };
        if current != *observed || current_pid != observed_pid {
            return false;
        }
        // Startup retirement is not GC policy: preserve retained history and
        // permanent launch recipes even when their old runtime binding is gone.
        if super::tags::is_keep_requested(current.tags.as_ref())
            || current.tags.as_ref().and_then(|tags| tags.get("strategy"))
                .is_some_and(|strategy| strategy == "permanent")
        {
            return false;
        }
        let Some(pid) = observed_pid.or(current.daemon_pid) else {
            return false;
        };
        let identity = check_record_identity(observed_pid, Some(&current));
        match identity {
            DaemonIdentityCheck::Mismatch => {}
            DaemonIdentityCheck::Gone if record_predates_current_boot(&current) => {}
            _ => return false,
        }
        // Keep metadata until every companion is removed, so an I/O failure
        // leaves a generation-fenced record available for the next startup.
        let removed = [
            socket_path(name), pid_path(name), events_path(name),
            recovery_revision_path(name), output_activity_path(name), metadata_path(name),
        ].into_iter().try_for_each(|path| match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        });
        if let Err(error) = removed {
            eprintln!(
                "event=registry_retirement session={name:?} generation={:?} pid={pid} identity={} liveness=not-live action=incomplete error={error:?}",
                current.generation, identity.as_str(),
            );
            return false;
        }
        eprintln!(
            "event=registry_retirement session={name:?} generation={:?} pid={pid} identity={} liveness=not-live action=retired",
            current.generation, identity.as_str(),
        );
        true
    }).unwrap_or(false)
}

/// Best-effort signal-free startup retirement of proven stale boot bindings,
/// including exited records, but never keep/permanent history or current-boot
/// Gone records. The starting daemon's unpublished name is excluded.
pub fn retire_stale_daemon_records(starting_name: &str) -> usize {
    let Ok(entries) = std::fs::read_dir(super::root::session_dir()) else {
        return 0;
    };
    let mut retired = 0;
    for entry in entries.flatten() {
        let Ok(file_name) = entry.file_name().into_string() else {
            continue;
        };
        if super::atomic::is_tmp_name(&file_name) {
            continue;
        }
        let Some(name) = file_name.strip_suffix(".json") else {
            continue;
        };
        if name == starting_name || super::names::validate_name(name).is_err() {
            continue;
        }
        if let Some((observed, observed_pid)) =
            super::list::read_identity_record(&super::root::session_dir(), name)
            && retire_stale_daemon_observation(name, &observed, observed_pid)
        {
            retired += 1;
        }
    }
    retired
}
