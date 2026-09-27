//! `.activity/<name>.json`: where a running Rust daemon keeps
//! `lastOutputAtMs`, so that `<name>.json` changes only when a lifecycle,
//! tag or client fact changes (docs/decisions/0015).
//!
//! The daemon is the only writer. It publishes the newest stamp at most once
//! a second while output flows, through the same temp-and-rename as every
//! other registry file, and it folds the final stamp into the exit record,
//! after which the sidecar is removed. The sidecar names the generation that
//! wrote it, so a reader never credits one daemon's output to the next
//! daemon that reuses the id. Every write and the exit-time removal run under
//! `<name>.lock` while the record still names the writer's generation, so a
//! daemon whose id was reused never clobbers or deletes its replacement's
//! sidecar.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::atomic::atomic_write;
use super::metadata::SessionMetadata;
use super::mutate::{MutateOptions, MutateStatus, mutate_metadata_under_lock_settled};
use super::root::{output_activity_path, session_dir};

/// The sidecar's content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputActivity {
    /// The daemon generation that observed the output.
    pub generation: String,
    /// Unix milliseconds for the newest child output that daemon has seen.
    pub last_output_at_ms: i64,
}

/// Publish `activity` for `name` while its generation still owns the name:
/// under `<name>.lock`, and only when the record names
/// `activity.generation`. A daemon whose id was reused must not overwrite
/// the replacement's sidecar. `Unchanged` means written (or a best-effort
/// write failed); `Busy` means the lock was held and the caller may retry;
/// anything else means this generation no longer owns the name.
pub fn publish_output_activity(name: &str, activity: &OutputActivity) -> MutateStatus {
    mutate_metadata_under_lock_settled(
        name,
        |_| false,
        &MutateOptions {
            expected_generation: Some(activity.generation.clone()),
            expected_metadata: None,
        },
        |_| {
            let _ = write_output_activity(name, activity);
        },
    )
}

fn write_output_activity(name: &str, activity: &OutputActivity) -> std::io::Result<()> {
    let path = output_activity_path(name);
    if let Some(dir) = path.parent()
        && !dir.is_dir()
    {
        std::fs::create_dir_all(dir)?;
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let bytes = serde_json::to_vec(activity).map_err(std::io::Error::other)?;
    atomic_write(&path, &bytes)
}

/// Record a daemon's exit through `mutate` (fenced on `generation`) and
/// retire the sidecar in the same critical section: the exit record now
/// carries the final stamp, and the unlink runs only while `generation`
/// still owns the name, so it can never remove a replacement's sidecar.
pub fn record_exit_retiring_output_activity(
    name: &str,
    generation: &str,
    mutate: impl FnOnce(&mut SessionMetadata) -> bool,
) -> MutateStatus {
    mutate_metadata_under_lock_settled(
        name,
        mutate,
        &MutateOptions {
            expected_generation: Some(generation.to_string()),
            expected_metadata: None,
        },
        |_| retire_output_activity(name),
    )
}

fn retire_output_activity(name: &str) {
    RETIRE_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
    let _ = std::fs::remove_file(output_activity_path(name));
}

thread_local! {
    static RETIRE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `hook` once, on the calling thread, immediately before the next
/// sidecar unlink by [`record_exit_retiring_output_activity`]. Test-only
/// seam for interleaving a replacement's publication; not a stable API.
#[doc(hidden)]
pub fn before_output_activity_retire_on_this_thread(hook: Box<dyn FnOnce()>) {
    RETIRE_HOOK.with(|slot| *slot.borrow_mut() = Some(hook));
}

/// Read the sidecar in the current session directory, `None` when it is
/// missing or unreadable.
pub fn read_output_activity(name: &str) -> Option<OutputActivity> {
    read_output_activity_in(&session_dir(), name)
}

/// Read a sidecar from an explicit registry root (as with `list_sessions_in`).
pub fn read_output_activity_in(root: &Path, name: &str) -> Option<OutputActivity> {
    let raw = std::fs::read(root.join(".activity").join(format!("{name}.json"))).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// The newest output stamp a reader should believe for `name`, given the
/// record it already read. See [`last_output_at_ms_in`] for an explicit root.
pub fn last_output_at_ms(name: &str, metadata: &SessionMetadata) -> Option<i64> {
    last_output_at_ms_in(&session_dir(), name, metadata)
}

/// Read the stamp from `root` without changing the root's environment.
pub fn last_output_at_ms_in(root: &Path, name: &str, metadata: &SessionMetadata) -> Option<i64> {
    let sidecar = if metadata.has_exited() {
        None
    } else {
        read_output_activity_in(root, name)
    };
    newest_output_at_ms(metadata, sidecar.as_ref())
}

/// A recorded exit carries the final stamp, so an exited record answers for
/// itself. A running record defers to a sidecar written by its own
/// generation, and otherwise keeps the stamp it carries: a Node daemon, and
/// a Rust daemon from before decision 0015, write the field into the record.
pub fn newest_output_at_ms(
    metadata: &SessionMetadata,
    sidecar: Option<&OutputActivity>,
) -> Option<i64> {
    if !metadata.has_exited()
        && let Some(activity) = sidecar
        && metadata.generation.as_deref() == Some(activity.generation.as_str())
    {
        return Some(activity.last_output_at_ms);
    }
    metadata.last_output_at_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(generation: &str, stamp: Option<i64>, exited: bool) -> SessionMetadata {
        SessionMetadata {
            generation: Some(generation.into()),
            last_output_at_ms: stamp,
            exited_at: exited.then(|| "2026-09-26T12:00:00.000Z".into()),
            ..SessionMetadata::default()
        }
    }

    fn sidecar(generation: &str, stamp: i64) -> OutputActivity {
        OutputActivity {
            generation: generation.into(),
            last_output_at_ms: stamp,
        }
    }

    #[test]
    fn a_running_record_takes_its_own_generations_sidecar() {
        let meta = record("g1", None, false);
        assert_eq!(
            newest_output_at_ms(&meta, Some(&sidecar("g1", 42))),
            Some(42)
        );
    }

    #[test]
    fn another_generations_sidecar_is_not_credited() {
        // A replacement daemon under the same id: the old daemon's last
        // write must not read as the new one's output.
        let meta = record("g2", None, false);
        assert_eq!(newest_output_at_ms(&meta, Some(&sidecar("g1", 42))), None);
        let node = record("g2", Some(7), false);
        assert_eq!(
            newest_output_at_ms(&node, Some(&sidecar("g1", 42))),
            Some(7)
        );
    }

    #[test]
    fn an_exited_record_answers_for_itself() {
        let meta = record("g1", Some(10), true);
        assert_eq!(
            newest_output_at_ms(&meta, Some(&sidecar("g1", 42))),
            Some(10)
        );
    }

    #[test]
    fn the_sidecar_round_trips_as_camel_case() {
        let json = serde_json::to_value(sidecar("g1", 42)).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"generation": "g1", "lastOutputAtMs": 42})
        );
    }
}
