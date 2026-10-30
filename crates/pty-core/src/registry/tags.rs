//! Tag rules shared by the CLI, the daemon and gc: the reserved-key rule,
//! `--filter-tag` matching, the `keep` tag, exit-time reap precedence, and
//! the gc bookkeeping keys a manual restart strips.
//!
//! node: src/tags.ts; src/sessions.ts:1020-1109; src/cli.ts:4081-4100

use super::metadata::{SessionMetadata, TagMap};
use super::time::parse_iso8601_ms;

/// Keys pty itself treats as bookkeeping and hides from the default
/// listing (`pty list --tags` shows them).
pub const EXACT_RESERVED_TAG_KEYS: &[&str] =
    &["ptyfile", "ptyfile.session", "ptyfile.tags", "strategy"];

/// Reserved: one of [`EXACT_RESERVED_TAG_KEYS`], or any key starting with
/// `:` (tool-owned tags such as pty-layout's `:l<pid>-<rand>`).
///
/// node: src/tags.ts:56-79
pub fn is_reserved_tag_key(key: &str) -> bool {
    EXACT_RESERVED_TAG_KEYS.contains(&key) || key.starts_with(':')
}

/// `true` when `session_tags` contains every `key=value` of `filter` (AND).
/// An empty filter always matches; a session without tags matches only an
/// empty filter.
///
/// node: src/tags.ts:38-46
pub fn matches_all_tags(session_tags: Option<&TagMap>, filter: &TagMap) -> bool {
    filter
        .iter()
        .all(|(k, v)| session_tags.and_then(|t| t.get(k)) == Some(v))
}

/// Pull every `--filter-tag key=value` pair out of `args` (consumed in
/// place, repeatable). Errors with Node's text when the value is missing or
/// has no `=`.
///
/// node: src/tags.ts:17-31
pub fn extract_filter_tags(args: &mut Vec<String>) -> Result<TagMap, String> {
    let mut tags = TagMap::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] != "--filter-tag" {
            i += 1;
            continue;
        }
        let kv = args.get(i + 1).cloned();
        let Some(kv) = kv.filter(|kv| kv.contains('=')) else {
            return Err("--filter-tag expects \"key=value\"".to_string());
        };
        let (k, v) = kv.split_once('=').expect("checked");
        tags.insert(k.to_string(), v.to_string());
        args.drain(i..i + 2);
    }
    Ok(tags)
}

/// Tag key that exempts a session from the daemon's exit-time self-reap
/// unconditionally, and from `pty gc`'s sweep for a bounded window
/// ([`DEFAULT_KEEP_MAX_AGE_MS`]).
///
/// node: src/sessions.ts:1020
pub const KEEP_TAG: &str = "keep";

/// Values that read as "no" for `keep` and `PTY_REAP_ON_EXIT`.
pub const KEEP_FALSEY: &[&str] = &["false", "0", "no", "off"];

fn reads_as_no(raw: &str) -> bool {
    let normalized = super::names::js_trim(raw).to_lowercase();
    KEEP_FALSEY.contains(&normalized.as_str())
}

/// `true` when `tags` asks for the session to be retained after death: the
/// `keep` key is present with any value other than `false|0|no|off` (after
/// trim + lowercase).
///
/// node: src/sessions.ts:1040-1044
pub fn is_keep_requested(tags: Option<&TagMap>) -> bool {
    match tags.and_then(|t| t.get(KEEP_TAG)) {
        None => false,
        Some(raw) => !reads_as_no(raw),
    }
}

/// How long `keep` holds a dead session against `pty gc`'s sweep, unless
/// the operator overrides it with `pty gc --keep-max-age <dur>`. Seven days
/// is long enough that "I killed it Friday, I'll look Monday" still works,
/// and short enough that a fleet of agents tagging every session cannot
/// grow the registry without bound.
///
/// node: src/sessions.ts:1084 (`DEFAULT_KEEP_MAX_AGE_MS`)
pub const DEFAULT_KEEP_MAX_AGE_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Has a dead `keep`-tagged session outlived its retention window?
///
/// Age is anchored on `exitedAt` when the daemon wrote an exit record, else
/// `createdAt` (a `vanished` session never wrote one) — the same anchor
/// precedence `pty list --older-than` uses. Metadata carrying neither, or an
/// unparseable timestamp, has no age and therefore never expires: retaining
/// an unaged record is the recoverable failure, deleting it is not.
///
/// `max_age_ms <= 0` expires everything, including unaged records — that is
/// the explicit "sweep the keep backlog now" request, not an inference from
/// a timestamp. Callers must apply this to dead sessions only; a running
/// session is never a sweep candidate regardless of its age.
///
/// node: src/sessions.ts:1098-1109 (`isKeepExpired`)
pub fn is_keep_expired(metadata: Option<&SessionMetadata>, now_ms: i64, max_age_ms: i64) -> bool {
    if max_age_ms <= 0 {
        return true;
    }
    let Some(meta) = metadata else {
        return false;
    };
    let anchor = meta
        .exited_at
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(meta.created_at.as_str());
    match parse_iso8601_ms(anchor) {
        Some(ts) => now_ms - ts >= max_age_ms,
        None => false,
    }
}

/// The config default for exit-time reaping: `PTY_REAP_ON_EXIT` unset →
/// reap; `false|0|no|off` → preserve; anything else → reap.
///
/// node: src/sessions.ts:1091-1097
pub fn reap_on_exit_default() -> bool {
    match std::env::var("PTY_REAP_ON_EXIT") {
        Err(_) => true,
        Ok(raw) => !reads_as_no(&raw),
    }
}

/// Should the daemon remove its own registry entry as it shuts down?
/// Precedence, highest first: `keep` → preserve; `ephemeral` → reap;
/// `strategy=permanent` → preserve; else `default_reap`.
///
/// node: src/sessions.ts:1069-1089
pub fn should_reap_at_exit(tags: Option<&TagMap>, ephemeral: bool, default_reap: bool) -> bool {
    if is_keep_requested(tags) {
        return false;
    }
    if ephemeral {
        return true;
    }
    if tags.and_then(|t| t.get("strategy")).map(String::as_str) == Some("permanent") {
        return false;
    }
    default_reap
}

/// Bookkeeping pty writes on a dead record: `pty gc`'s flapping keys and the
/// retention sweep's [`RETENTION_LOST_AT_TAG`]. A manual `restart` / `up`
/// strips them.
///
/// node: src/cli.ts:3667-3672, 4081-4100
pub const GC_BOOKKEEPING_KEYS: &[&str] = &[
    "strategy.status",
    "strategy.consecutive-fast-fails",
    "strategy.last-respawn-at",
    "strategy.command-hash",
    RETENTION_LOST_AT_TAG,
];

/// When the retention sweep first saw a `vanished` record's owner lost.
///
/// A vanished daemon writes no exit record, so this stamp is the earliest
/// time of death pty knows, and retention counts a vanished record's age
/// from it. A stamp older than the record's `createdAt` belongs to an
/// earlier run of the same id and does not count.
pub const RETENTION_LOST_AT_TAG: &str = "retention.lost-at";

/// How long daemons keep finished (`exited` / `vanished`) records before
/// their retention sweep removes them. Unset means off.
pub const RETAIN_FINISHED_ENV: &str = "PTY_RETAIN_FINISHED";

/// The retention window in milliseconds, or `None` when retention is off.
///
/// Off is the default: unset, empty, `false|0|no|off`, and any value that
/// is not a positive `Ns|Nm|Nh|Nd` duration. A value that cannot be read
/// never turns into "remove now".
pub fn retain_finished_window() -> Option<i64> {
    parse_retain_finished(std::env::var(RETAIN_FINISHED_ENV).ok().as_deref())
}

/// [`retain_finished_window`] for one raw value.
pub fn parse_retain_finished(raw: Option<&str>) -> Option<i64> {
    let raw = raw.filter(|raw| !reads_as_no(raw))?;
    crate::duration::parse_duration(raw).filter(|ms| *ms > 0)
}

/// Remove the [`GC_BOOKKEEPING_KEYS`] from `tags`; `None` when nothing is
/// left (so callers skip the `tags` field entirely, as Node does).
///
/// node: src/cli.ts:4088-4100
pub fn strip_gc_bookkeeping(tags: Option<&TagMap>) -> Option<TagMap> {
    let tags = tags?;
    if tags.is_empty() {
        return Some(tags.clone());
    }
    let out: TagMap = tags
        .iter()
        .filter(|(k, _)| !GC_BOOKKEEPING_KEYS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    (!out.is_empty()).then_some(out)
}
