//! Terminal events → the session's events log.
//!
//! node: src/server.ts:343-454; src/events.ts:352-390

use pty_core::events::{Event, NotificationSource};
use pty_terminal::TerminalEvent;

use super::lifecycle::Daemon;

fn source(name: &str) -> Option<NotificationSource> {
    match name {
        "osc9" => Some(NotificationSource::Osc9),
        "osc99" => Some(NotificationSource::Osc99),
        "osc777" => Some(NotificationSource::Osc777),
        _ => None,
    }
}

impl Daemon {
    /// Append durable events the actor collected since the last call. The
    /// writer serializes them under the event lock and applies the daemon
    /// retention rule (a line-count check every 100 appends).
    pub(crate) fn forward_terminal_events(&mut self) {
        for ev in self.actor.take_events() {
            let event = match ev {
                TerminalEvent::Bell => Event::bell(&self.name),
                TerminalEvent::TitleChange(value) => Event::title_change(&self.name, &value),
                TerminalEvent::Notification(n) => Event::notification(
                    &self.name,
                    n.title.as_deref(),
                    n.body.as_deref(),
                    source(n.source),
                ),
                TerminalEvent::FocusRequest => Event::focus_request(&self.name),
                // Cursor visibility is redraw state for terminal snapshots, not history.
                TerminalEvent::CursorVisible => continue,
            };
            self.events.append(event);
        }
    }
}
