//! Registry garbage collection and orphan-layout tag pruning.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pty_core::events::{AbandonReason, Event, append_event_locked};
use pty_core::ptyfile;
use pty_core::registry::{
    self, DEFAULT_KEEP_MAX_AGE_MS, DEFAULT_SOCKET_PROBE_BUDGET, SessionInfo, SessionMetadata,
    TagMap, apply_metadata_diff, cleanup_all_while_locked, cleanup_socket, event_lock_path,
    events_path, has_process_exited_for_reap, is_keep_expired, is_keep_requested, lock_or_refusal,
    metadata_matches_observation, metadata_path, now_epoch_ms, output_activity_path,
    parse_iso8601_ms, pid_alive, probe_sockets_within_budget, read_metadata, read_metadata_map,
    read_pid, read_pid_with, recovery_revision_path, session_dir, socket_path, socket_reachable,
    update_tags, with_both_locks, write_metadata, write_metadata_map,
};
use sha2::{Digest, Sha256};

use crate::{SpawnParams, apply_persisted_launch_options, spawn_daemon};
/// Options for one stateless garbage-collection pass.
#[derive(Debug, Clone)]
pub struct GcOptions {
    pub dry_run: bool,
    pub keep_max_age_ms: i64,
    pub idle_days: Option<i64>,
    pub fast_fail_window: Option<i64>,
    pub fast_fail_limit: Option<i64>,
    /// Executable implementing the private `__daemon` entrypoint.
    ///
    /// Required only when a stopped permanent session must be respawned.
    pub daemon_executable: Option<PathBuf>,
}

impl Default for GcOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            keep_max_age_ms: DEFAULT_KEEP_MAX_AGE_MS,
            idle_days: None,
            fast_fail_window: None,
            fast_fail_limit: None,
            daemon_executable: None,
        }
    }
}

/// What one pass did (or would do).
#[derive(Debug, Default)]
pub struct GcResult {
    pub removed: Vec<String>,
    pub kept: Vec<String>,
    /// Dead sessions swept despite a `keep` tag because they outlived the
    /// retention window. Disjoint from `removed`, which holds the untagged
    /// sweep, so the two reasons are reported apart.
    pub keep_expired: Vec<String>,
    pub killed_orphan_children: Vec<OrphanKill>,
    pub abandoned: Vec<Abandoned>,
    pub respawned: Vec<Respawned>,
    pub respawn_failed: Vec<RespawnFailed>,
    pub flapped: Vec<Flapped>,
    pub flapping_skipped: Vec<String>,
    pub reap_skipped: Vec<ReapSkip>,
}

#[derive(Debug)]
pub struct OrphanKill {
    pub name: String,
    pub parent: String,
    pub reason: &'static str,
}

#[derive(Debug)]
pub struct ReapSkip {
    pub name: String,
    pub operation: &'static str,
    pub reason: &'static str,
    pub signalled: bool,
}

#[derive(Debug)]
pub struct Abandoned {
    pub name: String,
    pub reason: AbandonReason,
    pub idle_days: Option<i64>,
}

#[derive(Debug)]
pub struct Respawned {
    pub name: String,
    pub ptyfile_reread: bool,
}

#[derive(Debug)]
pub struct RespawnFailed {
    pub name: String,
    pub error: String,
}

#[derive(Debug)]
pub struct Flapped {
    pub name: String,
    pub counter: i64,
    pub limit: i64,
    pub window: i64,
}

/// One session whose tags were (or would be) pruned.
#[derive(Debug)]
pub struct PrunedTags {
    pub name: String,
    pub removed_keys: Vec<String>,
}
/// One stateless reconciliation pass.
///
/// node: src/sessions.ts:487-726
pub fn gc(options: GcOptions) -> GcResult {
    let GcOptions {
        dry_run,
        keep_max_age_ms,
        idle_days: global_idle_days,
        fast_fail_window: global_fast_fail_window,
        fast_fail_limit: global_fast_fail_limit,
        daemon_executable,
    } = options;
    let mut result = GcResult::default();

    // Raw debris: runtime files whose metadata is missing or malformed.
    let raw_candidates = inventory_raw_cleanup_candidates(None);
    if dry_run {
        result.removed.extend(raw_candidates.iter().cloned());
    } else {
        for name in &raw_candidates {
            if cleanup_raw_candidate_guarded(name) {
                result.removed.push(name.clone());
            }
        }
    }
    let initial = registry::list_sessions();

    // STEP 1: orphan children, in name order so cycles resolve
    // deterministically.
    let mut with_parent: Vec<&SessionInfo> =
        initial.iter().filter(|s| parent_of(s).is_some()).collect();
    with_parent.sort_by(|a, b| a.name.cmp(&b.name));
    for s in with_parent {
        let parent = parent_of(s).unwrap_or_default();
        let parent_meta = read_metadata(&parent);
        let parent_pid = parent_meta
            .as_ref()
            .and_then(|m| read_pid_with(&parent, Some(m)));
        let parent_alive = parent_meta.is_some() && parent_pid.is_some_and(pid_alive);
        if parent_alive {
            continue;
        }
        let reason = if parent_meta.is_some() {
            "dead"
        } else {
            "missing"
        };
        if dry_run {
            result.killed_orphan_children.push(OrphanKill {
                name: s.name.clone(),
                parent,
                reason,
            });
            continue;
        }
        match reap_observed_session(s, ReapMode::Orphan) {
            Reap::Reaped(_) => result.killed_orphan_children.push(OrphanKill {
                name: s.name.clone(),
                parent,
                reason,
            }),
            Reap::NotEligible => {}
            Reap::Skipped { reason, signalled } => result.reap_skipped.push(ReapSkip {
                name: s.name.clone(),
                operation: "orphan",
                reason,
                signalled,
            }),
        }
    }

    // STEP 1.5: abandoned permanent sessions. This precedes respawn so a
    // cwd-gone or idle session cannot be recreated during the same pass.
    let after_step1 = if dry_run {
        initial.clone()
    } else {
        registry::list_sessions()
    };
    let now_ms = now_epoch_ms();
    for s in &after_step1 {
        let Some(meta) = s.metadata.as_ref() else {
            continue;
        };
        if !is_permanent(meta) {
            continue;
        }
        let Some(decision) = classify_abandoned(meta, global_idle_days, now_ms) else {
            continue;
        };
        if dry_run {
            result.abandoned.push(Abandoned {
                name: s.name.clone(),
                reason: decision.reason,
                idle_days: decision.idle_days,
            });
            continue;
        }
        match reap_observed_session(s, ReapMode::Abandoned { global_idle_days }) {
            Reap::Reaped(Some(decision)) => result.abandoned.push(Abandoned {
                name: s.name.clone(),
                reason: decision.reason,
                idle_days: decision.idle_days,
            }),
            Reap::Reaped(None) | Reap::NotEligible => {}
            Reap::Skipped { reason, signalled } => result.reap_skipped.push(ReapSkip {
                name: s.name.clone(),
                operation: "abandoned",
                reason,
                signalled,
            }),
        }
    }

    // STEP 2: permanent respawn. A dry run classifies the initial snapshots
    // without mutation; a real pass revalidates and serializes each decision
    // under the existing event → creation lock order.
    let excluded: BTreeSet<String> = result
        .abandoned
        .iter()
        .map(|a| a.name.clone())
        .chain(result.killed_orphan_children.iter().map(|k| k.name.clone()))
        .collect();
    let after_step15 = if dry_run {
        initial.clone()
    } else {
        registry::list_sessions()
    };
    for s in &after_step15 {
        let Some(meta) = s.metadata.as_ref() else {
            continue;
        };
        if excluded.contains(s.name.as_str()) || !s.is_gone() || !is_permanent(meta) {
            continue;
        }
        if dry_run {
            let (params, ptyfile_reread) = respawn_params(&s.name, meta);
            let decision = classify_flapping(
                meta,
                &params.command,
                &params.args,
                now_epoch_ms(),
                global_fast_fail_window,
                global_fast_fail_limit,
            );
            match decision.action {
                FlappingAction::Skip => result.flapping_skipped.push(s.name.clone()),
                FlappingAction::Flap => {
                    let d = decision;
                    result.flapped.push(Flapped {
                        name: s.name.clone(),
                        counter: d.counter,
                        limit: d.effective_limit,
                        window: d.effective_window,
                    });
                }
                FlappingAction::Respawn => result.respawned.push(Respawned {
                    name: s.name.clone(),
                    ptyfile_reread,
                }),
            }
            continue;
        }
        match reconcile_permanent(
            s,
            daemon_executable.as_deref(),
            global_fast_fail_window,
            global_fast_fail_limit,
        ) {
            PermanentOutcome::Respawned { ptyfile_reread } => {
                result.respawned.push(Respawned {
                    name: s.name.clone(),
                    ptyfile_reread,
                });
            }
            PermanentOutcome::Failed(error) => result.respawn_failed.push(RespawnFailed {
                name: s.name.clone(),
                error,
            }),
            PermanentOutcome::Flapped {
                counter,
                limit,
                window,
            } => result.flapped.push(Flapped {
                name: s.name.clone(),
                counter,
                limit,
                window,
            }),
            PermanentOutcome::SkippedFlapping => {
                result.flapping_skipped.push(s.name.clone());
            }
            PermanentOutcome::Stale => {}
        }
    }

    // STEP 3: exited/vanished non-permanent sessions lose their metadata;
    // `keep` exempts them only until the retention window expires.
    let final_list = if dry_run {
        initial
    } else {
        registry::list_sessions()
    };
    let now_ms = now_epoch_ms();
    for s in &final_list {
        if !s.is_gone() {
            continue;
        }
        let tags = s.metadata.as_ref().and_then(|m| m.tags.as_ref());
        if tags.and_then(|t| t.get("strategy")).map(String::as_str) == Some("permanent") {
            continue;
        }
        let keep_requested = is_keep_requested(tags);
        if keep_requested && !is_keep_expired(s.metadata.as_ref(), now_ms, keep_max_age_ms) {
            result.kept.push(s.name.clone());
            continue;
        }
        if dry_run || cleanup_observed_session(s) {
            let bucket = if keep_requested {
                &mut result.keep_expired
            } else {
                &mut result.removed
            };
            bucket.push(s.name.clone());
        }
    }
    result
}

#[derive(Debug, Clone, Copy)]
struct AbandonDecision {
    reason: AbandonReason,
    idle_days: Option<i64>,
}

fn is_permanent(meta: &SessionMetadata) -> bool {
    meta.tags
        .as_ref()
        .and_then(|t| t.get("strategy"))
        .map(String::as_str)
        == Some("permanent")
}

/// cwd-gone wins over idle; idle needs a valid positive threshold and a
/// parseable `lastAttachAt`.
///
/// node: src/sessions.ts:728-771
fn classify_abandoned(
    meta: &SessionMetadata,
    global_idle_days: Option<i64>,
    now_ms: i64,
) -> Option<AbandonDecision> {
    let tags = meta.tags.as_ref();
    let cwd_opted_out = tags
        .and_then(|t| t.get("strategy.abandon-if-cwd-gone"))
        .map(String::as_str)
        == Some("false");
    if !meta.cwd.is_empty()
        && !cwd_opted_out
        && std::fs::metadata(&meta.cwd).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    {
        return Some(AbandonDecision {
            reason: AbandonReason::CwdGone,
            idle_days: None,
        });
    }

    let tagged = tags
        .and_then(|t| t.get("strategy.idle-days"))
        .and_then(|v| js_parse_int(v))
        .filter(|v| *v > 0);
    let effective = tagged.or(global_idle_days.filter(|v| *v > 0))?;
    let last_attach = parse_iso8601_ms(meta.last_attach_at.as_deref()?)?;
    let age_days = (now_ms - last_attach).div_euclid(86_400_000);
    (age_days >= effective).then_some(AbandonDecision {
        reason: AbandonReason::Idle,
        idle_days: Some(age_days),
    })
}

/// Revalidate the stale list observation against current lock-held metadata.
/// Mutable policy fields are deliberately read from `current`, not from the
/// generation-only observation check.
fn revalidate_abandonment(
    observed: &SessionMetadata,
    current: &SessionMetadata,
    global_idle_days: Option<i64>,
    now_ms: i64,
) -> Option<AbandonDecision> {
    if !metadata_matches_observation(observed, current) || !is_permanent(current) {
        return None;
    }
    classify_abandoned(current, global_idle_days, now_ms)
}

const DEFAULT_FAST_FAIL_WINDOW: i64 = 60;
const DEFAULT_FAST_FAIL_LIMIT: i64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlappingAction {
    Respawn,
    Flap,
    Skip,
}

#[derive(Debug)]
struct FlappingDecision {
    action: FlappingAction,
    effective_window: i64,
    effective_limit: i64,
    counter: i64,
    bookkeeping: TagMap,
}

fn command_fingerprint(command: &str, args: &[String]) -> String {
    let mut hash = Sha256::new();
    hash.update(command.as_bytes());
    hash.update([0_u8]);
    for (index, arg) in args.iter().enumerate() {
        if index > 0 {
            hash.update([0_u8]);
        }
        hash.update(arg.as_bytes());
    }
    format!("{:x}", hash.finalize())[..16].to_string()
}

/// Classify one stopped permanent session from persisted bookkeeping.
///
/// node: src/sessions.ts:773-897
fn js_parse_int(s: &str) -> Option<i64> {
    let s = s.trim_start();
    let (neg, rest) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let end = rest.bytes().take_while(u8::is_ascii_digit).count();
    if end == 0 {
        return None;
    }
    let value: i64 = rest[..end].parse().ok()?;
    Some(if neg { -value } else { value })
}

fn classify_flapping(
    meta: &SessionMetadata,
    command: &str,
    args: &[String],
    now_ms: i64,
    global_window: Option<i64>,
    global_limit: Option<i64>,
) -> FlappingDecision {
    let tags = meta.tags.as_ref();
    let positive_tag = |key: &str| {
        tags.and_then(|t| t.get(key))
            .and_then(|v| js_parse_int(v))
            .filter(|v| *v > 0)
    };
    let effective_window = positive_tag("strategy.fast-fail-window")
        .or(global_window.filter(|v| *v > 0))
        .unwrap_or(DEFAULT_FAST_FAIL_WINDOW);
    let effective_limit = positive_tag("strategy.fast-fail-limit")
        .or(global_limit.filter(|v| *v > 0))
        .unwrap_or(DEFAULT_FAST_FAIL_LIMIT);
    let current_hash = command_fingerprint(command, args);
    let stored_hash = tags.and_then(|t| t.get("strategy.command-hash"));
    let command_changed = stored_hash.is_some_and(|h| h != &current_hash);
    let status = tags
        .and_then(|t| t.get("strategy.status"))
        .map(String::as_str);
    let previous_counter = tags
        .and_then(|t| t.get("strategy.consecutive-fast-fails"))
        .and_then(|v| js_parse_int(v))
        .unwrap_or(0);
    if status == Some("flapping") && !command_changed {
        return FlappingDecision {
            action: FlappingAction::Skip,
            effective_window,
            effective_limit,
            counter: previous_counter,
            bookkeeping: TagMap::new(),
        };
    }

    let last_respawn = tags
        .and_then(|t| t.get("strategy.last-respawn-at"))
        .and_then(|v| parse_iso8601_ms(v));
    let exited = meta.exited_at.as_deref().and_then(parse_iso8601_ms);
    let was_fast = last_respawn.zip(exited).is_some_and(|(start, end)| {
        let live_ms = end - start;
        live_ms >= 0 && live_ms < effective_window.saturating_mul(1000)
    });
    let counter = if command_changed {
        0
    } else if was_fast {
        previous_counter.saturating_add(1)
    } else {
        0
    };
    let mut bookkeeping = TagMap::new();
    if counter >= effective_limit {
        bookkeeping.insert("strategy.status".into(), "flapping".into());
        bookkeeping.insert(
            "strategy.consecutive-fast-fails".into(),
            counter.to_string(),
        );
        bookkeeping.insert("strategy.command-hash".into(), current_hash);
        if let Some(last) = tags.and_then(|t| t.get("strategy.last-respawn-at")) {
            bookkeeping.insert("strategy.last-respawn-at".into(), last.clone());
        }
        return FlappingDecision {
            action: FlappingAction::Flap,
            effective_window,
            effective_limit,
            counter,
            bookkeeping,
        };
    }

    bookkeeping.insert(
        "strategy.last-respawn-at".into(),
        pty_core::registry::iso8601_from_epoch_ms(now_ms),
    );
    bookkeeping.insert(
        "strategy.consecutive-fast-fails".into(),
        counter.to_string(),
    );
    bookkeeping.insert("strategy.command-hash".into(), current_hash);
    FlappingDecision {
        action: FlappingAction::Respawn,
        effective_window,
        effective_limit,
        counter,
        bookkeeping,
    }
}

enum PermanentOutcome {
    Respawned {
        ptyfile_reread: bool,
    },
    Failed(String),
    Flapped {
        counter: i64,
        limit: i64,
        window: i64,
    },
    SkippedFlapping,
    Stale,
}

/// Revalidate and serialize one permanent-session decision. The creation
/// lock remains held across cleanup and daemon publication so concurrent gc
/// invocations cannot both replace the same stable id. The event lock is
/// released before spawn because the new daemon publishes `session_start`.
fn reconcile_permanent(
    observed: &SessionInfo,
    daemon_executable: Option<&Path>,
    global_window: Option<i64>,
    global_limit: Option<i64>,
) -> PermanentOutcome {
    let name = &observed.name;
    let event_guard = match lock_or_refusal(&event_lock_path(name)) {
        Ok(guard) => guard,
        Err(_) => return PermanentOutcome::Stale,
    };
    let creation_guard = match lock_or_refusal(&registry::lock_path(name)) {
        Ok(guard) => guard,
        Err(_) => {
            event_guard.release();
            return PermanentOutcome::Stale;
        }
    };
    let Some(current) = read_metadata(name) else {
        return PermanentOutcome::Stale;
    };
    if observed
        .metadata
        .as_ref()
        .is_none_or(|old| !metadata_matches_observation(old, &current))
        || !is_permanent(&current)
        || read_pid_with(name, Some(&current)).is_some_and(pid_alive)
    {
        return PermanentOutcome::Stale;
    }

    let (mut params, ptyfile_reread) = respawn_params(name, &current);
    let now_ms = now_epoch_ms();
    let decision = classify_flapping(
        &current,
        &params.command,
        &params.args,
        now_ms,
        global_window,
        global_limit,
    );
    match decision.action {
        FlappingAction::Skip => PermanentOutcome::SkippedFlapping,
        FlappingAction::Flap => {
            let Some(mut raw) = read_metadata_map(name) else {
                return PermanentOutcome::Stale;
            };
            let Some(before) = SessionMetadata::from_map(raw.clone()) else {
                return PermanentOutcome::Stale;
            };
            if !metadata_matches_observation(&current, &before) {
                return PermanentOutcome::Stale;
            }
            let mut after = before.clone();
            let tags = after.tags.get_or_insert_with(TagMap::new);
            for (key, value) in &decision.bookkeeping {
                tags.insert(key.clone(), value.clone());
            }
            apply_metadata_diff(&before, &after, &mut raw);
            if write_metadata_map(name, &raw).is_err() {
                return PermanentOutcome::Stale;
            }
            let event = Event::session_flapping(
                name,
                decision.counter as u64,
                decision.effective_limit as u64,
                decision.effective_window as u64,
            );
            let _ = append_event_locked(name, &event);
            PermanentOutcome::Flapped {
                counter: decision.counter,
                limit: decision.effective_limit,
                window: decision.effective_window,
            }
        }
        FlappingAction::Respawn => {
            let Some(daemon_executable) = daemon_executable else {
                return PermanentOutcome::Failed(
                    "daemon executable is required to respawn permanent sessions".to_string(),
                );
            };
            for (key, value) in &decision.bookkeeping {
                params.tags.insert(key.clone(), value.clone());
            }
            if let Err(error) = pty_core::spawn::resolve_command(&params.command) {
                return PermanentOutcome::Failed(error);
            }
            let mut retry_meta = current.clone();
            retry_meta.tags = (!params.tags.is_empty()).then(|| params.tags.clone());
            cleanup_all_while_locked(name);
            event_guard.release();
            let spawn = spawn_daemon(daemon_executable, params).map_err(|error| error.to_string());
            if spawn.is_err() && read_metadata(name).is_none() {
                // Preserve the desired state when launch failed before a new
                // daemon could publish. The next gc tick must be able to retry.
                let _ = write_metadata(name, &retry_meta);
            }
            creation_guard.release();
            match spawn {
                Ok(_) => PermanentOutcome::Respawned { ptyfile_reread },
                Err(error) => PermanentOutcome::Failed(error),
            }
        }
    }
}

/// Rebuild launch parameters from last-known-good metadata, overlaying a
/// fresh pty.toml definition when the recorded binding still resolves.
///
/// node: src/sessions.ts:899-982
fn respawn_params(name: &str, meta: &SessionMetadata) -> (SpawnParams, bool) {
    let mut command = meta.command.clone();
    let mut args = meta.args.clone();
    let mut display_command = meta.display_command.clone();
    let mut cwd = meta.cwd.clone();
    let mut tags = meta.tags.clone().unwrap_or_default();
    let ptyfile_path = tags.get("ptyfile").cloned();
    let ptyfile_session = tags.get("ptyfile.session").cloned();
    let ptyfile_reread = ptyfile_path.is_some();
    if let (Some(path), Some(short_name)) = (&ptyfile_path, &ptyfile_session)
        && let Some(dir) = Path::new(path).parent()
        && let Ok(file) = ptyfile::read_pty_file(Some(dir))
        && let Some(session) = file.sessions.iter().find(|s| &s.short_name == short_name)
    {
        command = "/bin/sh".into();
        args = vec!["-c".into(), ptyfile::command_with_env_exports(session)];
        display_command = session.command.clone();
        cwd = session
            .cwd
            .clone()
            .unwrap_or_else(|| file.dir.to_string_lossy().into_owned());
        let mut user_keys: Vec<String> = session
            .tags
            .as_ref()
            .map(|manifest| manifest.keys().cloned().collect())
            .unwrap_or_default();
        user_keys.sort();
        let previous_keys: Vec<String> = tags
            .get("ptyfile.tags")
            .map(String::as_str)
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(str::to_string)
            .collect();
        for key in previous_keys {
            if !user_keys.iter().any(|user_key| user_key == &key) {
                tags.shift_remove(&key);
            }
        }
        if let Some(manifest_tags) = &session.tags {
            for (key, value) in manifest_tags {
                tags.insert(key.clone(), value.clone());
            }
        }
        tags.insert("ptyfile".into(), path.clone());
        tags.insert("ptyfile.session".into(), short_name.clone());
        tags.insert("ptyfile.tags".into(), user_keys.join(","));
    }
    tags.shift_remove("strategy.status");

    let mut params = SpawnParams::new(name, &command, &args);
    params.display_command = display_command;
    params.cwd = cwd;
    params.tags = tags;
    params.display_name = meta.display_name.clone();
    apply_persisted_launch_options(&mut params, meta);
    params.respawn = true;
    (params, ptyfile_reread)
}

fn parent_of(s: &SessionInfo) -> Option<String> {
    s.metadata
        .as_ref()
        .and_then(|m| m.tags.as_ref())
        .and_then(|t| t.get("parent"))
        .filter(|p| !p.is_empty())
        .cloned()
}

/// Registry debris that `list_sessions` cannot represent: `.sock`/`.pid`
/// with a dead pid and missing/malformed metadata (socket unreachable), or
/// malformed metadata with no runtime files at all.
///
/// node: src/sessions.ts:643-705
fn inventory_raw_cleanup_candidates(only: Option<&str>) -> Vec<String> {
    let Ok(dir) = std::fs::read_dir(session_dir()) else {
        return Vec::new();
    };
    let entries: BTreeSet<String> = dir
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    let mut names: BTreeSet<String> = BTreeSet::new();
    for entry in &entries {
        let name = entry
            .strip_suffix(".events.jsonl")
            .or_else(|| entry.strip_suffix(".sock"))
            .or_else(|| entry.strip_suffix(".pid"))
            .or_else(|| entry.strip_suffix(".json"));
        if let Some(name) = name
            && only.is_none_or(|o| o == name)
        {
            names.insert(name.to_string());
        }
    }

    struct Candidate {
        name: String,
        has_socket: bool,
        has_pid: bool,
        has_metadata: bool,
        pid_dead: bool,
    }
    let candidates: Vec<Candidate> = names
        .into_iter()
        .filter_map(|name| {
            let has_socket = entries.contains(&format!("{name}.sock"));
            let has_pid = entries.contains(&format!("{name}.pid"));
            let has_metadata = entries.contains(&format!("{name}.json"));
            if has_metadata && !metadata_is_malformed(&name) {
                return None;
            }
            let pid = if has_pid { read_pid(&name) } else { None };
            let pid_dead = pid.is_some_and(|p| !pid_alive(p));
            Some(Candidate {
                name,
                has_socket,
                has_pid,
                has_metadata,
                pid_dead,
            })
        })
        .collect();

    let to_probe: Vec<PathBuf> = candidates
        .iter()
        .filter(|c| c.has_socket && c.pid_dead)
        .map(|c| socket_path(&c.name))
        .collect();
    let reachability: HashMap<PathBuf, bool> =
        probe_sockets_within_budget(&to_probe, DEFAULT_SOCKET_PROBE_BUDGET);

    candidates
        .into_iter()
        .filter(|c| {
            if c.pid_dead {
                return !c.has_socket || reachability.get(&socket_path(&c.name)) == Some(&false);
            }
            c.has_metadata && !c.has_pid && !c.has_socket
        })
        .map(|c| c.name)
        .collect()
}

/// "malformed": the file exists but is not a JSON object (an unreadable
/// file is retained, as Node does).
///
/// node: src/sessions.ts:620-634
fn metadata_is_malformed(name: &str) -> bool {
    match std::fs::read(metadata_path(name)) {
        Ok(bytes) => !matches!(
            serde_json::from_slice::<serde_json::Value>(&bytes),
            Ok(serde_json::Value::Object(_))
        ),
        Err(_) => false,
    }
}

/// Remove one raw candidate while owning both locks, re-inventorying under
/// the lock so a generation that appeared meanwhile is left alone.
///
/// node: src/sessions.ts:707-753
fn cleanup_raw_candidate_guarded(name: &str) -> bool {
    with_both_locks(name, || {
        if !inventory_raw_cleanup_candidates(Some(name))
            .iter()
            .any(|n| n == name)
        {
            return false;
        }
        cleanup_socket(name);
        let _ = std::fs::remove_file(metadata_path(name));
        let _ = std::fs::remove_file(events_path(name));
        let _ = std::fs::remove_file(recovery_revision_path(name));
        let _ = std::fs::remove_file(output_activity_path(name));
        true
    })
    .unwrap_or(false)
}

/// Generation-CAS cleanup of an observed session.
///
/// node: src/sessions.ts:769-790
fn cleanup_observed_session(session: &SessionInfo) -> bool {
    let Some(observed) = &session.metadata else {
        return false;
    };
    with_both_locks(&session.name, || match read_metadata(&session.name) {
        Some(current) if metadata_matches_observation(observed, &current) => {
            cleanup_all_while_locked(&session.name);
            true
        }
        _ => false,
    })
    .unwrap_or(false)
}

#[derive(Clone, Copy)]
enum ReapMode {
    Orphan,
    Abandoned { global_idle_days: Option<i64> },
}

enum Reap {
    Reaped(Option<AbandonDecision>),
    /// The lock-held metadata no longer satisfies the abandonment policy.
    NotEligible,
    Skipped {
        reason: &'static str,
        signalled: bool,
    },
}

/// SIGTERM a session (when running), wait for its daemon, optionally append
/// an abandonment event, then remove its files. Abandonment is recomputed
/// from lock-held metadata before any signal. A running/socket-reachable
/// observation without a confirmed live pid is never unlinked.
///
/// node: src/sessions.ts:821-880
fn reap_observed_session(session: &SessionInfo, mode: ReapMode) -> Reap {
    let Some(observed) = &session.metadata else {
        return Reap::Skipped {
            reason: "stale",
            signalled: false,
        };
    };
    let name = &session.name;
    let mut abandonment = None;
    let mut signalled_pid = None;
    let first = with_both_locks(name, || -> Result<(), &'static str> {
        let current = read_metadata(name).ok_or("stale")?;
        if let ReapMode::Abandoned { global_idle_days } = mode {
            abandonment =
                revalidate_abandonment(observed, &current, global_idle_days, now_epoch_ms());
            if abandonment.is_none() {
                return Err("not-eligible");
            }
        } else if !metadata_matches_observation(observed, &current) {
            return Err("stale");
        }
        let current_pid =
            read_pid_with(name, Some(&current)).filter(|pid| !has_process_exited_for_reap(*pid));
        let running_now =
            session.is_running() || current_pid.is_some() || socket_reachable(&socket_path(name));
        if running_now {
            let Some(pid) = current_pid else {
                return Err("pid-unavailable");
            };
            // SAFETY: signalling a live pid validated against lock-held
            // metadata and its process-start token.
            let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
            if rc == 0 {
                signalled_pid = Some(pid);
            } else if pid_alive(pid) {
                return Err("signal-failed");
            }
        }
        Ok(())
    });
    match first {
        Err(_) => {
            return Reap::Skipped {
                reason: "busy",
                signalled: false,
            };
        }
        Ok(Err("not-eligible")) => return Reap::NotEligible,
        Ok(Err(reason)) => {
            return Reap::Skipped {
                reason,
                signalled: false,
            };
        }
        Ok(Ok(())) => {}
    }

    if let Some(pid) = signalled_pid {
        let deadline = Instant::now() + Duration::from_secs(7);
        while Instant::now() < deadline && !has_process_exited_for_reap(pid) {
            std::thread::sleep(Duration::from_millis(25));
        }
        if !has_process_exited_for_reap(pid) {
            return Reap::Skipped {
                reason: "shutdown-timeout",
                signalled: true,
            };
        }
    }
    let second = with_both_locks(name, || {
        let current = match read_metadata(name) {
            Some(current) if metadata_matches_observation(observed, &current) => current,
            _ => return false,
        };
        if read_pid_with(name, Some(&current)).is_some_and(|pid| !has_process_exited_for_reap(pid))
            || socket_reachable(&socket_path(name))
        {
            return false;
        }
        if let Some(decision) = abandonment {
            let event = Event::session_abandoned(
                name,
                decision.reason,
                decision.idle_days.map(|days| days as u64),
            );
            let _ = append_event_locked(name, &event);
        }
        cleanup_all_while_locked(name);
        true
    });
    match second {
        Err(_) => Reap::Skipped {
            reason: "busy",
            signalled: signalled_pid.is_some(),
        },
        Ok(false) => Reap::Skipped {
            reason: "stale",
            signalled: signalled_pid.is_some(),
        },
        Ok(true) => Reap::Reaped(abandonment),
    }
}

/// `:l<pid>-<rand>` → the pid, when the key has that shape.
///
/// node: src/sessions.ts:2026 (`ORPHAN_LAYOUT_TAG_RE`)
fn orphan_layout_tag_pid(key: &str) -> Option<Option<i64>> {
    let rest = key.strip_prefix(":l")?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let tail = rest[digits..].strip_prefix('-')?;
    if tail.is_empty()
        || !tail
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    {
        return None;
    }
    Some(rest[..digits].parse::<i64>().ok())
}

/// Drop layout tags whose owning pid is dead on every running session.
///
/// node: src/sessions.ts:2042-2074
pub fn prune_orphan_layout_tags(dry_run: bool) -> Vec<PrunedTags> {
    let mut out = Vec::new();
    for s in registry::list_sessions() {
        if !s.is_running() {
            continue;
        }
        let Some(tags) = s.metadata.as_ref().and_then(|m| m.tags.as_ref()) else {
            continue;
        };
        let to_remove: Vec<String> = tags
            .keys()
            .filter(|key| match orphan_layout_tag_pid(key) {
                None => false,
                Some(None) => true,
                Some(Some(pid)) => {
                    pid <= 0 || !i32::try_from(pid).map(pid_alive).unwrap_or(false)
                }
            })
            .cloned()
            .collect();
        if to_remove.is_empty() {
            continue;
        }
        if !dry_run && update_tags(&s.name, &TagMap::new(), &to_remove).is_err() {
            // The metadata disappeared between listing and update.
            continue;
        }
        out.push(PrunedTags {
            name: s.name,
            removed_keys: to_remove,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_tag_shape() {
        assert_eq!(orphan_layout_tag_pid(":l1234-abc"), Some(Some(1234)));
        assert_eq!(orphan_layout_tag_pid(":layout"), None);
        assert_eq!(orphan_layout_tag_pid(":l12-"), None);
        assert_eq!(orphan_layout_tag_pid(":l-abc"), None);
        assert_eq!(orphan_layout_tag_pid(":l12-ABC"), None);
    }

    #[test]
    fn abandonment_revalidation_uses_current_policy_fields() {
        let mut tags = TagMap::new();
        tags.insert("strategy".into(), "permanent".into());
        tags.insert("strategy.idle-days".into(), "1".into());
        let observed = SessionMetadata {
            generation: Some("same-generation".into()),
            cwd: "/".into(),
            last_attach_at: Some("1970-01-01T00:00:00.000Z".into()),
            tags: Some(tags),
            ..Default::default()
        };
        let mut current = observed.clone();
        current.last_attach_at = Some("2030-01-01T00:00:00.000Z".into());
        assert!(metadata_matches_observation(&observed, &current));
        assert!(classify_abandoned(&observed, None, 172_800_000).is_some());
        assert!(revalidate_abandonment(&observed, &current, None, 172_800_000).is_none());
        let mut cwd_tags = TagMap::new();
        cwd_tags.insert("strategy".into(), "permanent".into());
        let missing = SessionMetadata {
            generation: Some("cwd-generation".into()),
            cwd: "/definitely/missing/pty-gc-revalidation".into(),
            tags: Some(cwd_tags),
            ..Default::default()
        };
        let mut opted_out = missing.clone();
        opted_out
            .tags
            .as_mut()
            .unwrap()
            .insert("strategy.abandon-if-cwd-gone".into(), "false".into());
        assert!(classify_abandoned(&missing, None, 0).is_some());
        assert!(revalidate_abandonment(&missing, &opted_out, None, 0).is_none());
    }
}
