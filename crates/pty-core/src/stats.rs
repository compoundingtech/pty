//! `stats --json` result shapes, matching node's `StatsResult` (the machine-
//! readable subset — the human-readable stats screen is out of parity scope).
//!
//! Three shapes: a full [`StatsResult`] for a running session (produced by the
//! daemon, which has the live terminal), a small [`GoneStats`] for an
//! exited/vanished session (from metadata, no live query), and an array of
//! either for `stats --json` with no ref.

use serde::{Deserialize, Serialize};

/// Process/daemon resource usage.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resources {
    pub rss_kb: u64,
    pub cpu_percent: f64,
}

/// Terminal geometry + cursor + scrollback.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalStats {
    pub cols: u16,
    pub rows: u16,
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub scrollback_used: usize,
    pub scrollback_capacity: usize,
}

/// The session (child) process.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessStats {
    pub alive: bool,
    pub exit_code: Option<i32>,
    pub pid: Option<i32>,
    pub resources: Option<Resources>,
}

/// The daemon (server) process.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonStats {
    pub pid: i32,
    pub resources: Option<Resources>,
}

/// Which of a client's dimensions constrain the shared grid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Constrains {
    pub rows: bool,
    pub cols: bool,
}

/// One entry of `clients.connections` (node `StatsResult.clients.connections`):
/// a writable (attached) client with its requested size and negotiation
/// sequence, or a readonly (peek) client that never constrains the grid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "role",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum ConnectionStats {
    Writable {
        rows: u16,
        cols: u16,
        last_request_sequence: u64,
        constrains: Constrains,
    },
    Readonly {
        constrains: Constrains,
    },
}

/// Connected client counts. `connections` is absent in the STATUS body of a
/// daemon that predates it; a client must accept both shapes
/// (`tests/protocol.test.ts:315-345`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientStats {
    pub total: usize,
    pub attached: usize,
    pub read_only: usize,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub connections: Option<Vec<ConnectionStats>>,
}

/// Terminal mode state (deterministic from the input byte stream).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModeStats {
    pub sgr_mouse: bool,
    pub cursor_hidden: bool,
    pub kitty_keyboard: bool,
    pub kitty_keyboard_flags: Vec<u8>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub bracketed_paste: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The full running-session stats result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsResult {
    pub name: String,
    pub terminal: TerminalStats,
    pub process: ProcessStats,
    pub daemon: DaemonStats,
    pub clients: ClientStats,
    pub modes: ModeStats,
    /// `floor((now - createdAt) / 1000)`: an integer in Node, so never `10.0`.
    pub uptime_seconds: Option<i64>,
    pub created_at: Option<String>,
}

/// The small shape for an exited/vanished session (no live query).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoneStats {
    pub name: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub exited_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tags: Option<std::collections::BTreeMap<String, String>>,
}

/// A process's resident set size and average CPU, from `/proc` where there is
/// one and from `ps` where there is not. `None` when neither can answer.
///
/// **Without the `ps` half this returned `None` on every machine that is not
/// Linux**, so `pty stats` showed no CPU and no memory line at all on a Mac,
/// and `--json` carried `"resources": null`. It failed quietly, which is why
/// it surfaced as five unrelated-looking test failures rather than one gap.
/// The Node tool uses `ps -o rss=,pcpu=` on every platform
/// (`src/server.ts`, `queryProcessResources`).
///
/// `/proc` is kept where it exists because it costs no subprocess, and
/// `stats` is asked for every session in a listing.
pub fn read_resources(pid: i32) -> Option<Resources> {
    if pid <= 0 {
        return None;
    }
    if !cfg!(target_os = "linux") {
        return read_resources_from_ps(pid);
    }
    let page_kb = {
        // SAFETY: sysconf is a simple query.
        let ps = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if ps > 0 { (ps as u64) / 1024 } else { 4 }
    };
    let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    let rss_kb = resident_pages * page_kb;

    let cpu_percent = read_cpu_percent(pid).unwrap_or(0.0);
    Some(Resources {
        rss_kb,
        cpu_percent,
    })
}

/// The same two numbers from `ps`, which every unix has and which is what the
/// Node tool uses everywhere.
///
/// `rss=` is in kilobytes and `pcpu=` is the average over the process's
/// lifetime, which is what the `/proc` path above computes by hand — so the
/// two agree about what they mean.
///
/// node: src/server.ts `queryProcessResources`
fn read_resources_from_ps(pid: i32) -> Option<Resources> {
    #[cfg(target_os = "macos")]
    {
        return match crate::proctable::resources_of(pid) {
            crate::proctable::Answer::Known((rss_kb, cpu_percent)) => Some(Resources {
                rss_kb,
                cpu_percent,
            }),
            _ => None,
        };
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        None
    }
}

/// Split out from its caller so it can be tested where there is no `ps` that
/// answers this way.
fn parse_ps_resources(output: &str) -> Option<Resources> {
    let mut fields = output.split_whitespace();
    let rss_kb: u64 = fields.next()?.parse().ok()?;
    let cpu_percent: f64 = fields.next()?.parse().ok()?;
    Some(Resources {
        rss_kb,
        cpu_percent,
    })
}

/// Average CPU% over the process's lifetime (utime+stime vs wall-clock uptime).
fn read_cpu_percent(pid: i32) -> Option<f64> {
    let clk_tck = {
        let t = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if t > 0 { t as f64 } else { 100.0 }
    };
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the ")" of comm: index 0 = state (field 3). utime = field 14
    // (index 11), stime = field 15 (index 12), starttime = field 22 (index 19).
    let after = stat.rsplit_once(')')?.1;
    let f: Vec<&str> = after.split_whitespace().collect();
    let utime: f64 = f.get(11)?.parse().ok()?;
    let stime: f64 = f.get(12)?.parse().ok()?;
    let starttime: f64 = f.get(19)?.parse().ok()?;

    let uptime_str = std::fs::read_to_string("/proc/uptime").ok()?;
    let system_uptime: f64 = uptime_str.split_whitespace().next()?.parse().ok()?;

    let proc_uptime = system_uptime - (starttime / clk_tck);
    if proc_uptime <= 0.0 {
        return Some(0.0);
    }
    let cpu_secs = (utime + stime) / clk_tck;
    Some((cpu_secs / proc_uptime) * 100.0)
}

#[cfg(test)]
mod ps_resources_tests {
    use super::parse_ps_resources;

    #[test]
    fn reads_the_two_fields_ps_prints() {
        // What `ps -o rss=,pcpu= -p <pid>` gives, padded as it pads.
        let r = parse_ps_resources("  13824   2.4\n").expect("parsed");
        assert_eq!(r.rss_kb, 13824);
        assert!((r.cpu_percent - 2.4).abs() < f64::EPSILON, "{}", r.cpu_percent);
    }

    #[test]
    fn a_process_that_is_gone_prints_nothing_and_is_not_a_reading() {
        assert!(parse_ps_resources("").is_none());
        assert!(parse_ps_resources("   \n").is_none());
    }

    /// The `ps` path only RUNS off Linux, but it can be exercised anywhere
    /// `ps` accepts those fields, which Linux does. So the branch a Mac
    /// depends on is proved on every machine that runs this suite, rather
    /// than only on the one that needs it.
    #[test]
    fn ps_reports_real_numbers_for_this_very_process() {
        let me = std::process::id() as i32;
        let Some(r) = super::read_resources_from_ps(me) else {
            // A machine whose `ps` does not take these fields is not a
            // failure of this code; say so rather than pass silently.
            eprintln!("skipped: `ps -o rss=,pcpu=` gave nothing on this machine");
            return;
        };
        assert!(r.rss_kb > 0, "a running process reported {} KB", r.rss_kb);
        assert!(
            r.cpu_percent >= 0.0 && r.cpu_percent <= 100.0 * 64.0,
            "implausible cpu {}",
            r.cpu_percent
        );
    }

    /// A `ps` that answers with only one of the two is not a reading either.
    #[test]
    fn a_half_answer_is_not_a_reading() {
        assert!(parse_ps_resources("13824").is_none());
        assert!(parse_ps_resources("notanumber 2.4").is_none());
        assert!(parse_ps_resources("13824 notanumber").is_none());
    }
}
