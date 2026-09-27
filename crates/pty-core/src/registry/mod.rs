//! Session registry: the on-disk layout under `$PTY_ROOT`, byte-compatible
//! with the Node project's `src/sessions.ts` so Rust and Node daemons and
//! CLIs can share one root.
//!
//! Per session: `<name>.json` (metadata), `<name>.sock`, `<name>.pid`,
//! `<name>.events.jsonl`, `<name>.lock`, `<name>.events.lock`,
//! `.recovery/<name>.revision.json`, and the Rust daemon's output-activity
//! sidecar `.activity/<name>.json` ([`activity`]). Atomic writes go through
//! `<path>.tmp.<pid>.<16 hex>` + rename; readers skip names containing
//! `.tmp.`.
//!
//! Rust publishes complete lock owner records atomically and serializes stale
//! stealing between Rust contenders. Node retains an empty-publication window
//! and an unbound stale read-then-unlink that can defeat a newer Rust claim;
//! see [`lock`] for the mixed-registry boundary.

pub mod activity;
pub mod atomic;
pub mod cleanup;
pub mod evidence;
pub mod list;
pub mod lock;
pub mod metadata;
pub mod mutate;
pub mod names;
pub mod root;
pub mod tags;
pub mod time;

pub use activity::{
    OutputActivity, before_output_activity_retire_on_this_thread, last_output_at_ms,
    last_output_at_ms_in, newest_output_at_ms, publish_output_activity, read_output_activity,
    read_output_activity_in, record_exit_retiring_output_activity,
};
pub use atomic::{atomic_write, is_tmp_name, random_hex16};
pub use cleanup::{
    SessionGenerationOwner, cleanup, cleanup_all, cleanup_all_while_locked, cleanup_owned_all,
    cleanup_owned_socket, cleanup_socket, is_current_generation_owner,
};
pub use evidence::{
    EvidenceStream, EvidenceUnavailableReason, RemoveSessionGenerationResult, SessionExitEvidence,
    SessionExitEvidenceResult, SessionExitEvidenceTail, SessionExitStatus,
    get_session_exit_evidence, remove_session_generation,
};
pub use list::{
    DEFAULT_SOCKET_PROBE_BUDGET, ListOptions, SessionInfo, SessionStatus, all_session_names,
    ambiguous_reference_message, get_session, get_session_by_name, has_process_exited_for_reap,
    list_sessions, list_sessions_in, list_sessions_with, pid_alive, probe_sockets_within_budget,
    read_pid, read_pid_with, read_process_start_token, read_session_pid, read_signal_target_with,
    resolve_ref, session_exists, socket_reachable, wait_for_process_exit,
};
pub use lock::{
    EVENT_LOCK_WAIT, LockBusy, LockGuard, LockRefusal, METADATA_PATCH_LOCK_WAIT,
    acquire_event_lock, acquire_file_lock, acquire_lock, event_busy_message, is_creation_lock_held,
    is_lock_owned_by_pid, lock_or_refusal, metadata_busy_message, release_event_lock,
    release_file_lock, release_lock, take_event_lock, take_metadata_lock, try_acquire_file_lock,
    wait_for_event_lock, wait_for_metadata_lock, with_both_locks,
};
pub use metadata::{
    EnvMap, SESSION_EXIT_LAST_LINES_LIMIT, SessionMetadata, TagMap, apply_metadata_diff,
    pretty_json, read_metadata, read_metadata_map, write_metadata, write_metadata_map,
    write_metadata_publication,
};
pub use mutate::{
    MetadataChangeSnapshot, MetadataPatch, MetadataPatchEvent, MetadataPatchResult, MutateOptions,
    MutateStatus, TagCompareAndSetRequest, TagCompareAndSetResult, apply_metadata_patch_by_id,
    compare_and_set_tag_value, metadata_matches_observation, mutate_metadata_under_lock,
    mutate_metadata_under_lock_with, mutate_metadata_under_lock_with_wait, patch_metadata_by_id,
    set_display_name, update_tags,
};
pub use names::{
    SESSION_ID_ALPHABET, SESSION_ID_ATTEMPTS, auto_display_name, generate_id, random_session_name,
    sanitize_display_name, short_path, time_ago, time_ago_from_seconds, unique_id_failure_message,
    validate_display_name, validate_name,
};
pub use root::{
    SUN_PATH_MAX, default_session_dir, ensure_session_dir, event_lock_path, events_path, lock_path,
    metadata_path, output_activity_path, pid_path, recovery_revision_path, root_length_check,
    session_dir, socket_path,
};
pub use tags::{
    DEFAULT_KEEP_MAX_AGE_MS, EXACT_RESERVED_TAG_KEYS, GC_BOOKKEEPING_KEYS, KEEP_FALSEY, KEEP_TAG,
    extract_filter_tags, is_keep_expired, is_keep_requested, is_reserved_tag_key, matches_all_tags,
    reap_on_exit_default, should_reap_at_exit, strip_gc_bookkeeping,
};
pub use time::{
    iso8601, iso8601_from_epoch_ms, local_hms, now_epoch_ms, now_iso8601, parse_iso8601_ms,
};

/// Write a session's pid file (decimal, no newline) the way Node's daemon
/// does, after the socket is listening. Node's write is a plain
/// `writeFileSync`; the atomic temp+rename here is defence-in-depth so a
/// concurrent reader never catches an empty file.
///
/// node: src/server.ts:654
pub fn write_pid(name: &str, pid: u32) -> std::io::Result<()> {
    ensure_session_dir()?;
    atomic_write(&pid_path(name), pid.to_string().as_bytes())
}
