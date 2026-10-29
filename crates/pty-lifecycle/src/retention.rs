//! Opt-in retention for finished registry records (`PTY_RETAIN_FINISHED`).
//!
//! A daemon started with a window sweeps its root once, off its serving
//! loop, and removes `exited` and `vanished` records that have been dead for
//! longer than the window. It removes only what `pty gc`'s sweep would
//! remove ([`removable_finished`]), decides again under both locks from the
//! record as it is then, and never signals, respawns or abandons anything.
//! Retention is off unless the variable sets a window.

use std::sync::Mutex;

use pty_core::registry::{
    self, DEFAULT_KEEP_MAX_AGE_MS, RETENTION_LOST_AT_TAG, SessionInfo, SessionMetadata, TagMap,
    apply_metadata_diff, cleanup_all_while_locked, is_keep_requested, iso8601_from_epoch_ms,
    metadata_matches_observation, now_epoch_ms, parse_iso8601_ms, read_metadata_map,
    with_both_locks, write_metadata_map,
};

use crate::gc::{daemon_identity, is_permanent, known_owner_lost, removable_finished};

/// At most this many records are stamped or removed by one sweep, so a
/// large backlog is worked down over several daemon starts.
const MAX_ACTIONS_PER_SWEEP: usize = 256;

/// Held around each record's locked check and writes. `true` once
/// [`stop_retention`] has run: a daemon that exits mid-sweep waits for the
/// record in hand and never leaves one half removed.
static SWEEP_GATE: Mutex<bool> = Mutex::new(false);

/// What one sweep did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RetentionResult {
    /// Records removed because they had been dead longer than the window.
    pub removed: Vec<String>,
    /// Vanished records given their first [`RETENTION_LOST_AT_TAG`].
    pub stamped: Vec<String>,
}

/// Start one sweep of the caller's registry root on its own thread, when
/// `PTY_RETAIN_FINISHED` sets a window. Returns at once.
pub fn spawn_retention_sweep() {
    let Some(window_ms) = registry::retain_finished_window() else {
        return;
    };
    let root = registry::session_dir();
    let _ = std::thread::Builder::new()
        .name("pty-retention".into())
        .spawn(move || {
            registry::with_root(&root, || sweep_finished(window_ms, DEFAULT_KEEP_MAX_AGE_MS));
        });
}

/// Stop every retention sweep in this process. Waits for the record a
/// sweep is working on; no record is touched after it returns.
pub fn stop_retention() {
    *SWEEP_GATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
}

/// One bounded pass over the current root. A `keep` tag holds a record for
/// the longer of `window_ms` and `keep_max_age_ms`; retention never
/// shortens it.
pub fn sweep_finished(window_ms: i64, keep_max_age_ms: i64) -> RetentionResult {
    sweep_finished_gated(window_ms, keep_max_age_ms, &SWEEP_GATE)
}

fn sweep_finished_gated(
    window_ms: i64,
    keep_max_age_ms: i64,
    gate: &Mutex<bool>,
) -> RetentionResult {
    let mut result = RetentionResult::default();
    if window_ms <= 0 {
        return result;
    }
    let now_ms = now_epoch_ms();
    for session in registry::list_sessions_full() {
        if result.removed.len() + result.stamped.len() >= MAX_ACTIONS_PER_SWEEP {
            break;
        }
        let Some(metadata) = removable_finished(&session) else {
            continue;
        };
        let Some(action) = decide(metadata, now_ms, window_ms, keep_max_age_ms) else {
            continue;
        };
        match gated(gate, || act(&session, action, now_ms, window_ms, keep_max_age_ms)) {
            None => break,
            Some(true) if action == Action::Stamp => result.stamped.push(session.name.clone()),
            Some(true) => result.removed.push(session.name.clone()),
            Some(false) => {}
        }
    }
    result
}

/// What retention does to one finished record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// A vanished record's first sighting.
    Stamp,
    /// Dead for longer than its window.
    Remove,
}

/// The decision from one reading of a finished record. Owner loss and
/// `strategy=permanent` are the callers' tests: [`removable_finished`] on
/// the scan, [`act`] again under the locks.
fn decide(
    metadata: &SessionMetadata,
    now_ms: i64,
    window_ms: i64,
    keep_max_age_ms: i64,
) -> Option<Action> {
    let window = if is_keep_requested(metadata.tags.as_ref()) {
        window_ms.max(keep_max_age_ms)
    } else {
        window_ms
    };
    match loss_anchor(metadata) {
        Anchor::Unstamped => Some(Action::Stamp),
        Anchor::At(lost_at) if now_ms - lost_at >= window => Some(Action::Remove),
        Anchor::At(_) | Anchor::Unaged => None,
    }
}

/// When a finished record's owner was lost, as far as pty knows.
#[derive(Debug, PartialEq, Eq)]
enum Anchor {
    At(i64),
    /// Vanished, with no stamp from this run of the id yet.
    Unstamped,
    /// No readable time: retaining it is the recoverable failure.
    Unaged,
}

/// `exitedAt` for an exited record. A vanished record has no exit time, so
/// its loss counts from the sweep's first sighting of it, the
/// [`RETENTION_LOST_AT_TAG`] stamp. `createdAt` would make a long-lived
/// session that has just crashed, or every record left by a reboot,
/// removable at once.
fn loss_anchor(metadata: &SessionMetadata) -> Anchor {
    if let Some(exited_at) = metadata.exited_at.as_deref().filter(|at| !at.is_empty()) {
        return parse_iso8601_ms(exited_at).map_or(Anchor::Unaged, Anchor::At);
    }
    let Some(created_at) = parse_iso8601_ms(&metadata.created_at) else {
        return Anchor::Unaged;
    };
    let stamp = metadata
        .tags
        .as_ref()
        .and_then(|tags| tags.get(RETENTION_LOST_AT_TAG))
        .and_then(|value| parse_iso8601_ms(value));
    match stamp {
        // An older stamp was carried over from an earlier run of this id.
        Some(lost_at) if lost_at >= created_at => Anchor::At(lost_at),
        _ => Anchor::Unstamped,
    }
}

/// Run one record's locked work unless retention was stopped.
fn gated<T>(gate: &Mutex<bool>, work: impl FnOnce() -> T) -> Option<T> {
    let stopped = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if *stopped {
        return None;
    }
    Some(work())
}

/// Stamp or remove one record under both locks. The record must still be
/// the one observed, its owner still proven lost, and its current tags must
/// still call for `action`. A `keep` or `strategy=permanent` tag added after
/// the scan leaves the generation unchanged, so the decision is made again
/// from the record as it is now.
fn act(
    observed: &SessionInfo,
    action: Action,
    now_ms: i64,
    window_ms: i64,
    keep_max_age_ms: i64,
) -> bool {
    let Some(seen) = &observed.metadata else {
        return false;
    };
    let name = &observed.name;
    with_both_locks(name, || {
        let Some(mut raw) = read_metadata_map(name) else {
            return false;
        };
        let Some(current) = SessionMetadata::from_map(raw.clone()) else {
            return false;
        };
        if !metadata_matches_observation(seen, &current)
            || !known_owner_lost(daemon_identity(name, &current))
            || is_permanent(&current)
            || decide(&current, now_ms, window_ms, keep_max_age_ms) != Some(action)
        {
            return false;
        }
        match action {
            Action::Remove => {
                cleanup_all_while_locked(name);
                true
            }
            Action::Stamp => {
                let mut after = current.clone();
                after
                    .tags
                    .get_or_insert_with(TagMap::new)
                    .insert(RETENTION_LOST_AT_TAG.into(), iso8601_from_epoch_ms(now_ms));
                apply_metadata_diff(&current, &after, &mut raw);
                write_metadata_map(name, &raw).is_ok()
            }
        }
    })
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pty_core::registry::{
        event_lock_path, lock_or_refusal, metadata_path, read_metadata, write_metadata,
    };
    use std::path::{Path, PathBuf};

    const DAY_MS: i64 = 86_400_000;
    /// A pid that is certainly dead.
    const DEAD_PID: i32 = i32::MAX;

    struct Root(PathBuf);
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn root(label: &str) -> Root {
        let path = std::env::temp_dir().join(format!("ptyret-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).expect("isolated registry");
        Root(path)
    }

    fn iso(offset_ms: i64) -> String {
        iso8601_from_epoch_ms(now_epoch_ms() + offset_ms)
    }

    /// A finished record whose daemon is proven gone.
    fn gone(name: &str, created: i64, exited: Option<i64>, tags: &[(&str, String)]) {
        assert!(!registry::pid_alive(DEAD_PID));
        let metadata = SessionMetadata {
            generation: Some(format!("gen-{name}")),
            daemon_pid: Some(DEAD_PID),
            daemon_start_token: Some("fixture:dead".into()),
            daemon_boot_id: registry::read_boot_id(),
            created_at: iso(created),
            exited_at: exited.map(iso),
            exit_code: exited.map(|_| 0),
            tags: (!tags.is_empty()).then(|| {
                tags.iter()
                    .map(|(k, v)| (k.to_string(), v.clone()))
                    .collect()
            }),
            ..SessionMetadata::default()
        };
        write_metadata(name, &metadata).expect("metadata");
    }

    fn sweep(window_ms: i64) -> RetentionResult {
        sweep_finished_gated(window_ms, DEFAULT_KEEP_MAX_AGE_MS, &Mutex::new(false))
    }

    fn stamp_of(name: &str) -> Option<String> {
        read_metadata(name)?
            .tags?
            .get(RETENTION_LOST_AT_TAG)
            .cloned()
    }

    fn in_root<T>(root: &Root, f: impl FnOnce() -> T) -> T {
        registry::with_root(Path::new(&root.0), f)
    }

    #[test]
    fn exited_records_go_once_they_are_older_than_the_window() {
        let root = root("exited");
        in_root(&root, || {
            gone("old", -3 * DAY_MS, Some(-2 * DAY_MS), &[]);
            gone("young", -3 * DAY_MS, Some(-DAY_MS / 2), &[]);
            let result = sweep(DAY_MS);
            assert_eq!(result.removed, vec!["old".to_string()]);
            assert!(result.stamped.is_empty());
            assert!(!metadata_path("old").exists());
            assert!(metadata_path("young").exists());
        });
    }

    #[test]
    fn a_window_of_zero_is_off() {
        let root = root("zero");
        in_root(&root, || {
            gone("old", -3 * DAY_MS, Some(-2 * DAY_MS), &[]);
            gone("lost", -30 * DAY_MS, None, &[]);
            assert_eq!(sweep(0), RetentionResult::default());
            assert!(metadata_path("old").exists());
            assert_eq!(stamp_of("lost"), None);
        });
    }

    #[test]
    fn permanent_records_are_left_to_gc() {
        let root = root("permanent");
        in_root(&root, || {
            let tags = [("strategy", "permanent".to_string())];
            gone("perm", -3 * DAY_MS, Some(-2 * DAY_MS), &tags);
            gone("perm-lost", -30 * DAY_MS, None, &tags);
            assert_eq!(sweep(DAY_MS), RetentionResult::default());
            assert!(metadata_path("perm").exists());
            assert_eq!(stamp_of("perm-lost"), None);
        });
    }

    #[test]
    fn keep_holds_a_record_for_the_longer_window() {
        let root = root("keep");
        in_root(&root, || {
            let keep = [("keep", "true".to_string())];
            gone("kept", -10 * DAY_MS, Some(-2 * DAY_MS), &keep);
            gone("expired", -10 * DAY_MS, Some(-8 * DAY_MS), &keep);
            let result = sweep(DAY_MS);
            assert_eq!(result.removed, vec!["expired".to_string()]);
            assert!(metadata_path("kept").exists());
        });
    }

    #[test]
    fn a_vanished_record_counts_from_its_first_sighting() {
        let root = root("vanished");
        in_root(&root, || {
            // Created a month ago: by createdAt it would go at once.
            gone("lost", -30 * DAY_MS, None, &[]);
            let first = sweep(DAY_MS);
            assert_eq!(first.stamped, vec!["lost".to_string()]);
            assert!(first.removed.is_empty());
            let stamp = stamp_of("lost").expect("stamped");
            assert!(now_epoch_ms() - parse_iso8601_ms(&stamp).unwrap() < 60_000);
            // A second sweep inside the window keeps the stamp and the record.
            assert_eq!(sweep(DAY_MS), RetentionResult::default());
            assert_eq!(stamp_of("lost"), Some(stamp));
            // A window after the stamp, it goes.
            gone(
                "lost",
                -30 * DAY_MS,
                None,
                &[(RETENTION_LOST_AT_TAG, iso(-2 * DAY_MS))],
            );
            assert_eq!(sweep(DAY_MS).removed, vec!["lost".to_string()]);
            assert!(!metadata_path("lost").exists());
        });
    }

    #[test]
    fn a_stamp_from_an_earlier_run_of_the_id_does_not_count() {
        let root = root("carried");
        in_root(&root, || {
            // Relaunched an hour ago, carrying a month-old stamp.
            gone(
                "relaunched",
                -DAY_MS / 24,
                None,
                &[(RETENTION_LOST_AT_TAG, iso(-30 * DAY_MS))],
            );
            let result = sweep(DAY_MS);
            assert_eq!(result.stamped, vec!["relaunched".to_string()]);
            assert!(result.removed.is_empty());
            let stamp = stamp_of("relaunched").expect("restamped");
            assert!(now_epoch_ms() - parse_iso8601_ms(&stamp).unwrap() < 60_000);
        });
    }

    #[test]
    fn an_unaged_record_never_expires() {
        let root = root("unaged");
        in_root(&root, || {
            gone("unaged", 0, None, &[]);
            let mut metadata = read_metadata("unaged").unwrap();
            metadata.created_at = "not a time".into();
            write_metadata("unaged", &metadata).unwrap();
            assert_eq!(sweep(DAY_MS), RetentionResult::default());
            assert_eq!(stamp_of("unaged"), None);
        });
    }

    /// Add `key=value` the way `pty tag` does: the generation is unchanged,
    /// so an observation from before still matches.
    fn tag_in_place(name: &str, key: &str, value: &str) {
        let mut current = read_metadata(name).unwrap();
        current
            .tags
            .get_or_insert_with(TagMap::new)
            .insert(key.into(), value.into());
        write_metadata(name, &current).unwrap();
    }

    #[test]
    fn a_protective_tag_added_after_the_scan_stops_the_sweep() {
        let root = root("late-tag");
        in_root(&root, || {
            let now_ms = now_epoch_ms();
            let act_on = |name: &str, action| {
                let observed = registry::get_session_by_name(name).expect("observed");
                (observed, action)
            };
            for (name, key, value) in [("kept", "keep", "true"), ("perm", "strategy", "permanent")] {
                gone(name, -3 * DAY_MS, Some(-2 * DAY_MS), &[]);
                let (observed, action) = act_on(name, Action::Remove);
                let seen = observed.metadata.as_ref().unwrap();
                assert_eq!(
                    decide(seen, now_ms, DAY_MS, DEFAULT_KEEP_MAX_AGE_MS),
                    Some(action)
                );
                tag_in_place(name, key, value);
                assert!(
                    !act(&observed, action, now_ms, DAY_MS, DEFAULT_KEEP_MAX_AGE_MS),
                    "{name}"
                );
                assert!(metadata_path(name).exists(), "{name}");
            }
            gone("lost", -30 * DAY_MS, None, &[]);
            let (observed, action) = act_on("lost", Action::Stamp);
            tag_in_place("lost", "strategy", "permanent");
            assert!(!act(&observed, action, now_ms, DAY_MS, DEFAULT_KEEP_MAX_AGE_MS));
            assert_eq!(stamp_of("lost"), None);
            // Untouched since the scan, the same kind of observation is acted on.
            gone("plain", -3 * DAY_MS, Some(-2 * DAY_MS), &[]);
            let (observed, action) = act_on("plain", Action::Remove);
            assert!(act(&observed, action, now_ms, DAY_MS, DEFAULT_KEEP_MAX_AGE_MS));
            assert!(!metadata_path("plain").exists());
        });
    }

    #[test]
    fn a_stopped_sweep_touches_nothing() {
        let root = root("stopped");
        in_root(&root, || {
            gone("old", -3 * DAY_MS, Some(-2 * DAY_MS), &[]);
            gone("lost", -30 * DAY_MS, None, &[]);
            let stopped = Mutex::new(true);
            let result = sweep_finished_gated(DAY_MS, DEFAULT_KEEP_MAX_AGE_MS, &stopped);
            assert_eq!(result, RetentionResult::default());
            assert!(metadata_path("old").exists());
            assert_eq!(stamp_of("lost"), None);
        });
    }

    #[test]
    fn a_held_lock_skips_the_record() {
        let root = root("busy");
        in_root(&root, || {
            gone("old", -3 * DAY_MS, Some(-2 * DAY_MS), &[]);
            gone("lost", -30 * DAY_MS, None, &[]);
            let held = [
                lock_or_refusal(&event_lock_path("old")).expect("hold old"),
                lock_or_refusal(&event_lock_path("lost")).expect("hold lost"),
            ];
            assert_eq!(sweep(DAY_MS), RetentionResult::default());
            assert!(metadata_path("old").exists());
            assert_eq!(stamp_of("lost"), None);
            drop(held);
            let result = sweep(DAY_MS);
            assert_eq!(result.removed, vec!["old".to_string()]);
            assert_eq!(result.stamped, vec!["lost".to_string()]);
        });
    }

    #[cfg(target_os = "linux")]
    struct Live(std::process::Child);
    #[cfg(target_os = "linux")]
    impl Drop for Live {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A record owned by a live process: an exit record whose daemon is still
    /// finishing (`match`), one whose owner cannot be proven (`unknown`), and
    /// one left by an earlier boot (`mismatch`, so `vanished`).
    #[cfg(target_os = "linux")]
    #[test]
    fn records_whose_owner_is_not_proven_lost_are_kept_and_a_reboot_only_stamps() {
        let live = Live(
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("live owner"),
        );
        let pid = live.0.id() as i32;
        let root = root("owners");
        in_root(&root, || {
            let owned = |name: &str, token: Option<String>, boot: Option<String>, exited: bool| {
                let metadata = SessionMetadata {
                    generation: Some(format!("gen-{name}")),
                    daemon_pid: Some(pid),
                    daemon_start_token: token,
                    daemon_boot_id: boot,
                    created_at: iso(-30 * DAY_MS),
                    exited_at: exited.then(|| iso(-20 * DAY_MS)),
                    exit_code: exited.then_some(0),
                    ..SessionMetadata::default()
                };
                write_metadata(name, &metadata).expect("metadata");
            };
            let token = registry::read_process_start_token(pid);
            assert!(token.is_some());
            owned("finishing", token.clone(), registry::read_boot_id(), true);
            owned("unproven", None, registry::read_boot_id(), true);
            owned(
                "earlier-boot",
                token,
                Some("00000000-0000-0000-0000-000000000000".into()),
                false,
            );
            let result = sweep(DAY_MS);
            assert!(result.removed.is_empty(), "{result:?}");
            assert_eq!(result.stamped, vec!["earlier-boot".to_string()]);
            assert!(metadata_path("finishing").exists());
            assert!(metadata_path("unproven").exists());
            assert!(metadata_path("earlier-boot").exists());
        });
        drop(live);
    }
}
