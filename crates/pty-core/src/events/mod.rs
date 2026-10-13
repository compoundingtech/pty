//! `<name>.events.jsonl`: the append-only event log, compatible with Node's
//! `src/events.ts` in envelope, type names, payloads, retention and lock
//! protocol.
//!
//! Envelope: `{"session", "type", "ts", ...payload}` with `ts` an ISO-8601
//! timestamp (`Date.prototype.toISOString`). Retention: at or past 1000
//! lines the last 500 are kept. Every complete snapshot is published with a
//! sibling temporary file + rename so lock-free raw readers cannot see a
//! partial append or retention rewrite.
//!
//! node: src/events.ts

pub mod follow;

use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::registry::atomic::atomic_write;
use crate::registry::lock::{EVENT_LOCK_WAIT, LockGuard, take_event_lock, wait_for_event_lock};
use crate::registry::metadata::TagMap;
use crate::registry::mutate::MetadataChangeSnapshot;
use crate::registry::root::{ensure_session_dir, events_path};
use crate::registry::time::{local_hms, now_iso8601, parse_iso8601_ms};

pub use follow::{EventFollower, FollowerOptions};

/// The system event type names.
///
/// node: src/events.ts:10-22
pub mod event_type {
    pub const BELL: &str = "bell";
    pub const TITLE_CHANGE: &str = "title_change";
    pub const NOTIFICATION: &str = "notification";
    pub const FOCUS_REQUEST: &str = "focus_request";
    pub const CURSOR_VISIBLE: &str = "cursor_visible";
    pub const SESSION_START: &str = "session_start";
    pub const SESSION_EXIT: &str = "session_exit";
    pub const SESSION_EXEC: &str = "session_exec";
    pub const SESSION_RESPAWN: &str = "session_respawn";
    /// Written by a daemon that could not kill everything it started.
    /// **This tool writes it and the Node tool does not** (decision 0008); a
    /// Node reader renders it through the same fallback it uses for `user.*`.
    pub const SESSION_DESCENDANTS_SURVIVED: &str = "session_descendants_survived";
    pub const SESSION_ABANDONED: &str = "session_abandoned";
    pub const SESSION_FLAPPING: &str = "session_flapping";
    pub const DISPLAY_NAME_CHANGE: &str = "display_name_change";
    pub const TAGS_CHANGE: &str = "tags_change";
    pub const METADATA_CHANGE: &str = "metadata_change";
    /// Prefix of user-published events.
    pub const USER_PREFIX: &str = "user.";

    /// Every system type, for docs and validation.
    pub const ALL: &[&str] = &[
        BELL,
        TITLE_CHANGE,
        NOTIFICATION,
        FOCUS_REQUEST,
        CURSOR_VISIBLE,
        SESSION_START,
        SESSION_EXIT,
        SESSION_EXEC,
        SESSION_RESPAWN,
        SESSION_ABANDONED,
        SESSION_FLAPPING,
        DISPLAY_NAME_CHANGE,
        TAGS_CHANGE,
        METADATA_CHANGE,
        // Written here and not by the Node tool (decision 0008).
        SESSION_DESCENDANTS_SURVIVED,
    ];
}

/// The `source` of a `notification` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationSource {
    Osc9,
    Osc99,
    Osc777,
}

impl NotificationSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            NotificationSource::Osc9 => "osc9",
            NotificationSource::Osc99 => "osc99",
            NotificationSource::Osc777 => "osc777",
        }
    }
}

/// Why `pty gc` abandoned a permanent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbandonReason {
    CwdGone,
    Idle,
}

impl AbandonReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            AbandonReason::CwdGone => "cwd-gone",
            AbandonReason::Idle => "idle",
        }
    }
}

fn ts_from_string_or_number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    match Value::deserialize(d)? {
        Value::String(s) => Ok(s),
        Value::Number(n) => Ok(n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .map(crate::registry::time::iso8601_from_epoch_ms)
            .unwrap_or_else(|| n.to_string())),
        other => Ok(other.to_string()),
    }
}

/// One event record. Serializes flat: `session`, `type`, `ts`, then every
/// payload field in insertion order.
///
/// node: src/events.ts:32-40
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub session: String,
    #[serde(rename = "type")]
    pub r#type: String,
    #[serde(deserialize_with = "ts_from_string_or_number")]
    pub ts: String,
    #[serde(flatten)]
    pub payload: Map<String, Value>,
}

impl Event {
    /// An event of `type` for `session` stamped now, with no payload.
    pub fn new(session: &str, r#type: &str) -> Self {
        Event {
            session: session.to_string(),
            r#type: r#type.to_string(),
            ts: now_iso8601(),
            payload: Map::new(),
        }
    }

    /// Replace the timestamp.
    pub fn with_ts(mut self, ts: &str) -> Self {
        self.ts = ts.to_string();
        self
    }

    /// Append a payload field.
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.payload.insert(key.to_string(), value.into());
        self
    }

    /// A payload field.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.payload.get(key)
    }

    /// A payload field as a string.
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.payload.get(key).and_then(Value::as_str)
    }

    /// Is this a `user.*` event?
    pub fn is_user_event(&self) -> bool {
        self.r#type.starts_with(event_type::USER_PREFIX)
    }

    /// The record as one JSON line (no trailing newline).
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn bell(session: &str) -> Self {
        Event::new(session, event_type::BELL)
    }

    pub fn title_change(session: &str, value: &str) -> Self {
        Event::new(session, event_type::TITLE_CHANGE).with("value", value)
    }

    /// `notification {title?, body?, source?}`; absent parts are omitted.
    pub fn notification(
        session: &str,
        title: Option<&str>,
        body: Option<&str>,
        source: Option<NotificationSource>,
    ) -> Self {
        let mut e = Event::new(session, event_type::NOTIFICATION);
        if let Some(t) = title {
            e = e.with("title", t);
        }
        if let Some(b) = body {
            e = e.with("body", b);
        }
        if let Some(s) = source {
            e = e.with("source", s.as_str());
        }
        e
    }

    pub fn focus_request(session: &str) -> Self {
        Event::new(session, event_type::FOCUS_REQUEST)
    }

    pub fn cursor_visible(session: &str) -> Self {
        Event::new(session, event_type::CURSOR_VISIBLE)
    }

    /// `session_start {tags?}`; `tags` only when non-empty.
    ///
    /// node: src/server.ts:674-676
    pub fn session_start(session: &str, tags: Option<&TagMap>) -> Self {
        let mut e = Event::new(session, event_type::SESSION_START);
        if let Some(tags) = tags
            && !tags.is_empty()
        {
            e = e.with("tags", tag_map_value(tags));
        }
        e
    }

    /// `session_exit {exitCode, signal?}`; a signal death carries
    /// `exitCode = 128 + signal` and `signal`.
    ///
    /// node: src/server.ts:581-584
    pub fn session_exit(session: &str, exit_code: i32, signal: Option<i32>) -> Self {
        let mut e = Event::new(session, event_type::SESSION_EXIT).with("exitCode", exit_code);
        if let Some(sig) = signal.filter(|s| *s != 0) {
            e = e.with("signal", sig);
        }
        e
    }

    /// `session_descendants_survived {data:{pids}}` — processes still alive
    /// after the daemon signalled them with TERM and then KILL.
    ///
    /// **The daemon has always known this and had nowhere to put it.** It
    /// warned on its own standard error, which has no reader for almost all
    /// of a daemon's life, so the one moment it had something worth saying
    /// was the one moment nobody was listening. `pty kill` reports on the
    /// daemon and cannot see a surviving child, so without this the fact
    /// existed and reached no one.
    pub fn session_descendants_survived(session: &str, pids: &[i32]) -> Self {
        Event::new(session, event_type::SESSION_DESCENDANTS_SURVIVED)
            .with("data", serde_json::json!({ "pids": pids }))
    }

    pub fn session_exec(session: &str, previous_command: &str, command: &str) -> Self {
        Event::new(session, event_type::SESSION_EXEC)
            .with("previousCommand", previous_command)
            .with("command", command)
    }

    pub fn session_respawn(session: &str) -> Self {
        Event::new(session, event_type::SESSION_RESPAWN)
    }

    /// `session_abandoned {reason, idleDays?}`.
    pub fn session_abandoned(session: &str, reason: AbandonReason, idle_days: Option<u64>) -> Self {
        let mut e =
            Event::new(session, event_type::SESSION_ABANDONED).with("reason", reason.as_str());
        if let Some(days) = idle_days {
            e = e.with("idleDays", days);
        }
        e
    }

    pub fn session_flapping(session: &str, counter: u64, limit: u64, window: u64) -> Self {
        Event::new(session, event_type::SESSION_FLAPPING)
            .with("counter", counter)
            .with("limit", limit)
            .with("window", window)
    }

    /// `user.<suffix> {data?, text?}`. The type is not validated here; use
    /// [`emit_user_event`] or [`validate_user_event_type`].
    pub fn user(session: &str, r#type: &str, data: Option<Value>, text: Option<&str>) -> Self {
        let mut e = Event::new(session, r#type);
        if let Some(d) = data {
            e = e.with("data", d);
        }
        if let Some(t) = text {
            e = e.with("text", t);
        }
        e
    }

    /// `display_name_change {previous, value}` (`null` when absent).
    pub fn display_name_change(
        session: &str,
        previous: Option<String>,
        value: Option<String>,
    ) -> Self {
        Event::new(session, event_type::DISPLAY_NAME_CHANGE)
            .with("previous", previous.map(Value::from).unwrap_or(Value::Null))
            .with("value", value.map(Value::from).unwrap_or(Value::Null))
    }

    /// `tags_change {previous, value}` with full maps.
    pub fn tags_change(session: &str, previous: TagMap, value: TagMap) -> Self {
        Event::new(session, event_type::TAGS_CHANGE)
            .with("previous", tag_map_value(&previous))
            .with("value", tag_map_value(&value))
    }

    /// `metadata_change {previous, value}` with only the touched keys.
    pub fn metadata_change(
        session: &str,
        previous: MetadataChangeSnapshot,
        value: MetadataChangeSnapshot,
    ) -> Self {
        Event::new(session, event_type::METADATA_CHANGE)
            .with(
                "previous",
                serde_json::to_value(previous).unwrap_or(Value::Null),
            )
            .with("value", serde_json::to_value(value).unwrap_or(Value::Null))
    }
}

fn tag_map_value(tags: &IndexMap<String, String>) -> Value {
    Value::Object(
        tags.iter()
            .map(|(k, v)| (k.clone(), Value::from(v.as_str())))
            .collect(),
    )
}

/// Validate a user-emitted event type. `None` when valid, else the message
/// the CLI and the client helper both print.
///
/// node: src/events.ts:201-217
pub fn validate_user_event_type(r#type: &str) -> Option<String> {
    if r#type.is_empty() {
        return Some("event type must be a non-empty string".to_string());
    }
    if !r#type.starts_with(event_type::USER_PREFIX) {
        return Some(format!(
            "custom events must start with \"user.\" (got {})",
            serde_json::to_string(r#type).unwrap_or_default()
        ));
    }
    if r#type == event_type::USER_PREFIX {
        return Some("event type \"user.\" needs a suffix (e.g. \"user.build-done\")".to_string());
    }
    if r#type
        .chars()
        .any(|c| crate::registry::names::is_js_whitespace(c) || (c as u32) < 0x20)
    {
        return Some("event type may not contain whitespace or control characters".to_string());
    }
    None
}

/// Retention: at or past this many lines the log is rewritten…
pub const MAX_LINES: usize = 1000;
/// …keeping only the newest this many.
pub const KEEP_LINES: usize = 500;
/// The daemon writer counts lines instead of stat-ing: check every N appends.
pub const TRUNCATE_CHECK_INTERVAL: usize = 100;
/// One-shot writers skip the line count while the file is smaller than this.
pub const TRUNCATE_SIZE_THRESHOLD: u64 = (MAX_LINES as u64) * 40;
/// `read_recent_events` default.
pub const DEFAULT_RECENT_EVENTS: usize = 50;

/// The newest [`KEEP_LINES`] as a byte slice when retention is due.
fn retained_suffix(content: &[u8]) -> Option<&[u8]> {
    let text = std::str::from_utf8(content).ok()?;
    let lines: Vec<&str> = text.trim_end().split('\n').collect();
    if lines.len() < MAX_LINES {
        return None;
    }
    let first = lines[lines.len() - KEEP_LINES];
    let start = first.as_ptr() as usize - text.as_ptr() as usize;
    Some(&content[start..])
}

/// Publish one appended record by replacing the whole bounded log. The event
/// lock serializes writers; the sibling temporary file + rename gives raw
/// readers one complete JSONL snapshot instead of exposing a growing last
/// record.
///
/// Retention is applied to the in-memory snapshot before publication. Even
/// when the cap is crossed, one append performs exactly one log read and one
/// atomic write rather than publishing an unbounded intermediate or rewriting
/// twice.
fn append_line(
    path: &Path,
    event: &Event,
    should_check_retention: impl FnOnce(u64) -> bool,
) -> std::io::Result<()> {
    let mut content = match std::fs::read(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    let line = event.to_json();
    content.reserve(line.len() + 1);
    content.extend_from_slice(line.as_bytes());
    content.push(b'\n');
    let size = u64::try_from(content.len()).unwrap_or(u64::MAX);
    let published = if should_check_retention(size) {
        retained_suffix(&content).unwrap_or(&content)
    } else {
        &content
    };
    atomic_write(path, published)
}

/// Apply retention at each original record boundary. Publish each shrink
/// separately so offset-based followers can recognize a retention rewrite.
fn append_batch(path: &Path, events: &[Event], append_count: &mut usize) -> std::io::Result<()> {
    let mut content = match std::fs::read(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    let mut published_len = content.len();
    let mut dirty = false;
    for event in events {
        content.extend_from_slice(event.to_json().as_bytes());
        content.push(b'\n');
        dirty = true;
        *append_count += 1;
        if *append_count >= TRUNCATE_CHECK_INTERVAL {
            if let Some(suffix) = retained_suffix(&content) {
                let start = content.len() - suffix.len();
                let retained_len = content.len() - start;
                if retained_len >= published_len {
                    // Large records can make even the retained suffix larger
                    // than the old log. Establish a full snapshot to shrink.
                    publish_batch_snapshot(path, &content)?;
                }
                content.drain(..start);
                publish_batch_snapshot(path, &content)?;
                published_len = content.len();
                dirty = false;
            }
            *append_count = 0;
        }
    }
    if dirty {
        publish_batch_snapshot(path, &content)?;
    }
    Ok(())
}

fn publish_batch_snapshot(path: &Path, content: &[u8]) -> std::io::Result<()> {
    #[cfg(test)]
    BATCH_WRITES.with(|count| count.set(count.get() + 1));
    atomic_write(path, content)
}

#[cfg(test)]
thread_local! {
    static BATCH_WRITES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Append while the caller owns the session's event lock. Publication
/// atomically replaces the previous snapshot and applies the one-shot
/// size-gated retention check in the same replacement.
///
/// node: src/events.ts:277-310
pub fn append_event_locked(name: &str, event: &Event) -> std::io::Result<()> {
    ensure_session_dir()?;
    let path = events_path(name);
    append_line(&path, event, |size| size >= TRUNCATE_SIZE_THRESHOLD)
}

/// One-shot append that fails immediately when the event lock is held
/// (`Session id "<name>" event log is busy. Retry the operation.`).
///
/// node: src/events.ts:286-295
pub fn append_event_sync(name: &str, event: &Event) -> Result<(), String> {
    let _lock = take_event_lock(name)?;
    append_event_locked(name, event).map_err(|e| e.to_string())
}

/// One-shot append that waits up to 5 s for the event lock (the async
/// writer path `pty emit` uses).
///
/// node: src/events.ts:257-265
pub fn append_event(name: &str, event: &Event) -> Result<(), String> {
    ensure_session_dir().map_err(|e| e.to_string())?;
    let _lock: LockGuard = wait_for_event_lock(name, EVENT_LOCK_WAIT)?;
    append_event_locked(name, event).map_err(|e| e.to_string())
}

/// Validate and append a `user.*` event stamped now.
///
/// node: src/events.ts:330-349
pub fn emit_user_event(
    name: &str,
    r#type: &str,
    data: Option<Value>,
    text: Option<&str>,
) -> Result<Event, String> {
    if let Some(err) = validate_user_event_type(r#type) {
        return Err(err);
    }
    let event = Event::user(name, r#type, data, text);
    append_event(name, &event)?;
    Ok(event)
}

/// Truncate the log to empty (creating it), as the daemon does at start.
/// Replacement is atomic so a raw reader already walking the old log can
/// finish that complete snapshot.
///
/// node: src/events.ts:392-402
pub fn clear_events(name: &str) -> Result<(), String> {
    ensure_session_dir().map_err(|e| e.to_string())?;
    let _lock = take_event_lock(name)?;
    let _ = atomic_write(&events_path(name), b"");
    Ok(())
}

/// Unlink the log under the event lock.
///
/// node: src/events.ts:404-413
pub fn remove_events(name: &str) -> Result<(), String> {
    let _lock = take_event_lock(name)?;
    let _ = std::fs::remove_file(events_path(name));
    Ok(())
}

/// The newest `count` events. Empty when the file is missing or any inspected
/// line fails to parse (as Node's `readRecentEvents`).
///
/// node: src/events.ts:415-423
pub fn read_recent_events(name: &str, count: usize) -> Vec<Event> {
    read_recent_events_at(&events_path(name), count, None)
}

/// Every retained event whose type exactly matches `event_type`.
///
/// The retained file is itself bounded by the writer's retention policy.
pub fn read_retained_events_of_type(name: &str, event_type: &str) -> Vec<Event> {
    read_recent_events_at(&events_path(name), usize::MAX, Some(event_type))
}

/// The newest `count` events from `root/<name>.events.jsonl`.
pub fn read_recent_events_in(root: &Path, name: &str, count: usize) -> Vec<Event> {
    read_recent_events_at(&root.join(format!("{name}.events.jsonl")), count, None)
}

fn read_recent_events_at(path: &Path, count: usize, event_type: Option<&str>) -> Vec<Event> {
    if count == 0 {
        return Vec::new();
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(count.min(content.lines().count()));
    for line in content.trim_end().split('\n').filter(|line| !line.is_empty()).rev() {
        let event = match serde_json::from_str::<Event>(line) {
            Ok(event) => event,
            Err(_) => return Vec::new(),
        };
        if event_type.is_none_or(|expected| event.r#type == expected) {
            out.push(event);
            if out.len() == count {
                break;
            }
        }
    }
    out.reverse();
    out
}

/// Every parseable event in the log, in order (a parse failure skips that
/// line rather than emptying the result).
pub fn read_all_events(name: &str) -> Vec<Event> {
    read_all_events_at(&events_path(name))
}

/// Every parseable event from `root/<name>.events.jsonl`, in order.
pub fn read_all_events_in(root: &Path, name: &str) -> Vec<Event> {
    read_all_events_at(&root.join(format!("{name}.events.jsonl")))
}

fn read_all_events_at(path: &Path) -> Vec<Event> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str::<Event>(l).ok())
        .collect()
}

enum WriterMsg {
    Append(Event),
    Flush(Sender<()>),
}

/// The daemon's serialized background writer publishes records in 250 ms
/// batches, holding the cross-process event lock once per batch. Retention
/// is checked at the same record boundaries as the one-record writer.
/// Exit records, explicit flushes and shutdown publish pending records.
/// Errors are dropped, as Node's promise chain drops them.
///
/// node: src/events.ts:352-380
pub struct EventWriter {
    tx: Option<Sender<WriterMsg>>,
    handle: Option<JoinHandle<()>>,
}

impl EventWriter {
    pub fn new(name: &str) -> Self {
        let (tx, rx) = channel::<WriterMsg>();
        let name = name.to_string();
        let handle = std::thread::Builder::new()
            .name(format!("events-{name}"))
            .spawn(move || Self::run(&name, rx))
            .ok();
        EventWriter {
            tx: Some(tx),
            handle,
        }
    }

    fn run(name: &str, rx: Receiver<WriterMsg>) {
        let mut append_count = 0usize;
        let mut pending = Vec::new();
        let mut deadline: Option<Instant> = None;
        loop {
            if deadline.is_some_and(|until| Instant::now() >= until) {
                Self::publish(name, &mut pending, &mut append_count);
                deadline = None;
            }
            let msg = if let Some(until) = deadline {
                rx.recv_timeout(until.saturating_duration_since(Instant::now()))
            } else {
                rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
            };
            match msg {
                Ok(WriterMsg::Append(event)) => {
                    let exiting = event.r#type == event_type::SESSION_EXIT;
                    pending.push(event);
                    deadline.get_or_insert_with(|| Instant::now() + Duration::from_millis(250));
                    if exiting {
                        Self::publish(name, &mut pending, &mut append_count);
                        deadline = None;
                    }
                }
                Ok(WriterMsg::Flush(reply)) => {
                    Self::publish(name, &mut pending, &mut append_count);
                    deadline = None;
                    let _ = reply.send(());
                }
                Err(RecvTimeoutError::Timeout) => {
                    Self::publish(name, &mut pending, &mut append_count);
                    deadline = None;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    Self::publish(name, &mut pending, &mut append_count);
                    break;
                }
            }
        }
    }

    fn publish(name: &str, pending: &mut Vec<Event>, append_count: &mut usize) {
        if pending.is_empty() {
            return;
        }
        if let Ok(_lock) = wait_for_event_lock(name, EVENT_LOCK_WAIT) {
            let mut next_count = *append_count;
            if append_batch(&events_path(name), pending, &mut next_count).is_ok() {
                *append_count = next_count;
            }
        }
        pending.clear();
    }

    /// Queue an event; returns immediately.
    pub fn append(&self, event: Event) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(WriterMsg::Append(event));
        }
    }

    /// Block until every queued event has been written (or given up on).
    pub fn flush(&self) {
        let Some(tx) = &self.tx else {
            return;
        };
        let (reply_tx, reply_rx) = channel();
        if tx.send(WriterMsg::Flush(reply_tx)).is_ok() {
            let _ = reply_rx.recv();
        }
    }

    /// Flush, then stop the writer thread.
    pub fn close(mut self) {
        self.flush();
        self.tx = None;
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for EventWriter {
    fn drop(&mut self) {
        self.flush();
        self.tx = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// JavaScript template-literal rendering of a JSON value (`${v}`).
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(items) => items.iter().map(js_string).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// JavaScript truthiness of a JSON value.
fn js_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

fn js_stringify(v: Option<&Value>) -> String {
    match v {
        None => "undefined".to_string(),
        Some(v) => serde_json::to_string(v).unwrap_or_default(),
    }
}

fn kv_listing(v: Option<&Value>) -> String {
    match v {
        Some(Value::Object(map)) if !map.is_empty() => map
            .iter()
            .map(|(k, v)| format!("{k}={}", js_string(v)))
            .collect::<Vec<_>>()
            .join(" "),
        _ => "{}".to_string(),
    }
}

/// The one-line text `pty events` prints: `[HH:MM:SS] <session>: <body>`
/// with the time in the local zone.
///
/// node: src/events.ts:548-604
pub fn format_event(event: &Event) -> String {
    let time = parse_iso8601_ms(&event.ts)
        .map(local_hms)
        .unwrap_or_else(|| "Invalid Date".to_string());
    let prefix = format!("[{time}] {}:", event.session);
    let p = &event.payload;
    match event.r#type.as_str() {
        event_type::BELL => format!("{prefix} bell"),
        event_type::TITLE_CHANGE => format!(
            "{prefix} title -> \"{}\"",
            p.get("value")
                .map(js_string)
                .unwrap_or_else(|| "undefined".to_string())
        ),
        event_type::NOTIFICATION => {
            let mut parts = vec![prefix, "notification".to_string()];
            if js_truthy(p.get("title")) {
                parts.push(format!("-- \"{}\"", js_string(&p["title"])));
            }
            if js_truthy(p.get("body")) {
                parts.push(js_string(&p["body"]));
            }
            parts.join(" ")
        }
        event_type::FOCUS_REQUEST => format!("{prefix} focus requested"),
        event_type::CURSOR_VISIBLE => format!("{prefix} cursor restored"),
        event_type::SESSION_START => {
            let tag_str = match p.get("tags") {
                Some(Value::Object(tags)) if js_truthy(p.get("tags")) => {
                    let listing = tags
                        .iter()
                        .map(|(k, v)| format!("{k}={}", js_string(v)))
                        .collect::<Vec<_>>()
                        .join(" ");
                    format!(" {listing}")
                }
                _ => String::new(),
            };
            format!("{prefix} started{tag_str}")
        }
        event_type::SESSION_EXIT => {
            let code = p
                .get("exitCode")
                .map(js_string)
                .unwrap_or_else(|| "undefined".to_string());
            if js_truthy(p.get("signal")) {
                format!(
                    "{prefix} killed by signal {} (code {code})",
                    js_string(&p["signal"])
                )
            } else {
                format!("{prefix} exited (code {code})")
            }
        }
        event_type::SESSION_EXEC => format!(
            "{prefix} exec {} (was {})",
            p.get("command")
                .map(js_string)
                .unwrap_or_else(|| "undefined".to_string()),
            p.get("previousCommand")
                .map(js_string)
                .unwrap_or_else(|| "undefined".to_string())
        ),
        event_type::SESSION_RESPAWN => format!("{prefix} respawned"),
        event_type::SESSION_ABANDONED => {
            let reason = p
                .get("reason")
                .map(js_string)
                .unwrap_or_else(|| "undefined".to_string());
            match p.get("idleDays") {
                Some(days) if reason == "idle" => {
                    format!("{prefix} abandoned (idle {}d)", js_string(days))
                }
                _ => format!("{prefix} abandoned ({reason})"),
            }
        }
        event_type::DISPLAY_NAME_CHANGE => format!(
            "{prefix} display_name -> {} (was {})",
            js_stringify(p.get("value")),
            js_stringify(p.get("previous"))
        ),
        event_type::TAGS_CHANGE => format!(
            "{prefix} tags -> {} (was {})",
            kv_listing(p.get("value")),
            kv_listing(p.get("previous"))
        ),
        event_type::METADATA_CHANGE => format!(
            "{prefix} metadata -> {} (was {})",
            js_stringify(p.get("value")),
            js_stringify(p.get("previous"))
        ),
        other => {
            let suffix = match (p.get("text"), p.get("data")) {
                (Some(text), _) if !text.is_null() => format!(" \"{}\"", js_string(text)),
                (_, Some(data)) => format!(" {}", serde_json::to_string(data).unwrap_or_default()),
                _ => String::new(),
            };
            format!("{prefix} {other}{suffix}")
        }
    }
}

#[cfg(test)]
mod batch_tests {
    use super::*;

    struct TestRoot(std::path::PathBuf);

    impl TestRoot {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("pty-batch-{}-{label}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn batch_reduces_replacements_and_preserves_bytes_and_retention() {
        let root = TestRoot::new("retention");
        crate::registry::root::with_root(root.path(), || {
            let events: Vec<_> = (0..2150)
                .map(|i| Event::bell("batch").with_ts(&format!("timestamp-{i}")))
                .collect();
            let reference = root.path().join("reference");
            for (index, event) in events.iter().enumerate() {
                append_line(&reference, event, |_| {
                    (index + 1) % TRUNCATE_CHECK_INTERVAL == 0
                })
                .unwrap();
            }
            BATCH_WRITES.with(|writes| writes.set(0));
            append_batch(&events_path("batch"), &events, &mut 0).unwrap();
            BATCH_WRITES.with(|writes| assert_eq!(writes.get(), 7));
            assert_eq!(
                std::fs::read(events_path("batch")).unwrap(),
                std::fs::read(reference).unwrap()
            );
            let (tx, rx) = channel();
            for _ in 0..80 {
                tx.send(WriterMsg::Append(Event::bell("short"))).unwrap();
            }
            drop(tx);
            BATCH_WRITES.with(|writes| writes.set(0));
            EventWriter::run("short", rx);
            BATCH_WRITES.with(|writes| assert_eq!(writes.get(), 1));
            assert_eq!(read_all_events("short").len(), 80);
        });
    }

    #[test]
    fn exit_and_flush_publish_separate_batches_before_acknowledgement() {
        let root = TestRoot::new("exit");
        crate::registry::root::with_root(root.path(), || {
            let (tx, rx) = channel();
            tx.send(WriterMsg::Append(Event::bell("exit"))).unwrap();
            tx.send(WriterMsg::Append(Event::session_exit("exit", 0, None)))
                .unwrap();
            tx.send(WriterMsg::Append(Event::bell("exit"))).unwrap();
            let (reply, ack) = channel();
            tx.send(WriterMsg::Flush(reply)).unwrap();
            drop(tx);
            BATCH_WRITES.with(|writes| writes.set(0));
            EventWriter::run("exit", rx);
            ack.recv().unwrap();
            BATCH_WRITES.with(|writes| assert_eq!(writes.get(), 2));
            assert_eq!(read_all_events("exit").len(), 3);
        });
    }
}
