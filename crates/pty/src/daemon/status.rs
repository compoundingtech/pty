//! The STATUS reply: Node's `collectStats`.
//!
//! node: src/server.ts:1084-1156, 214-232

use pty_core::registry;
use pty_core::stats::{
    ClientStats, ConnectionStats, Constrains, DaemonStats, ModeStats, ProcessStats, Resources,
    StatsResult, TerminalStats,
};

use super::clients::Role;
use super::lifecycle::Daemon;

/// `ps -o rss=,pcpu= -p <pid>`, Node's source on every platform; `/proc`
/// on Linux is cheaper and equivalent.
///
/// node: src/server.ts:217-232
pub fn query_process_resources(pid: i32) -> Option<Resources> {
    if pid <= 0 {
        return None;
    }
    // One implementation, not one per caller: `/proc` on Linux and
    // `proc_pidinfo` on macOS, both inside `pty_core`.
    pty_core::stats::read_resources(pid)
}

impl Daemon {
    /// node: src/server.ts:1084-1156
    pub(crate) fn collect_stats(&self) -> StatsResult {
        let meta = registry::read_metadata(&self.name);
        let mut attached = 0;
        let mut read_only = 0;
        let mut connections = Vec::new();
        let (term_rows, term_cols) = (self.actor.rows(), self.actor.cols());
        for c in self.clients.values() {
            match c.role {
                Role::Readonly => {
                    read_only += 1;
                    connections.push(ConnectionStats::Readonly {
                        constrains: Constrains {
                            rows: false,
                            cols: false,
                        },
                    });
                }
                Role::Writable if c.attach_seq > 0 => {
                    attached += 1;
                    connections.push(ConnectionStats::Writable {
                        rows: c.rows,
                        cols: c.cols,
                        last_request_sequence: c.attach_seq,
                        constrains: Constrains {
                            rows: c.rows == term_rows,
                            cols: c.cols == term_cols,
                        },
                    });
                }
                _ => {}
            }
        }
        let created_at = meta.as_ref().map(|m| m.created_at.clone()).filter(|c| !c.is_empty());
        let uptime_seconds = created_at.as_deref().and_then(|c| {
            let created = registry::parse_iso8601_ms(c)?;
            Some(((registry::now_epoch_ms() - created) as f64 / 1000.0).floor() as i64)
        });
        let child_pid = (!self.exited).then_some(self.child_pid);
        let daemon_pid = std::process::id() as i32;
        let (cursor_x, cursor_y, _) = self.actor.cursor();
        let modes = self.actor.modes();
        StatsResult {
            name: self.name.clone(),
            terminal: TerminalStats {
                cols: term_cols,
                rows: term_rows,
                cursor_x,
                cursor_y,
                scrollback_used: self.actor.scrollback_used(),
                scrollback_capacity: self.actor.scrollback_capacity(),
            },
            process: ProcessStats {
                alive: !self.exited,
                exit_code: self.exited.then_some(self.exit_code),
                pid: child_pid,
                resources: child_pid.and_then(query_process_resources),
            },
            daemon: DaemonStats {
                pid: daemon_pid,
                resources: query_process_resources(daemon_pid),
            },
            clients: ClientStats {
                total: attached + read_only,
                attached,
                read_only,
                connections: Some(connections),
            },
            modes: ModeStats {
                sgr_mouse: modes.sgr_mouse,
                cursor_hidden: modes.cursor_hidden,
                kitty_keyboard: !modes.kitty_stack.is_empty(),
                kitty_keyboard_flags: modes.kitty_stack.clone(),
                bracketed_paste: modes.bracketed_paste,
            },
            uptime_seconds,
            created_at,
        }
    }
}
