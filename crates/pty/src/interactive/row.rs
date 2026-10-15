//! What one line of the picker says, and how the filter ranks it.

use pty_core::registry::{SessionInfo, SessionStatus, is_reserved_tag_key, short_path, time_ago};
use pty_tui::fuzzy_match;

/// A running session beats a stopped one whatever else matches.
const RUNNING_BONUS: i64 = 100_000;
/// A match on the name or the label beats a match on the command.
const NAME_BONUS: i64 = 10_000;

/// One line of the list.
pub enum Row {
    Session(Box<SessionInfo>),
    /// The last line of the list: make a new session.
    Create,
}

/// How well `session` matches `query`, or `None` if it does not.
///
/// The query may be `host/session`; only the part after the slash is matched
/// here, because every session in this list is local.
pub fn score(session: &SessionInfo, query: &str) -> Option<i64> {
    let query = query.rsplit('/').next().unwrap_or(query);
    let meta = session.metadata.as_ref();
    let display_name = meta.and_then(|m| m.display_name.as_deref()).unwrap_or("");
    let command = meta.map(|m| m.display_command.as_str()).unwrap_or("");
    let cwd = meta.map(|m| m.cwd.as_str()).unwrap_or("");

    let running = session.status == SessionStatus::Running;
    if query.is_empty() {
        return Some(if running { RUNNING_BONUS } else { 0 });
    }

    let name_score = fuzzy_match(query, &session.name)
        .into_iter()
        .chain(fuzzy_match(query, display_name))
        .max()
        .map(|s| s + NAME_BONUS);
    let other_score = fuzzy_match(query, command)
        .into_iter()
        .chain(fuzzy_match(query, cwd))
        .max();
    let best = name_score.into_iter().chain(other_score).max()?;
    Some(best + if running { RUNNING_BONUS } else { 0 })
}

/// The text of one session line, without the selection marker.
///
/// `● label (id) [permanent] #k=v  ~/dir  the command  (exited 2h ago)`
pub fn describe(session: &SessionInfo) -> String {
    let meta = session.metadata.as_ref();
    let mut out = String::new();
    out.push_str(match session.status {
        SessionStatus::Running => "● ",
        _ => "○ ",
    });

    match meta.and_then(|m| m.display_name.as_deref()) {
        Some(label) if !label.is_empty() => {
            out.push_str(label);
            out.push_str(" (");
            out.push_str(&session.name);
            out.push(')');
        }
        _ => out.push_str(&session.name),
    }

    let tags = meta.and_then(|m| m.tags.as_ref());
    if tags.and_then(|t| t.get("strategy")).map(String::as_str) == Some("permanent") {
        out.push_str(" [permanent]");
    }
    if let Some(tags) = tags {
        for (k, v) in tags.iter().filter(|(k, _)| !is_reserved_tag_key(k)) {
            out.push_str(&format!(" #{k}={v}"));
        }
    }

    if let Some(cwd) = meta.map(|m| m.cwd.as_str()).filter(|c| !c.is_empty()) {
        out.push_str("  ");
        out.push_str(&short_path(cwd));
    }
    if let Some(cmd) = meta
        .map(|m| m.display_command.as_str())
        .filter(|c| !c.is_empty())
    {
        out.push_str("  ");
        out.push_str(cmd);
    }

    if session.status != SessionStatus::Running {
        let when = meta
            .and_then(|m| m.exited_at.as_deref())
            .filter(|s| !s.is_empty())
            .map(time_ago);
        match when {
            Some(ago) => out.push_str(&format!("  (exited {ago})")),
            None => out.push_str("  (exited)"),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_running_session_outranks_a_stopped_one() {
        // Two sessions the query matches equally; only the status differs.
        let mut running = session("alpha", SessionStatus::Running);
        let stopped = session("alpha", SessionStatus::Exited);
        running.status = SessionStatus::Running;
        assert!(score(&running, "alpha").unwrap() > score(&stopped, "alpha").unwrap());
    }

    #[test]
    fn a_query_that_matches_nothing_drops_the_row() {
        assert_eq!(score(&session("alpha", SessionStatus::Running), "zzzz"), None);
    }

    #[test]
    fn a_host_prefix_is_ignored_because_every_row_is_local() {
        let s = session("alpha", SessionStatus::Running);
        assert_eq!(score(&s, "somehost/alpha"), score(&s, "alpha"));
    }

    fn session(name: &str, status: SessionStatus) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            socket_path: Default::default(),
            pid: None,
            status,
            identity: pty_core::registry::DaemonIdentityCheck::Match,
            metadata: None,
        }
    }
}
