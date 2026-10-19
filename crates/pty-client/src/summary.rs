//! One rendering of "which session is this" for `pty list` and for the lines a
//! client prints when an attach or `peek -f` ends: a header naming the event,
//! the session's `pty list` line, and the command that gets you back.

use pty_core::duration::format_duration;
use pty_core::registry::{
    self, SessionInfo, SessionMetadata, TagMap, is_reserved_tag_key, now_epoch_ms,
    parse_iso8601_ms, short_path,
};

use super::remote::RemoteSessionRow;

/// What a client knows about a session beyond its id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSummary {
    /// The stable id (`pty attach` takes it).
    pub id: String,
    /// Non-empty display name.
    pub display_name: Option<String>,
    /// Absolute cwd; empty when unknown.
    pub cwd: String,
    /// The command as the user typed it.
    pub command: Option<String>,
    pub tags: Option<TagMap>,
    /// ISO-8601 `createdAt`.
    pub created_at: Option<String>,
    pub ephemeral: bool,
}

impl SessionSummary {
    pub fn from_metadata(id: &str, meta: &SessionMetadata) -> Self {
        SessionSummary {
            id: id.to_string(),
            display_name: meta.display_name.clone().filter(|d| !d.is_empty()),
            cwd: meta.cwd.clone(),
            command: Some(meta.display_command.clone()),
            tags: meta.tags.clone(),
            created_at: Some(meta.created_at.clone()).filter(|c| !c.is_empty()),
            ephemeral: meta.ephemeral.unwrap_or(false),
        }
    }

    /// A registry entry; a socket-only entry knows its id and nothing else.
    pub fn from_info(info: &SessionInfo) -> Self {
        match &info.metadata {
            Some(meta) => Self::from_metadata(&info.name, meta),
            None => SessionSummary {
                id: info.name.clone(),
                ..Default::default()
            },
        }
    }

    /// A row from a remote host's `list`.
    pub fn from_remote_row(row: &RemoteSessionRow) -> Self {
        SessionSummary {
            id: row.name.clone(),
            display_name: row.display_name.clone().filter(|d| !d.is_empty()),
            cwd: row.cwd.clone().unwrap_or_default(),
            command: row.command.clone(),
            tags: row
                .tags
                .as_ref()
                .map(|t| t.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
            created_at: None,
            ephemeral: false,
        }
    }

    /// `<prefix><label><marker><tags><status> — <cwd> — <dim command>`, the
    /// shape of every `pty list` line.
    pub fn line(&self, style: &LineStyle) -> String {
        let cwd = if self.cwd.is_empty() {
            String::new()
        } else {
            short_path(&self.cwd)
        };
        format!(
            "{}{}{}{}{} — {cwd} — \x1b[2m{}\x1b[0m",
            style.prefix,
            render_label(self.display_name.as_deref(), &self.id, style.bold),
            strategy_marker(self.tags.as_ref()),
            render_tags(self.tags.as_ref(), style.show_all_tags),
            style.status,
            self.command.as_deref().unwrap_or(""),
        )
    }

    /// `<display name> (<id>)` or `<id>`, without styling.
    pub fn plain_label(&self) -> String {
        match &self.display_name {
            Some(dn) => format!("{dn} ({})", self.id),
            None => self.id.clone(),
        }
    }

    /// Whether the registry entry outlives the session's exit, so `pty attach`
    /// can still offer to restart it.
    fn kept_at_exit(&self) -> bool {
        !registry::should_reap_at_exit(
            self.tags.as_ref(),
            self.ephemeral,
            registry::reap_on_exit_default(),
        )
    }

    /// How long the session has run, when its start is known.
    fn age(&self, now_ms: i64) -> Option<String> {
        let start = parse_iso8601_ms(self.created_at.as_deref()?)?;
        Some(format_duration(now_ms - start))
    }
}

/// How [`SessionSummary::line`] decorates the line.
#[derive(Debug, Clone, Copy)]
pub struct LineStyle<'a> {
    /// Before the label (indent, status icon).
    pub prefix: &'a str,
    /// SGR opening the label.
    pub bold: &'a str,
    /// After the tags, e.g. ` (pid: 42)`.
    pub status: &'a str,
    /// Show reserved tag keys too (`pty list --tags`).
    pub show_all_tags: bool,
}

/// The line under a trailer header.
const TRAILER_LINE: LineStyle<'static> = LineStyle {
    prefix: "  ",
    bold: "\x1b[1m",
    status: "",
    show_all_tags: false,
};

/// Tags as hashtags; reserved keys hidden unless `show_all`.
///
/// node: src/cli.ts:2340-2344
pub fn render_tags(tags: Option<&TagMap>, show_all: bool) -> String {
    let Some(tags) = tags else {
        return String::new();
    };
    let entries: Vec<String> = tags
        .iter()
        .filter(|(k, _)| show_all || !is_reserved_tag_key(k))
        .map(|(k, v)| format!("#{k}={v}"))
        .collect();
    if entries.is_empty() {
        String::new()
    } else {
        format!(" {}", entries.join(" "))
    }
}

/// ` [flapping]` (red) beats ` [permanent]` (yellow).
///
/// node: src/cli.ts:4102-4112
pub fn strategy_marker(tags: Option<&TagMap>) -> &'static str {
    let Some(tags) = tags else {
        return "";
    };
    if tags.get("strategy.status").map(String::as_str) == Some("flapping") {
        return " \x1b[31m[flapping]\x1b[0m";
    }
    if tags.get("strategy").map(String::as_str) == Some("permanent") {
        return " \x1b[33m[permanent]\x1b[0m";
    }
    ""
}

/// `<bold>dn</bold> <dim>(name)</dim>` or `<bold>name</bold>`.
///
/// node: src/cli.ts:2349-2355
pub fn render_label(dn: Option<&str>, name: &str, bold: &str) -> String {
    match dn {
        Some(dn) => format!("{bold}{dn}\x1b[0m \x1b[2m({name})\x1b[0m"),
        None => format!("{bold}{name}\x1b[0m"),
    }
}

/// Produces the freshest summary the client can get when it prints a trailer.
pub type SummaryProvider = Box<dyn FnMut() -> Option<SessionSummary> + Send>;

/// A local session: re-read the metadata when asked, so a `pty rename`
/// during the attach shows; fall back to what was known at the start once the
/// exit has reaped the entry.
pub fn local_summary_provider(id: &str) -> SummaryProvider {
    let id = id.to_string();
    let read =
        move |id: &str| registry::read_metadata(id).map(|m| SessionSummary::from_metadata(id, &m));
    let snapshot = read(&id);
    Box::new(move || read(&id).or_else(|| snapshot.clone()))
}

/// A summary known once, e.g. a remote row fetched at dial.
pub fn fixed_summary_provider(summary: Option<SessionSummary>) -> SummaryProvider {
    Box::new(move || summary.clone())
}

/// Why a client stopped showing a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEnd {
    /// Ctrl+\ in `attach`.
    Detached,
    /// Ctrl+\ in `peek -f`.
    PeekDetached,
    /// The session exited with this code.
    Exited(i32),
    /// The attachment lost its transport without a child EXIT.
    ConnectionLost,
}

/// Which session a trailer is about.
#[derive(Debug, Clone, Copy)]
pub struct TrailerTarget<'a> {
    pub id: &'a str,
    /// The fabric peer of a `--remote` session.
    pub peer: Option<&'a str>,
    pub summary: Option<&'a SessionSummary>,
}

impl TrailerTarget<'_> {
    fn attach_command(&self) -> String {
        match self.peer {
            Some(peer) => format!("pty attach --remote {peer} {}", self.id),
            None => format!("pty attach {}", self.id),
        }
    }
}

/// `[<event>]`, the header alone (machine mode prints only this).
pub fn trailer_header(end: SessionEnd, target: &TrailerTarget, now_ms: i64) -> String {
    let id = target.id;
    match end {
        SessionEnd::Detached | SessionEnd::PeekDetached => format!("[detached from {id}]"),
        SessionEnd::Exited(code) => match target.summary.and_then(|s| s.age(now_ms)) {
            Some(age) => format!("[{id} exited with code {code} after {age}]"),
            None => format!("[{id} exited with code {code}]"),
        },
        SessionEnd::ConnectionLost => format!("[connection lost to {id}]"),
    }
}

/// The next step after the session is no longer shown, if there is one.
fn trailer_hint(end: SessionEnd, target: &TrailerTarget) -> Option<String> {
    match end {
        SessionEnd::Detached => Some(format!("reattach: {}", target.attach_command())),
        SessionEnd::PeekDetached => Some(match target.peer {
            Some(peer) => format!("reattach: pty peek -f --remote {peer} {}", target.id),
            None => format!("reattach: pty peek -f {}", target.id),
        }),
        SessionEnd::Exited(_) => (target.peer.is_none()
            && target.summary.is_some_and(SessionSummary::kept_at_exit))
        .then(|| format!("restart: {}", target.attach_command())),
        SessionEnd::ConnectionLost => Some(format!("reconnect: {}", target.attach_command())),
    }
}

/// `\r\n<header>\r\n[  <summary line>\r\n][  <hint>\r\n]` — the caller writes
/// the terminal reset before it.
pub fn render_trailer(end: SessionEnd, target: &TrailerTarget, now_ms: i64) -> String {
    let mut out = format!("\r\n{}\r\n", trailer_header(end, target, now_ms));
    if let Some(summary) = target.summary {
        out.push_str(&summary.line(&TRAILER_LINE));
        out.push_str("\r\n");
    }
    if let Some(hint) = trailer_hint(end, target) {
        out.push_str("  ");
        out.push_str(&hint);
        out.push_str("\r\n");
    }
    out
}

/// [`render_trailer`] at the current time.
pub fn render_trailer_now(end: SessionEnd, target: &TrailerTarget) -> String {
    render_trailer(end, target, now_epoch_ms())
}

/// The stderr line printed when an interactive attach starts.
pub fn attach_banner(id: &str, summary: Option<&SessionSummary>) -> String {
    let label = summary.map_or_else(|| id.to_string(), SessionSummary::plain_label);
    format!("[attached to {label} — press Ctrl+\\ to detach]\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn web() -> SessionSummary {
        SessionSummary {
            id: "web-3f2a".into(),
            display_name: Some("My Web Server".into()),
            cwd: "/opt/proj".into(),
            command: Some("node app.js".into()),
            tags: Some(TagMap::from_iter([("role".to_string(), "web".to_string())])),
            created_at: Some("2026-09-24T10:00:00.000Z".into()),
            ephemeral: false,
        }
    }

    fn local(summary: Option<&SessionSummary>) -> TrailerTarget<'_> {
        TrailerTarget {
            id: "web-3f2a",
            peer: None,
            summary,
        }
    }

    fn at(iso: &str) -> i64 {
        parse_iso8601_ms(iso).unwrap()
    }

    #[test]
    fn detach_trailer_names_the_session_and_how_to_return() {
        let s = web();
        assert_eq!(
            render_trailer(SessionEnd::Detached, &local(Some(&s)), 0),
            "\r\n[detached from web-3f2a]\r\n  \x1b[1mMy Web Server\x1b[0m \x1b[2m(web-3f2a)\x1b[0m #role=web — /opt/proj — \x1b[2mnode app.js\x1b[0m\r\n  reattach: pty attach web-3f2a\r\n"
        );
    }

    #[test]
    fn remote_hints_name_the_peer_and_survive_a_missing_row() {
        let target = TrailerTarget {
            id: "w",
            peer: Some("box"),
            summary: None,
        };
        assert_eq!(
            render_trailer(SessionEnd::Detached, &target, 0),
            "\r\n[detached from w]\r\n  reattach: pty attach --remote box w\r\n"
        );
        assert_eq!(
            render_trailer(SessionEnd::ConnectionLost, &target, 0),
            "\r\n[connection lost to w]\r\n  reconnect: pty attach --remote box w\r\n"
        );
    }

    #[test]
    fn exit_header_carries_the_runtime_when_the_start_is_known() {
        let s = web();
        let now = at("2026-09-24T12:14:00.000Z");
        assert_eq!(
            trailer_header(SessionEnd::Exited(1), &local(Some(&s)), now),
            "[web-3f2a exited with code 1 after 2h14m]"
        );
        assert_eq!(
            trailer_header(SessionEnd::Exited(1), &local(None), now),
            "[web-3f2a exited with code 1]"
        );
    }

    #[test]
    fn restart_hint_only_when_the_entry_outlives_the_exit() {
        let mut kept = web();
        kept.tags = Some(TagMap::from_iter([(
            "keep".to_string(),
            "true".to_string(),
        )]));
        let mut ephemeral = web();
        ephemeral.ephemeral = true;
        assert_eq!(
            trailer_hint(SessionEnd::Exited(0), &local(Some(&kept))),
            Some("restart: pty attach web-3f2a".into())
        );
        assert_eq!(
            trailer_hint(SessionEnd::Exited(0), &local(Some(&ephemeral))),
            None
        );
        assert_eq!(trailer_hint(SessionEnd::Exited(0), &local(None)), None);
    }

    #[test]
    fn banner_uses_the_plain_label() {
        assert_eq!(
            attach_banner("web-3f2a", Some(&web())),
            "[attached to My Web Server (web-3f2a) — press Ctrl+\\ to detach]\n"
        );
        assert_eq!(
            attach_banner("x", None),
            "[attached to x — press Ctrl+\\ to detach]\n"
        );
    }
}
