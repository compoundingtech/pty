//! `pty list` / `pty ls`: the registry as text (with Node's SGR codes) or
//! JSON, with tag / status / age filters, `--summary`, and the `--remote`
//! host groups.
//!
//! node: src/cli.ts:1250-1316 (parsing), 2165-2446 (`cmdList`),
//! 4102-4130 (`strategyMarker`, `shortPath`, `timeAgo`)

use std::collections::HashSet;

use pty_client as client;
use pty_client::list::{ClientQuery, ClientSet, attached_clients};
use pty_client::summary::{LineStyle, SessionSummary};
use pty_core::duration::{format_duration, parse_duration};
use pty_core::registry::{
    self, SessionInfo, SessionStatus, TagMap, extract_filter_tags, matches_all_tags, now_epoch_ms,
    parse_iso8601_ms, time_ago,
};
use serde_json::{Map, Value};

use super::{CliError, CliResult};

/// One remote host group as `--remote` renders it.
#[derive(Debug, Clone, Default)]
pub struct RemoteHost {
    pub label: String,
    pub sessions: Vec<RemoteSession>,
    pub error: Option<String>,
}

/// One session as a remote host reports it.
#[derive(Debug, Clone, Default)]
pub struct RemoteSession {
    pub name: String,
    pub status: String,
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub tags: Option<TagMap>,
    pub display_name: Option<String>,
}

/// The remote host groups for `--remote [<peer>]`: a named peer is dialed
/// over fabric; bare `--remote` asks `pty-relay ls --json` for the peers to
/// dial and is best-effort, so a relay that is absent yields no groups.
///
/// A dial that fails is a group carrying the error, not a failure of the
/// command: `pty list --remote <peer>` still exits 0 and still lists the
/// local sessions.
///
/// node: src/cli.ts:2223-2247
pub fn remote_list_hosts(peer: Option<&str>) -> Vec<RemoteHost> {
    match peer {
        Some(peer) => vec![host_group(peer)],
        None => relay_peers().iter().map(|p| host_group(p)).collect(),
    }
}

/// One host group: dial the peer, ask it for its sessions, and record
/// whatever went wrong instead of propagating it.
fn host_group(peer: &str) -> RemoteHost {
    let dialer = client::remote::RemoteDialer::default();
    let sessions = dialer
        .dial(peer)
        .and_then(|socket_path| dialer.fetch_remote_list(&socket_path));
    match sessions {
        Ok(rows) => RemoteHost {
            label: peer.to_string(),
            sessions: rows.into_iter().map(remote_session).collect(),
            error: None,
        },
        Err(e) => RemoteHost {
            label: peer.to_string(),
            sessions: Vec::new(),
            error: Some(e.to_string()),
        },
    }
}

fn remote_session(row: client::remote::RemoteSessionRow) -> RemoteSession {
    RemoteSession {
        name: row.name,
        status: row.status,
        command: row.command,
        cwd: row.cwd,
        tags: row.tags.map(|t| t.into_iter().collect()),
        display_name: row.display_name,
    }
}

/// `pty-relay ls --json` names the peers worth dialing. Node ignores every
/// failure here, so a machine with no relay simply has no remote groups.
///
/// node: src/cli.ts:2223-2233
fn relay_peers() -> Vec<String> {
    let Ok(out) = std::process::Command::new("pty-relay")
        .args(["ls", "--json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_slice::<Value>(&out.stdout) else {
        return Vec::new();
    };
    let rows = value
        .get("peers")
        .and_then(Value::as_array)
        .or_else(|| value.as_array());
    rows.map(|rows| {
        rows.iter()
            .filter_map(|r| {
                r.as_str()
                    .map(str::to_string)
                    .or_else(|| r.get("name").and_then(Value::as_str).map(str::to_string))
            })
            .collect()
    })
    .unwrap_or_default()
}

/// Everything `cmdList` takes.
#[derive(Debug, Clone, Default)]
pub struct ListOptions {
    pub json: bool,
    pub show_tags: bool,
    pub remote: bool,
    pub remote_peer: Option<String>,
    pub filter_tags: TagMap,
    pub status_filter: Option<SessionStatus>,
    pub older_than_ms: Option<i64>,
    pub newer_than_ms: Option<i64>,
    pub summary: bool,
    /// `--clients`: query each running daemon for its attached clients.
    /// JSON list only; the text and summary views ignore it.
    pub clients: bool,
}

/// Parse `args` (the full argv from the command word on, as Node's
/// `listArgs = args.slice()`) and list.
///
/// node: src/cli.ts:1250-1316
pub fn run(args: &[String]) -> CliResult {
    let mut list_args = args.to_vec();
    let filter_tags = extract_filter_tags(&mut list_args).map_err(CliError)?;

    let mut opts = ListOptions {
        filter_tags,
        ..Default::default()
    };
    let mut consumed: HashSet<usize> = HashSet::new();
    let mut i = 1;
    while i < list_args.len() {
        let arg = list_args[i].as_str();
        let val = list_args.get(i + 1).map(String::as_str);
        match arg {
            "--status" => {
                opts.status_filter = Some(match val {
                    Some("running") => SessionStatus::Running,
                    Some("exited") => SessionStatus::Exited,
                    Some("vanished") => SessionStatus::Vanished,
                    _ => {
                        return Err(CliError(
                            "--status expects one of: running, exited, vanished".to_string(),
                        ));
                    }
                });
                consumed.insert(i);
                consumed.insert(i + 1);
                i += 1;
            }
            "--older-than" | "--newer-than" => {
                let Some(parsed) = val.and_then(parse_duration) else {
                    return Err(CliError(format!(
                        "{arg} expects a duration like 30s, 5m, 2h, 1d"
                    )));
                };
                if arg == "--older-than" {
                    opts.older_than_ms = Some(parsed);
                } else {
                    opts.newer_than_ms = Some(parsed);
                }
                consumed.insert(i);
                consumed.insert(i + 1);
                i += 1;
            }
            "--remote" => {
                opts.remote = true;
                match val {
                    Some(peer) if !peer.starts_with('-') => {
                        opts.remote_peer = Some(peer.to_string());
                        consumed.insert(i);
                        consumed.insert(i + 1);
                        i += 1;
                    }
                    _ => {
                        consumed.insert(i);
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    let remaining: Vec<&str> = list_args
        .iter()
        .enumerate()
        .filter(|(idx, _)| !consumed.contains(idx))
        .map(|(_, a)| a.as_str())
        .collect();
    opts.json = remaining.contains(&"--json");
    opts.show_tags = remaining.contains(&"--tags");
    opts.summary = remaining.contains(&"--summary");
    opts.clients = remaining.contains(&"--clients");
    cmd_list(&opts)
}

/// `displayName ?? name`.
fn sort_key(s: &SessionInfo) -> &str {
    s.metadata
        .as_ref()
        .and_then(|m| m.display_name.as_deref())
        .unwrap_or(&s.name)
}

/// The truthy display name.
fn display_name(s: &SessionInfo) -> Option<&str> {
    s.display_name().filter(|d| !d.is_empty())
}

/// Non-empty `createdAt`.
fn created_at(s: &SessionInfo) -> Option<&str> {
    s.metadata
        .as_ref()
        .map(|m| m.created_at.as_str())
        .filter(|c| !c.is_empty())
}

/// The filtered, sorted sessions.
fn select(opts: &ListOptions) -> Vec<SessionInfo> {
    let mut sessions = registry::list_sessions();
    if !opts.filter_tags.is_empty() {
        sessions.retain(|s| {
            matches_all_tags(
                s.metadata.as_ref().and_then(|m| m.tags.as_ref()),
                &opts.filter_tags,
            )
        });
    }
    if let Some(status) = opts.status_filter {
        sessions.retain(|s| s.status == status);
    }
    if opts.older_than_ms.is_some() || opts.newer_than_ms.is_some() {
        let now = now_epoch_ms();
        sessions.retain(|s| {
            // Anchor age on exitedAt when available (true exit), else
            // createdAt. Sessions with no timestamp cannot be aged, so they
            // are excluded when either age flag is set.
            let anchor = s
                .metadata
                .as_ref()
                .and_then(|m| m.exited_at.as_deref())
                .or_else(|| created_at(s));
            let Some(anchor) = anchor else {
                return false;
            };
            // An unparseable timestamp is NaN in Node: every comparison is
            // false, so the session stays.
            let Some(then) = parse_iso8601_ms(anchor) else {
                return true;
            };
            let age = now - then;
            if let Some(older) = opts.older_than_ms
                && age < older
            {
                return false;
            }
            if let Some(newer) = opts.newer_than_ms
                && age > newer
            {
                return false;
            }
            true
        });
    }
    // Stable display order: ASCII sort on the user-visible label.
    sessions.sort_by(|a, b| sort_key(a).cmp(sort_key(b)));
    sessions
}

struct Summary {
    total: usize,
    running: usize,
    exited: usize,
    vanished: usize,
    oldest: Option<Endpoint>,
    newest: Option<Endpoint>,
}

struct Endpoint {
    name: String,
    status: SessionStatus,
    age_seconds: i64,
    display_name: Option<String>,
}

impl Endpoint {
    fn json(&self) -> Value {
        let mut m = Map::new();
        m.insert("name".into(), Value::from(self.name.as_str()));
        m.insert("status".into(), Value::from(self.status.as_str()));
        m.insert("ageSeconds".into(), Value::from(self.age_seconds));
        if let Some(dn) = &self.display_name {
            m.insert("displayName".into(), Value::from(dn.as_str()));
        }
        Value::Object(m)
    }

    fn label(&self) -> String {
        match &self.display_name {
            Some(dn) => format!("{dn} ({})", self.name),
            None => self.name.clone(),
        }
    }
}

/// Oldest/newest anchored on `createdAt` only.
///
/// node: src/cli.ts:2252-2285
fn build_summary(sessions: &[SessionInfo]) -> Summary {
    let mut running = 0;
    let mut exited = 0;
    let mut vanished = 0;
    for s in sessions {
        match s.status {
            SessionStatus::Running => running += 1,
            SessionStatus::Exited => exited += 1,
            SessionStatus::Vanished => vanished += 1,
        }
    }
    let mut oldest: Option<(&SessionInfo, i64)> = None;
    let mut newest: Option<(&SessionInfo, i64)> = None;
    for s in sessions {
        let Some(ts) = created_at(s).and_then(parse_iso8601_ms) else {
            continue;
        };
        if oldest.is_none_or(|(_, t)| ts < t) {
            oldest = Some((s, ts));
        }
        if newest.is_none_or(|(_, t)| ts > t) {
            newest = Some((s, ts));
        }
    }
    let now = now_epoch_ms();
    let endpoint = |pick: Option<(&SessionInfo, i64)>| {
        pick.map(|(s, ts)| Endpoint {
            name: s.name.clone(),
            status: s.status,
            age_seconds: ((now - ts).div_euclid(1000)).max(0),
            display_name: display_name(s).map(str::to_string),
        })
    };
    Summary {
        total: sessions.len(),
        running,
        exited,
        vanished,
        oldest: endpoint(oldest),
        newest: endpoint(newest),
    }
}

/// One `list --json` element, keys in Node's order. `clients` is `None`
/// unless `--clients` asked for it; an unknown set renders as `null`.
///
/// node: src/cli.ts:2292-2306
fn session_json(s: &SessionInfo, clients: Option<&ClientSet>) -> Value {
    let meta = s.metadata.as_ref();
    let mut m = Map::new();
    m.insert("name".into(), Value::from(s.name.as_str()));
    m.insert("status".into(), Value::from(s.status.as_str()));
    m.insert("pid".into(), s.pid.map(Value::from).unwrap_or(Value::Null));
    m.insert(
        "command".into(),
        meta.map(|m| Value::from(m.display_command.as_str()))
            .unwrap_or(Value::Null),
    );
    m.insert(
        "cwd".into(),
        meta.map(|m| Value::from(m.cwd.as_str())).unwrap_or(Value::Null),
    );
    m.insert(
        "createdAt".into(),
        meta.map(|m| Value::from(m.created_at.as_str()))
            .unwrap_or(Value::Null),
    );
    m.insert(
        "exitCode".into(),
        meta.and_then(|m| m.exit_code)
            .map(Value::from)
            .unwrap_or(Value::Null),
    );
    m.insert(
        "exitedAt".into(),
        meta.and_then(|m| m.exited_at.as_deref())
            .map(Value::from)
            .unwrap_or(Value::Null),
    );
    if let Some(clients) = clients
        && s.is_running()
    {
        m.insert(
            "clients".into(),
            serde_json::to_value(clients).expect("a client set is serializable"),
        );
    }
    if let Some(tags) = meta.and_then(|m| m.tags.as_ref()) {
        m.insert("tags".into(), tag_map_json(tags));
    }
    if let Some(dn) = display_name(s) {
        m.insert("displayName".into(), Value::from(dn));
    }
    Value::Object(m)
}

fn tag_map_json(tags: &TagMap) -> Value {
    Value::Object(
        tags.iter()
            .map(|(k, v)| (k.clone(), Value::from(v.as_str())))
            .collect(),
    )
}

fn remote_host_json(h: &RemoteHost) -> Value {
    let mut m = Map::new();
    m.insert("label".into(), Value::from(h.label.as_str()));
    m.insert(
        "sessions".into(),
        Value::Array(
            h.sessions
                .iter()
                .map(|s| {
                    let mut o = Map::new();
                    o.insert("name".into(), Value::from(s.name.as_str()));
                    o.insert("status".into(), Value::from(s.status.as_str()));
                    if let Some(c) = &s.command {
                        o.insert("command".into(), Value::from(c.as_str()));
                    }
                    if let Some(c) = &s.cwd {
                        o.insert("cwd".into(), Value::from(c.as_str()));
                    }
                    if let Some(t) = &s.tags {
                        o.insert("tags".into(), tag_map_json(t));
                    }
                    if let Some(d) = &s.display_name {
                        o.insert("displayName".into(), Value::from(d.as_str()));
                    }
                    Value::Object(o)
                })
                .collect(),
        ),
    );
    m.insert(
        "error".into(),
        h.error
            .as_deref()
            .map(Value::from)
            .unwrap_or(Value::Null),
    );
    Value::Object(m)
}

/// `cmdList`.
///
/// node: src/cli.ts:2165-2446
pub fn cmd_list(opts: &ListOptions) -> CliResult {
    let sessions = select(opts);
    let remote_hosts = if opts.remote_peer.is_some() || opts.remote {
        remote_list_hosts(opts.remote_peer.as_deref())
    } else {
        Vec::new()
    };

    if opts.json {
        if opts.summary {
            let s = build_summary(&sessions);
            let mut by_status = Map::new();
            by_status.insert("running".into(), Value::from(s.running));
            by_status.insert("exited".into(), Value::from(s.exited));
            by_status.insert("vanished".into(), Value::from(s.vanished));
            let mut m = Map::new();
            m.insert("total".into(), Value::from(s.total));
            m.insert("byStatus".into(), Value::Object(by_status));
            m.insert(
                "oldest".into(),
                s.oldest.as_ref().map(Endpoint::json).unwrap_or(Value::Null),
            );
            m.insert(
                "newest".into(),
                s.newest.as_ref().map(Endpoint::json).unwrap_or(Value::Null),
            );
            println!("{}", Value::Object(m));
            return Ok(0);
        }
        let clients = if opts.clients {
            attached_clients(&sessions, &ClientQuery::default())
        } else {
            Vec::new()
        };
        let local = Value::Array(
            sessions
                .iter()
                .enumerate()
                .map(|(i, s)| session_json(s, clients.get(i)))
                .collect(),
        );
        if opts.remote && !remote_hosts.is_empty() {
            let mut m = Map::new();
            m.insert("local".into(), local);
            m.insert(
                "remote".into(),
                Value::Array(remote_hosts.iter().map(remote_host_json).collect()),
            );
            println!("{}", Value::Object(m));
        } else {
            println!("{local}");
        }
        return Ok(0);
    }

    if opts.summary {
        let s = build_summary(&sessions);
        if s.total == 0 {
            println!("No matching sessions.");
            return Ok(0);
        }
        let mut parts: Vec<String> = Vec::new();
        if s.running > 0 {
            parts.push(format!("{} running", s.running));
        }
        if s.exited > 0 {
            parts.push(format!("{} exited", s.exited));
        }
        if s.vanished > 0 {
            parts.push(format!("{} vanished", s.vanished));
        }
        println!(
            "{} session{} — {}",
            s.total,
            if s.total == 1 { "" } else { "s" },
            parts.join(", ")
        );
        if let Some(oldest) = &s.oldest {
            println!(
                "oldest: {} ({}, {})",
                oldest.label(),
                oldest.status,
                format_duration(oldest.age_seconds * 1000)
            );
        }
        if let Some(newest) = &s.newest
            && s.oldest.as_ref().is_none_or(|o| o.name != newest.name)
        {
            println!(
                "newest: {} ({}, {})",
                newest.label(),
                newest.status,
                format_duration(newest.age_seconds * 1000)
            );
        }
        return Ok(0);
    }

    if sessions.is_empty() && remote_hosts.is_empty() {
        println!("No active sessions.");
        return Ok(0);
    }

    let running: Vec<&SessionInfo> = sessions.iter().filter(|s| s.is_running()).collect();
    let exited: Vec<&SessionInfo> = sessions
        .iter()
        .filter(|s| s.status == SessionStatus::Exited)
        .collect();
    let vanished: Vec<&SessionInfo> = sessions
        .iter()
        .filter(|s| s.status == SessionStatus::Vanished)
        .collect();

    if !running.is_empty() {
        println!("Active sessions:");
        for s in &running {
            let mut summary = SessionSummary::from_info(s);
            summary.command.get_or_insert_with(|| "unknown".to_string());
            let pid = s
                .pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "null".to_string());
            let status = format!(" (pid: {pid})");
            println!(
                "{}",
                summary.line(&line_style(opts, "  ", "\x1b[1;36m", &status))
            );
        }
    }

    if !exited.is_empty() {
        if !running.is_empty() {
            println!();
        }
        println!("Exited sessions:");
        for s in &exited {
            let meta = s.metadata.as_ref();
            let code = meta
                .and_then(|m| m.exit_code)
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".to_string());
            let ago = meta
                .and_then(|m| m.exited_at.as_deref())
                .map(time_ago)
                .unwrap_or_else(|| "unknown".to_string());
            let status = format!(" (exited with code {code}, {ago})");
            println!(
                "{}",
                SessionSummary::from_info(s).line(&line_style(opts, "  ", "\x1b[1m", &status))
            );
        }
    }

    if !vanished.is_empty() {
        if !running.is_empty() || !exited.is_empty() {
            println!();
        }
        println!("\x1b[33mVanished sessions (no exit record — killed or crashed):\x1b[0m");
        for s in &vanished {
            let ago = created_at(s)
                .map(time_ago)
                .unwrap_or_else(|| "unknown".to_string());
            let status = format!(" (vanished, started {ago})");
            println!(
                "{}",
                SessionSummary::from_info(s).line(&line_style(
                    opts,
                    "  \u{26a0} ",
                    "\x1b[1;33m",
                    &status
                ))
            );
        }
    }

    for host in &remote_hosts {
        println!();
        if let Some(err) = &host.error {
            println!("\x1b[1m{}\x1b[0m \x1b[31m(error: {err})\x1b[0m", host.label);
            continue;
        }
        println!("\x1b[1m{}\x1b[0m ({} sessions):", host.label, host.sessions.len());
        let mut sorted: Vec<&RemoteSession> = host.sessions.iter().collect();
        sorted.sort_by(|a, b| {
            let ka = a.display_name.as_deref().unwrap_or(&a.name);
            let kb = b.display_name.as_deref().unwrap_or(&b.name);
            ka.cmp(kb)
        });
        for s in sorted {
            let icon = if s.status == "running" {
                "  \u{25cf} "
            } else {
                "  \u{25cb} "
            };
            let summary = SessionSummary {
                id: s.name.clone(),
                display_name: s.display_name.clone().filter(|d| !d.is_empty()),
                cwd: s.cwd.clone().unwrap_or_default(),
                command: s.command.clone(),
                tags: s.tags.clone(),
                ..Default::default()
            };
            println!(
                "{}",
                summary.line(&line_style(opts, icon, "\x1b[1;36m", ""))
            );
        }
    }
    Ok(0)
}

fn line_style<'a>(
    opts: &ListOptions,
    prefix: &'a str,
    bold: &'a str,
    status: &'a str,
) -> LineStyle<'a> {
    LineStyle {
        prefix,
        bold,
        status,
        show_all_tags: opts.show_tags,
    }
}

fn tags_of(s: &SessionInfo) -> Option<&TagMap> {
    s.metadata.as_ref().and_then(|m| m.tags.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pty_core::protocol::AttachedClient;

    fn session(status: SessionStatus) -> SessionInfo {
        SessionInfo {
            name: "test".into(),
            socket_path: "/nonexistent/test.sock".into(),
            pid: Some(42),
            status,
            metadata: None,
        }
    }

    #[test]
    fn json_clients_shape() {
        let known = ClientSet::Known(vec![AttachedClient {
            pid: Some(123),
            tty: Some("/dev/pts/3".into()),
            attached_at: "2026-09-25T12:00:00.000Z".into(),
        }]);
        let running = session(SessionStatus::Running);

        let json = session_json(&running, Some(&known));
        assert_eq!(
            json["clients"],
            serde_json::json!([{"pid": 123, "tty": "/dev/pts/3", "attachedAt": "2026-09-25T12:00:00.000Z"}])
        );
        assert_eq!(
            session_json(&running, Some(&ClientSet::Known(Vec::new())))["clients"],
            serde_json::json!([])
        );
        let unknown = session_json(&running, Some(&ClientSet::Unknown));
        assert!(unknown.get("clients").is_some_and(Value::is_null), "{unknown}");
        // Not requested: no key, exactly as without --clients.
        assert!(session_json(&running, None).get("clients").is_none());
        for status in [SessionStatus::Exited, SessionStatus::Vanished] {
            assert!(session_json(&session(status), Some(&known)).get("clients").is_none());
        }
    }
}
