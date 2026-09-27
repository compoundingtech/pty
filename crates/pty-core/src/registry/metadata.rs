//! `<name>.json`: the on-disk `SessionMetadata` record, byte-compatible with
//! Node's `writeMetadata` (pretty JSON, two-space indent, Node's key order
//! on publication, unknown fields preserved on rewrite).
//!
//! node: src/sessions.ts:137-181, 292-319, 595-602; src/server.ts:655-673

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::atomic::atomic_write;
use super::root::{ensure_session_dir, metadata_path};

/// An insertion-ordered string map, the shape of `tags`, `extraEnv` and
/// `env` (JavaScript object key order is what Node writes and reads).
pub type TagMap = IndexMap<String, String>;

/// Alias of [`TagMap`] for the environment maps.
pub type EnvMap = IndexMap<String, String>;

/// `lastLines` keeps at most this many rows (`SESSION_EXIT_LAST_LINES_LIMIT`).
pub const SESSION_EXIT_LAST_LINES_LIMIT: usize = 200;

/// On-disk metadata for a session. `extra` catches every field this version
/// does not know so a rewrite preserves it.
///
/// Field order here is the order a JavaScript mutation would append new
/// keys in (`displayName` before `tags`, exit fields last); the daemon's
/// publication order is [`SessionMetadata::publication_map`].
///
/// node: src/sessions.ts:137-181
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetadata {
    /// Opaque daemon-generation token (32 hex). Legacy records lack it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
    /// Pid of the daemon that owns this generation. Accepted for liveness
    /// only when `recovery.processStartToken` still proves the process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_pid: Option<i32>,
    /// Start identity of the Rust daemon that published this generation.
    /// Node generations carry the equivalent token in
    /// `recovery.processStartToken`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_start_token: Option<String>,
    /// Recovery capability advertised by Node daemons. Opaque here: preserved
    /// verbatim on rewrite, never produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<Value>,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// The command as the user typed it.
    #[serde(default)]
    pub display_command: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral: Option<bool>,
    /// ISO-8601 with milliseconds and `Z`.
    #[serde(default)]
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<TagMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolate_env: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_env: Option<EnvMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unset_env: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<EnvMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exited_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_lines: Option<Vec<String>>,
    /// Stamped by the daemon on every non-readonly ATTACH.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attach_at: Option<String>,
    /// Unix milliseconds for the newest child output the daemon has seen.
    /// Absent until a session produces output, and absent on a record an
    /// older daemon wrote — never zero, and never a claim that the session
    /// is idle.
    ///
    /// A Rust daemon writes this field into the record only at exit. While
    /// the child runs, the stamp lives in the `.activity/<name>.json`
    /// sidecar ([`super::activity`]), so that output alone never rewrites
    /// this file; read it through [`super::activity::last_output_at_ms`].
    /// A Node daemon still writes it here at most once a second
    /// (docs/decisions/0015).
    ///
    /// node: src/sessions.ts (`lastOutputAtMs`), the Node pty repository's
    /// `docs/vrs/requirements.md` R14 (not this repository's `docs/vrs`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_output_at_ms: Option<i64>,
    /// A counter the Rust daemon bumps, in the same write, whenever the
    /// client facts `pty stats` reports change: a writable client attaches
    /// or leaves, a writable client's size changes, or the session's
    /// negotiated size changes. It starts again with every daemon generation,
    /// so compare it only together with `generation`. Absent until the first
    /// such change, and never written by a Node daemon. Readonly (PEEK) and
    /// command connections do not bump it: observing a session must not
    /// change its record (docs/decisions/0015).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_generation: Option<u64>,
    /// Every field this version does not model, round-tripped verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl SessionMetadata {
    /// Has the daemon recorded an exit (`exitedAt` present)?
    pub fn has_exited(&self) -> bool {
        self.exited_at.is_some()
    }

    /// `recovery.processStartToken`, when the record carries one.
    pub fn process_start_token(&self) -> Option<&str> {
        self.recovery.as_ref()?.get("processStartToken")?.as_str()
    }

    /// Exact daemon start identity, independent of which runtime published
    /// the record.
    pub fn daemon_start_token(&self) -> Option<&str> {
        self.daemon_start_token
            .as_deref()
            .or_else(|| self.process_start_token())
    }

    /// The record as a JSON object in this struct's key order, `None` fields
    /// omitted (for diffing and comparison; see `publication_map` for the
    /// on-disk publication order).
    pub fn to_map(&self) -> Map<String, Value> {
        match serde_json::to_value(self) {
            Ok(Value::Object(map)) => map,
            _ => Map::new(),
        }
    }

    /// Parse a JSON object into a record (unknown fields land in `extra`).
    pub fn from_map(map: Map<String, Value>) -> Option<Self> {
        serde_json::from_value(Value::Object(map)).ok()
    }

    /// The Node publication order with one optional Rust identity field after
    /// `daemonPid`: `generation, daemonPid, daemonStartToken?, recovery?,
    /// command, args, displayCommand, cwd, rows, cols, ephemeral, createdAt,
    /// tags?, displayName?, isolateEnv?, extraEnv?, unsetEnv?, env?`.
    /// `ephemeral` is always written (`=== true`); `tags`/`extraEnv`/
    /// `unsetEnv` only when non-empty; `isolateEnv` only when true. Anything
    /// in `extra` follows.
    ///
    /// node: src/server.ts:655-673
    pub fn publication_map(&self) -> Map<String, Value> {
        let mut m = Map::new();
        if let Some(g) = &self.generation {
            m.insert("generation".into(), Value::from(g.as_str()));
        }
        if let Some(pid) = self.daemon_pid {
            m.insert("daemonPid".into(), Value::from(pid));
        }
        if let Some(token) = &self.daemon_start_token {
            m.insert("daemonStartToken".into(), Value::from(token.as_str()));
        }
        if let Some(r) = &self.recovery {
            m.insert("recovery".into(), r.clone());
        }
        m.insert("command".into(), Value::from(self.command.as_str()));
        m.insert("args".into(), Value::from(self.args.clone()));
        m.insert(
            "displayCommand".into(),
            Value::from(self.display_command.as_str()),
        );
        m.insert("cwd".into(), Value::from(self.cwd.as_str()));
        if let Some(rows) = self.rows {
            m.insert("rows".into(), Value::from(rows));
        }
        if let Some(cols) = self.cols {
            m.insert("cols".into(), Value::from(cols));
        }
        m.insert(
            "ephemeral".into(),
            Value::from(self.ephemeral == Some(true)),
        );
        m.insert("createdAt".into(), Value::from(self.created_at.as_str()));
        if let Some(tags) = &self.tags
            && !tags.is_empty()
        {
            m.insert("tags".into(), string_map_value(tags));
        }
        if let Some(dn) = &self.display_name
            && !dn.is_empty()
        {
            m.insert("displayName".into(), Value::from(dn.as_str()));
        }
        if self.isolate_env == Some(true) {
            m.insert("isolateEnv".into(), Value::Bool(true));
        }
        if let Some(extra_env) = &self.extra_env
            && !extra_env.is_empty()
        {
            m.insert("extraEnv".into(), string_map_value(extra_env));
        }
        if let Some(unset) = &self.unset_env
            && !unset.is_empty()
        {
            m.insert("unsetEnv".into(), Value::from(unset.clone()));
        }
        if let Some(env) = &self.env {
            m.insert("env".into(), string_map_value(env));
        }
        for key in [
            "exitCode",
            "exitedAt",
            "lastLines",
            "lastAttachAt",
            "lastOutputAtMs",
            "clientGeneration",
        ] {
            if let Some(v) = self.to_map().get(key) {
                m.insert(key.into(), v.clone());
            }
        }
        for (k, v) in &self.extra {
            m.insert(k.clone(), v.clone());
        }
        m
    }
}

fn string_map_value(map: &IndexMap<String, String>) -> Value {
    Value::Object(
        map.iter()
            .map(|(k, v)| (k.clone(), Value::from(v.as_str())))
            .collect(),
    )
}

/// `JSON.stringify(value, null, 2)`.
pub fn pretty_json(map: &Map<String, Value>) -> String {
    serde_json::to_string_pretty(&Value::Object(map.clone())).unwrap_or_default()
}

/// Read `<name>.json` as a raw JSON object, `None` when missing, unreadable
/// or not an object.
pub fn read_metadata_map(name: &str) -> Option<Map<String, Value>> {
    read_metadata_map_at(&metadata_path(name))
}

fn read_metadata_map_at(path: &std::path::Path) -> Option<Map<String, Value>> {
    let bytes = std::fs::read(path).ok()?;
    match serde_json::from_slice::<Value>(&bytes).ok()? {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

/// Read session metadata from an already-resolved registry path.
pub(crate) fn read_metadata_at(path: &std::path::Path) -> Option<SessionMetadata> {
    SessionMetadata::from_map(read_metadata_map_at(path)?)
}

/// Read session metadata, `None` when it does not exist or cannot be parsed.
///
/// node: src/sessions.ts:595-602
pub fn read_metadata(name: &str) -> Option<SessionMetadata> {
    read_metadata_at(&metadata_path(name))
}

/// Write a raw metadata object atomically as pretty JSON.
pub fn write_metadata_map(name: &str, map: &Map<String, Value>) -> std::io::Result<()> {
    ensure_session_dir()?;
    atomic_write(&metadata_path(name), pretty_json(map).as_bytes())
}

/// Write session metadata atomically in Node's publication key order (see
/// [`SessionMetadata::publication_map`]). This is the daemon's first write;
/// use [`super::mutate::mutate_metadata_under_lock`] for read-modify-write
/// so a Node-written file keeps its key order and unknown fields.
///
/// node: src/sessions.ts:292-319; src/server.ts:655-673
pub fn write_metadata(name: &str, meta: &SessionMetadata) -> std::io::Result<()> {
    write_metadata_publication(name, meta)
}

/// The daemon's first write: Node's publication key order and presence
/// rules (see [`SessionMetadata::publication_map`]).
pub fn write_metadata_publication(name: &str, meta: &SessionMetadata) -> std::io::Result<()> {
    write_metadata_map(name, &meta.publication_map())
}

/// Apply the difference between `before` and `after` onto `map`, the way a
/// JavaScript mutation of the parsed object would: changed or added keys
/// are set in place (an existing key keeps its position, a new one is
/// appended), keys that became `None` are deleted, untouched keys — known
/// or unknown — are left exactly where they were.
pub fn apply_metadata_diff(
    before: &SessionMetadata,
    after: &SessionMetadata,
    map: &mut Map<String, Value>,
) {
    let before = before.to_map();
    let after = after.to_map();
    for (k, v) in &after {
        if before.get(k) != Some(v) {
            map.insert(k.clone(), v.clone());
        }
    }
    for k in before.keys() {
        if !after.contains_key(k) {
            map.shift_remove(k);
        }
    }
}
