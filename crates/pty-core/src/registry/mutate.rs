//! Whole-record metadata mutation under the creation lock, and the three
//! presentation patches built on it (`metadata patch`, `rename`, `tag`),
//! each emitting its event under the event lock.
//!
//! node: src/sessions.ts:330-593, 740-752

use std::time::{Duration, Instant};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::lock::{
    LockRefusal, METADATA_PATCH_LOCK_WAIT, is_creation_lock_held, metadata_busy_message,
    take_event_lock, wait_for_event_lock, wait_for_metadata_lock,
};
use super::metadata::{
    SessionMetadata, TagMap, apply_metadata_diff, read_metadata_map, write_metadata_map,
};
use super::names::validate_display_name;
use crate::events::{Event, append_event_locked};

/// Compare-and-swap guards for [`mutate_metadata_under_lock`].
#[derive(Debug, Default, Clone)]
pub struct MutateOptions {
    /// Refuse (`GenerationMismatch`) unless the record's `generation` equals this.
    pub expected_generation: Option<String>,
    /// Refuse (`GenerationMismatch`) unless the record still matches this
    /// observation: same `generation`, or (legacy records) identical content.
    pub expected_metadata: Option<SessionMetadata>,
}

/// Outcome of [`mutate_metadata_under_lock`].
///
/// node: src/sessions.ts:338-345
#[derive(Debug, Clone, PartialEq)]
pub enum MutateStatus {
    /// `<name>.lock` is held by a live process.
    Busy,
    /// No readable `<name>.json`.
    Missing,
    /// The record no longer matches `expected_generation` / `expected_metadata`.
    GenerationMismatch,
    /// The record changed on disk while the mutation ran.
    Stale,
    /// The mutation returned `false`; nothing was written.
    Unchanged(SessionMetadata),
    /// The record was rewritten.
    Changed(SessionMetadata),
}

impl MutateStatus {
    /// Node's status string.
    pub fn as_str(&self) -> &'static str {
        match self {
            MutateStatus::Busy => "busy",
            MutateStatus::Missing => "missing",
            MutateStatus::GenerationMismatch => "generation-mismatch",
            MutateStatus::Stale => "stale",
            MutateStatus::Unchanged(_) => "unchanged",
            MutateStatus::Changed(_) => "changed",
        }
    }
}

/// Does the current record still belong to the observation?
///
/// node: src/sessions.ts:740-752
pub fn metadata_matches_observation(observed: &SessionMetadata, current: &SessionMetadata) -> bool {
    if observed.generation.is_some() || current.generation.is_some() {
        return observed.generation.is_some() && observed.generation == current.generation;
    }
    observed.to_map() == current.to_map()
}

/// Serialize one whole-record mutation against every other writer: take
/// `<name>.lock`, read, check the guards, run `mutate`, re-read to detect a
/// concurrent rewrite, then publish the mutated object (unknown fields and
/// key order preserved).
///
/// node: src/sessions.ts:347-398
pub fn mutate_metadata_under_lock(
    name: &str,
    mutate: impl FnOnce(&mut SessionMetadata) -> bool,
    options: &MutateOptions,
) -> MutateStatus {
    mutate_metadata_under_lock_with(name, mutate, options, |_| {})
}

/// [`mutate_metadata_under_lock`] with an `on_published` hook that runs
/// after the write, still under the lock.
pub fn mutate_metadata_under_lock_with(
    name: &str,
    mutate: impl FnOnce(&mut SessionMetadata) -> bool,
    options: &MutateOptions,
    on_published: impl FnOnce(&SessionMetadata),
) -> MutateStatus {
    mutate_metadata_under_lock_with_wait(
        name,
        mutate,
        options,
        on_published,
        std::time::Duration::ZERO,
    )
}

/// [`mutate_metadata_under_lock_with`] that waits up to `wait` for
/// `<name>.lock` instead of reporting `Busy` at once.
///
/// compoundingtech/pty#180: `pty run` holds the creation lock across daemon
/// spawn and publication, so a just-spawned attached child's first metadata
/// write met a held lock. Transient creation/attach holders clear quickly;
/// a holder that outlives the budget still reports `Busy` (fail-closed), and
/// a lock file that cannot be created at all reports `Busy` at once, exactly
/// as the fail-fast path does today.
pub fn mutate_metadata_under_lock_with_wait(
    name: &str,
    mutate: impl FnOnce(&mut SessionMetadata) -> bool,
    options: &MutateOptions,
    on_published: impl FnOnce(&SessionMetadata),
    wait: std::time::Duration,
) -> MutateStatus {
    mutate_metadata_under_lock_core(name, mutate, options, on_published, |_| {}, wait)
}

/// [`mutate_metadata_under_lock`] whose `on_settled` hook runs once the
/// record is final for this call — after a `Changed` publish or an
/// `Unchanged` decline — still under `<name>.lock` and only when the guards
/// held. A writer whose side effect must never outlive its generation's
/// ownership of the name (the output-activity sidecar) does it here, so a
/// replacement generation cannot publish between the check and the effect.
pub(crate) fn mutate_metadata_under_lock_settled(
    name: &str,
    mutate: impl FnOnce(&mut SessionMetadata) -> bool,
    options: &MutateOptions,
    on_settled: impl Fn(&SessionMetadata),
) -> MutateStatus {
    mutate_metadata_under_lock_core(
        name,
        mutate,
        options,
        &on_settled,
        &on_settled,
        std::time::Duration::ZERO,
    )
}

fn mutate_metadata_under_lock_core(
    name: &str,
    mutate: impl FnOnce(&mut SessionMetadata) -> bool,
    options: &MutateOptions,
    on_published: impl FnOnce(&SessionMetadata),
    on_unchanged: impl FnOnce(&SessionMetadata),
    wait: std::time::Duration,
) -> MutateStatus {
    let _lock = match wait_for_metadata_lock(name, wait) {
        Ok(guard) => guard,
        // `Unavailable` folds into `Busy` here, as `acquire_lock` folds
        // every I/O error into "not taken" today. The presentation path
        // takes the event lock first and surfaces its cause, so this arm
        // is nearly unreachable through it; daemon-internal callers keep
        // their existing `Busy` handling either way.
        Err(LockRefusal::Busy) | Err(LockRefusal::Unavailable(_)) => return MutateStatus::Busy,
    };
    let Some(raw) = read_metadata_map(name) else {
        return MutateStatus::Missing;
    };
    let Some(before) = SessionMetadata::from_map(raw.clone()) else {
        return MutateStatus::Missing;
    };
    let guards_hold = |current: &SessionMetadata| {
        if let Some(expected) = &options.expected_metadata
            && !metadata_matches_observation(expected, current)
        {
            return false;
        }
        if let Some(expected) = &options.expected_generation
            && current.generation.as_deref() != Some(expected.as_str())
        {
            return false;
        }
        true
    };
    if !guards_hold(&before) {
        return MutateStatus::GenerationMismatch;
    }

    let mut after = before.clone();
    if !mutate(&mut after) {
        on_unchanged(&before);
        return MutateStatus::Unchanged(before);
    }

    let Some(latest_raw) = read_metadata_map(name) else {
        return MutateStatus::Stale;
    };
    if latest_raw != raw {
        return MutateStatus::Stale;
    }
    if !guards_hold(&before) {
        return MutateStatus::GenerationMismatch;
    }

    let mut published = raw;
    apply_metadata_diff(&before, &after, &mut published);
    if write_metadata_map(name, &published).is_err() {
        return MutateStatus::Stale;
    }
    let published = SessionMetadata::from_map(published).unwrap_or(after);
    on_published(&published);
    MutateStatus::Changed(published)
}

/// Typed request for filesystem-backed, generation-fenced tag compare-and-set.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TagCompareAndSetRequest {
    pub expected_generation: String,
    pub tag: String,
    pub expected_value: String,
    pub value: String,
}

impl TagCompareAndSetRequest {
    pub fn validate(&self) -> bool {
        !self.expected_generation.is_empty() && !self.tag.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "_tag")]
pub enum TagCompareAndSetResult {
    Changed { value: String },
    Unchanged { value: String },
    ValueMismatch { value: Option<String> },
    Busy,
    Missing,
    GenerationMismatch,
    Stale,
}

/// Generation-fenced compare-and-set for one exact tag value.
///
/// The daemon protocol is the only supported way to disarm a startup lease;
/// this filesystem primitive intentionally does not know about daemon timers.
///
/// node: src/sessions.ts `compareAndSetTagValue`
pub fn compare_and_set_tag_value(
    name: &str,
    expected_generation: &str,
    tag: &str,
    expected_value: &str,
    value: &str,
) -> TagCompareAndSetResult {
    assert!(!tag.is_empty(), "tag must not be empty");
    let mut current_value = None;
    let mut matched = false;
    let result = mutate_metadata_under_lock(
        name,
        |metadata| {
            current_value = metadata
                .tags
                .as_ref()
                .and_then(|tags| tags.get(tag))
                .cloned();
            if current_value.as_deref() != Some(expected_value) {
                return false;
            }
            matched = true;
            if current_value.as_deref() == Some(value) {
                return false;
            }
            metadata
                .tags
                .get_or_insert_with(TagMap::new)
                .insert(tag.to_string(), value.to_string());
            true
        },
        &MutateOptions {
            expected_generation: Some(expected_generation.to_string()),
            expected_metadata: None,
        },
    );
    match result {
        MutateStatus::Changed(_) => TagCompareAndSetResult::Changed {
            value: value.to_string(),
        },
        MutateStatus::Unchanged(_) if matched => TagCompareAndSetResult::Unchanged {
            value: value.to_string(),
        },
        MutateStatus::Unchanged(_) => TagCompareAndSetResult::ValueMismatch {
            value: current_value,
        },
        MutateStatus::Busy => TagCompareAndSetResult::Busy,
        MutateStatus::Missing => TagCompareAndSetResult::Missing,
        MutateStatus::GenerationMismatch => TagCompareAndSetResult::GenerationMismatch,
        MutateStatus::Stale => TagCompareAndSetResult::Stale,
    }
}

/// A presentation patch: `displayName` (`Some(None)` clears) and per-key
/// tag updates (`None` removes).
///
/// node: src/sessions.ts:321-324
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MetadataPatch {
    pub display_name: Option<Option<String>>,
    pub tags: Option<IndexMap<String, Option<String>>>,
}

impl MetadataPatch {
    /// Parse and validate the JSON object `pty metadata patch` reads from
    /// stdin. Errors carry Node's texts.
    ///
    /// node: src/sessions.ts:400-434
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let Value::Object(obj) = value else {
            return Err("Metadata patch must be a JSON object.".to_string());
        };
        for key in obj.keys() {
            if key != "displayName" && key != "tags" {
                return Err(format!(
                    "Metadata patch has unknown field \"{key}\". Allowed fields: displayName, tags."
                ));
            }
        }
        let mut patch = MetadataPatch::default();
        if let Some(dn) = obj.get("displayName") {
            patch.display_name = Some(match dn {
                Value::Null => None,
                Value::String(s) => Some(s.clone()),
                _ => return Err("Metadata patch displayName must be a string or null.".to_string()),
            });
        }
        if let Some(tags) = obj.get("tags") {
            let Value::Object(tags) = tags else {
                return Err("Metadata patch tags must be a JSON object.".to_string());
            };
            let mut out = IndexMap::new();
            for (k, v) in tags {
                if k.is_empty() {
                    return Err("Metadata patch tag keys must be non-empty.".to_string());
                }
                let value = match v {
                    Value::Null => None,
                    Value::String(s) => Some(s.clone()),
                    _ => {
                        return Err(format!(
                            "Metadata patch tag values must be strings or null (invalid key: \"{k}\")."
                        ));
                    }
                };
                out.insert(k.clone(), value);
            }
            patch.tags = Some(out);
        }
        patch.validate()?;
        Ok(patch)
    }

    /// Validate a typed patch (display name rules, non-empty tag keys).
    ///
    /// node: src/sessions.ts:400-434
    pub fn validate(&self) -> Result<(), String> {
        if let Some(Some(dn)) = &self.display_name {
            validate_display_name(dn).map_err(|e| format!("Invalid displayName: {e}"))?;
        }
        if let Some(tags) = &self.tags {
            for key in tags.keys() {
                if key.is_empty() {
                    return Err("Metadata patch tag keys must be non-empty.".to_string());
                }
            }
        }
        Ok(())
    }
}

/// The result of a presentation patch.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MetadataPatchResult {
    pub changed: bool,
    pub metadata: SessionMetadata,
}

/// The `previous` / `value` halves of a `metadata_change` event: only the
/// fields and tag keys that effectively changed, `null` meaning absent.
///
/// node: src/sessions.ts:330-333; src/events.ts:175-186
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataChangeSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<IndexMap<String, Option<String>>>,
}

/// Which event a presentation patch emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataPatchEvent {
    MetadataChange,
    DisplayNameChange,
    TagsChange,
}

/// The fail-fast core used by `rename` / `tag` and direct callers: event
/// lock, then the creation lock, mutate, publish, append exactly one event
/// when something changed.
///
/// node: src/sessions.ts:478-595
pub fn apply_metadata_patch_by_id(
    id: &str,
    patch: &MetadataPatch,
    event: MetadataPatchEvent,
) -> Result<MetadataPatchResult, String> {
    apply_metadata_patch_by_id_with_wait(id, patch, event, Duration::ZERO)
}

fn apply_metadata_patch_by_id_with_wait(
    id: &str,
    patch: &MetadataPatch,
    event: MetadataPatchEvent,
    wait: Duration,
) -> Result<MetadataPatchResult, String> {
    patch.validate()?;
    let deadline = Instant::now() + wait;
    let _event_lock = if wait.is_zero() {
        take_event_lock(id)?
    } else {
        wait_for_event_lock(id, wait)?
    };

    #[derive(Default)]
    struct PatchState {
        previous_tags: TagMap,
        next_tags: TagMap,
        previous: MetadataChangeSnapshot,
        value: MetadataChangeSnapshot,
        error: Option<String>,
    }
    let state = std::cell::RefCell::new(PatchState::default());

    // `metadata patch --id` waits out a transient creation/attach holder.
    // Its event-lock acquisition above and the creation lock share one
    // bounded budget. Direct callers, `rename`, and `tag` pass zero and keep
    // their historical fail-fast behavior. A holder that outlives the budget
    // still fails closed with today's busy text.
    let result = mutate_metadata_under_lock_with_wait(
        id,
        |metadata| {
            let mut st = state.borrow_mut();
            let st = &mut *st;
            st.previous_tags = metadata.tags.clone().unwrap_or_default();
            st.next_tags = st.previous_tags.clone();
            if let Some(requested) = &patch.display_name {
                let before = metadata.display_name.clone();
                let after = requested.clone();
                if before != after {
                    st.previous.display_name = Some(before);
                    st.value.display_name = Some(after.clone());
                    metadata.display_name = after;
                }
            }
            if let Some(tags) = &patch.tags {
                let mut changed_keys: Vec<String> = Vec::new();
                for (key, requested) in tags {
                    let before = st.previous_tags.get(key).cloned();
                    if &before == requested {
                        continue;
                    }
                    changed_keys.push(key.clone());
                    match requested {
                        None => {
                            st.next_tags.shift_remove(key);
                        }
                        Some(v) => {
                            st.next_tags.insert(key.clone(), v.clone());
                        }
                    }
                }
                if !changed_keys.is_empty() {
                    changed_keys.sort();
                    let mut prev_snapshot = IndexMap::new();
                    let mut value_snapshot = IndexMap::new();
                    for key in &changed_keys {
                        prev_snapshot.insert(key.clone(), st.previous_tags.get(key).cloned());
                        value_snapshot.insert(key.clone(), st.next_tags.get(key).cloned());
                    }
                    st.previous.tags = Some(prev_snapshot);
                    st.value.tags = Some(value_snapshot);
                    metadata.tags = if st.next_tags.is_empty() {
                        None
                    } else {
                        Some(st.next_tags.clone())
                    };
                }
            }
            let changed = st.previous.display_name.is_some() || st.previous.tags.is_some();
            if !changed {
                return false;
            }
            if let Some(dn) = &metadata.display_name
                && let Err(e) = validate_display_name(dn)
            {
                st.error = Some(e);
                return false;
            }
            if let Some(tags) = &metadata.tags
                && tags.keys().any(String::is_empty)
            {
                st.error = Some("Resulting metadata contains an empty tag key.".to_string());
                return false;
            }
            true
        },
        &MutateOptions::default(),
        |_published| {
            let st = state.borrow();
            let ev = match event {
                MetadataPatchEvent::MetadataChange => {
                    Event::metadata_change(id, st.previous.clone(), st.value.clone())
                }
                MetadataPatchEvent::DisplayNameChange => Event::display_name_change(
                    id,
                    st.previous.display_name.clone().flatten(),
                    st.value.display_name.clone().flatten(),
                ),
                MetadataPatchEvent::TagsChange => {
                    Event::tags_change(id, st.previous_tags.clone(), st.next_tags.clone())
                }
            };
            let _ = append_event_locked(id, &ev);
        },
        deadline.saturating_duration_since(Instant::now()),
    );

    let mutation_error = state.into_inner().error;
    if let Some(e) = mutation_error {
        return Err(e);
    }
    match result {
        MutateStatus::Busy => Err(metadata_busy_message(id)),
        MutateStatus::Missing => Err(format!("Session id \"{id}\" not found.")),
        MutateStatus::Stale | MutateStatus::GenerationMismatch => Err(format!(
            "Session id \"{id}\" metadata changed during the operation. Retry it."
        )),
        MutateStatus::Unchanged(metadata) => Ok(MetadataPatchResult {
            changed: false,
            metadata,
        }),
        MutateStatus::Changed(metadata) => Ok(MetadataPatchResult {
            changed: true,
            metadata,
        }),
    }
}

/// Atomically merge presentation metadata for one exact stable id (no
/// displayName fallback), emitting `metadata_change` with only the touched
/// keys. A live creation lock is waited out before taking the event lock, so
/// daemon publication events retain their own bounded lock budget. Genuinely
/// unknown ids still fail fast.
///
/// node: src/sessions.ts `patchMetadataById`
pub fn patch_metadata_by_id(
    id: &str,
    patch: &MetadataPatch,
) -> Result<MetadataPatchResult, String> {
    patch.validate()?;
    let path = super::root::metadata_path(id);
    let deadline = Instant::now() + METADATA_PATCH_LOCK_WAIT;
    while is_creation_lock_held(id) {
        let now = Instant::now();
        if now >= deadline {
            return Err(metadata_busy_message(id));
        }
        std::thread::sleep(Duration::from_millis(25).min(deadline - now));
    }
    if !path.is_file() {
        return Err(format!("Session id \"{id}\" not found."));
    }
    apply_metadata_patch_by_id_with_wait(
        id,
        patch,
        MetadataPatchEvent::MetadataChange,
        deadline.saturating_duration_since(Instant::now()),
    )
}

/// Set or clear (`None` or `""`) the display name, emitting
/// `display_name_change` only when the value actually changed.
///
/// node: src/sessions.ts:573-579
pub fn set_display_name(
    name: &str,
    display_name: Option<&str>,
) -> Result<MetadataPatchResult, String> {
    let patch = MetadataPatch {
        display_name: Some(display_name.filter(|s| !s.is_empty()).map(str::to_string)),
        tags: None,
    };
    apply_metadata_patch_by_id(name, &patch, MetadataPatchEvent::DisplayNameChange)
}

/// Merge `updates` then apply `removals`, emitting `tags_change` with the
/// full previous and next maps only when the effective tags changed.
///
/// node: src/sessions.ts:585-593
pub fn update_tags(
    name: &str,
    updates: &TagMap,
    removals: &[String],
) -> Result<MetadataPatchResult, String> {
    let mut tags: IndexMap<String, Option<String>> = updates
        .iter()
        .map(|(k, v)| (k.clone(), Some(v.clone())))
        .collect();
    for key in removals {
        tags.insert(key.clone(), None);
    }
    let patch = MetadataPatch {
        display_name: None,
        tags: Some(tags),
    };
    apply_metadata_patch_by_id(name, &patch, MetadataPatchEvent::TagsChange)
}
