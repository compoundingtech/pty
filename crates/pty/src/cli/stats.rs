//! `pty stats [--json] [--all] [<ref>]`: one session's live figures, or
//! every session's.
//!
//! node: src/cli.ts:1345-1356 (dispatch), 2448-2564 (`cmdStats`),
//! 2566-2595 (`printStats`), 2596-2615 (`formatMemory`, `formatUptime`)

use pty_client as client;
use pty_core::registry::{self, SessionInfo, SessionMetadata, short_path, time_ago};
use pty_core::stats::{GoneStats, StatsResult};

use super::{CliError, CliResult};

/// `<n> KB` below a megabyte, then `<x.x> MB`, then `<x.xx> GB`.
///
/// node: src/cli.ts:2596-2602
fn format_memory(rss_kb: u64) -> String {
    if rss_kb < 1024 {
        return format!("{rss_kb} KB");
    }
    let mb = rss_kb as f64 / 1024.0;
    if mb < 1024.0 {
        return format!("{mb:.1} MB");
    }
    format!("{:.2} GB", mb / 1024.0)
}

/// `unknown`, `<s>s`, `<m>m <s>s`, `<h>h <m>m`, then `<d>d <h>h`.
///
/// node: src/cli.ts:2604-2615
fn format_uptime(seconds: Option<i64>) -> String {
    let Some(seconds) = seconds else {
        return "unknown".to_string();
    };
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let m = seconds / 60;
    let s = seconds % 60;
    if m < 60 {
        return format!("{m}m {s}s");
    }
    let h = m / 60;
    let rm = m % 60;
    if h < 24 {
        return format!("{h}h {rm}m");
    }
    format!("{}d {}h", h / 24, h % 24)
}

/// The stats block for one session.
///
/// node: src/cli.ts:2566-2594
fn print_stats(stats: &StatsResult, meta: Option<&SessionMetadata>) {
    let cmd = meta
        .map(|m| m.display_command.as_str())
        .unwrap_or("unknown");
    let cwd = meta
        .map(|m| m.cwd.as_str())
        .filter(|c| !c.is_empty())
        .map(short_path)
        .unwrap_or_else(|| "unknown".to_string());

    println!("Session: {}", stats.name);
    println!("  Command:    {cmd}");
    println!("  CWD:        {cwd}");
    println!("  Uptime:     {}", format_uptime(stats.uptime_seconds));
    let pid_suffix = match stats.process.pid {
        Some(pid) if pid != 0 => format!(" (pid {pid})"),
        _ => String::new(),
    };
    let state = if stats.process.alive {
        "running".to_string()
    } else {
        // Node interpolates the raw value, so a missing code prints `null`.
        let code = stats
            .process
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "null".to_string());
        format!("exited (code {code})")
    };
    println!("  Process:    {state}{pid_suffix}");
    if let Some(r) = &stats.process.resources {
        println!("  CPU:        {:.1}%", r.cpu_percent);
        println!("  Memory:     {}", format_memory(r.rss_kb));
    }
    let daemon_mem = stats
        .daemon
        .resources
        .as_ref()
        .map(|r| format!(", {}", format_memory(r.rss_kb)))
        .unwrap_or_default();
    println!("  Daemon:     pid {}{daemon_mem}", stats.daemon.pid);
    println!(
        "  Terminal:   {}x{}",
        stats.terminal.cols, stats.terminal.rows
    );
    println!(
        "  Cursor:     row {}, col {}",
        stats.terminal.cursor_y, stats.terminal.cursor_x
    );
    println!(
        "  Scrollback: {} / {} lines",
        stats.terminal.scrollback_used, stats.terminal.scrollback_capacity
    );
    println!(
        "  Clients:    {} ({} attached, {} readonly)",
        stats.clients.total, stats.clients.attached, stats.clients.read_only
    );

    let mut modes: Vec<String> = Vec::new();
    if stats.modes.sgr_mouse {
        modes.push("SGR mouse".to_string());
    }
    if stats.modes.cursor_hidden {
        modes.push("cursor hidden".to_string());
    }
    if stats.modes.kitty_keyboard {
        let flags: Vec<String> = stats
            .modes
            .kitty_keyboard_flags
            .iter()
            .map(u8::to_string)
            .collect();
        modes.push(format!("kitty keyboard (flags: {})", flags.join(",")));
    }
    println!(
        "  Modes:      {}",
        if modes.is_empty() {
            "none".to_string()
        } else {
            modes.join(", ")
        }
    );
}

/// The `{name, status, exitCode, exitedAt, tags?}` shape for a session that
/// is no longer running. `tags` is omitted when the metadata has none.
fn gone_stats(name: &str, meta: Option<&SessionMetadata>, status: &str) -> GoneStats {
    GoneStats {
        name: name.to_string(),
        status: status.to_string(),
        exit_code: meta.and_then(|m| m.exit_code),
        exited_at: meta.and_then(|m| m.exited_at.clone()),
        tags: meta
            .and_then(|m| m.tags.as_ref())
            .map(|t| t.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
    }
}

fn json_line<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

/// `cmdStats`.
pub fn run(args: &[String]) -> CliResult {
    let json = args.iter().any(|a| a == "--json");
    let all = args.iter().any(|a| a == "--all");
    // Flags may sit anywhere; the first bare token is the ref and any
    // further token is ignored (cli.ts:1345-1356).
    let reference = args.iter().find(|a| !a.starts_with('-')).cloned();

    if let Some(reference) = reference {
        let session = registry::get_session(&reference).map_err(CliError)?;
        let Some(session) = session else {
            eprintln!("Session \"{reference}\" not found.");
            return Ok(1);
        };
        if session.is_gone() {
            if json {
                println!(
                    "{}",
                    json_line(&gone_stats(
                        &session.name,
                        session.metadata.as_ref(),
                        session.status.as_str()
                    ))
                );
            } else if session.status == registry::SessionStatus::Vanished {
                // Node prints the reference as the caller typed it.
                println!(
                    "Session \"{reference}\" has vanished (no exit record — killed or crashed)."
                );
            } else {
                let code = session
                    .metadata
                    .as_ref()
                    .and_then(|m| m.exit_code)
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "?".to_string());
                println!("Session \"{reference}\" has exited (code {code}).");
            }
            return Ok(0);
        }

        // Query by the resolved stable id, so a display name reaches the
        // right socket.
        match client::query_stats(&session.name) {
            Ok(stats) => {
                if json {
                    println!("{}", json_line(&stats));
                } else {
                    print_stats(&stats, session.metadata.as_ref());
                }
                Ok(0)
            }
            Err(e) => {
                eprintln!("{e}");
                Ok(1)
            }
        }
    } else {
        run_all(json, all)
    }
}

/// No ref: every session at once.
///
/// node: src/cli.ts:2492-2564
fn run_all(json: bool, all: bool) -> CliResult {
    let sessions = registry::list_sessions();
    let running: Vec<&SessionInfo> = sessions.iter().filter(|s| s.is_running()).collect();
    let gone: Vec<&SessionInfo> = sessions.iter().filter(|s| s.is_gone()).collect();

    if running.is_empty() && (!all || gone.is_empty()) {
        println!("No running sessions.");
        return Ok(0);
    }

    // Node queries in parallel; one thread per running session keeps the
    // wall time at one timeout rather than N.
    let results: Vec<(&SessionInfo, Result<StatsResult, String>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = running
            .iter()
            .map(|s| {
                let name = s.name.clone();
                (
                    *s,
                    scope.spawn(move || client::query_stats(&name).map_err(|e| e.to_string())),
                )
            })
            .collect();
        handles
            .into_iter()
            .map(|(s, h)| {
                (
                    s,
                    h.join()
                        .unwrap_or_else(|_| Err("stats query panicked".to_string())),
                )
            })
            .collect()
    });

    if json {
        let mut items: Vec<String> = Vec::new();
        for (session, result) in &results {
            items.push(match result {
                Ok(stats) => json_line(stats),
                Err(msg) => format!(
                    "{{\"name\":{},\"error\":{}}}",
                    json_line(&session.name),
                    json_line(msg)
                ),
            });
        }
        if all {
            for s in &gone {
                items.push(json_line(&gone_stats(
                    &s.name,
                    s.metadata.as_ref(),
                    s.status.as_str(),
                )));
            }
        }
        println!("[{}]", items.join(","));
        return Ok(0);
    }

    for (i, (session, result)) in results.iter().enumerate() {
        match result {
            Ok(stats) => print_stats(stats, session.metadata.as_ref()),
            Err(msg) => {
                println!("Session: {}", session.name);
                println!("  Error: {msg}");
            }
        }
        if i + 1 < results.len() {
            println!();
        }
    }

    if all && !gone.is_empty() {
        if !results.is_empty() {
            println!();
        }
        let exited: Vec<&&SessionInfo> = gone
            .iter()
            .filter(|s| s.status == registry::SessionStatus::Exited)
            .collect();
        let vanished: Vec<&&SessionInfo> = gone
            .iter()
            .filter(|s| s.status == registry::SessionStatus::Vanished)
            .collect();
        if !exited.is_empty() {
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
                println!("  {} (exited with code {code}, {ago})", s.name);
            }
        }
        if !vanished.is_empty() {
            if !exited.is_empty() {
                println!();
            }
            println!("Vanished sessions (no exit record):");
            for s in &vanished {
                let ago = s
                    .metadata
                    .as_ref()
                    .map(|m| m.created_at.as_str())
                    .filter(|c| !c.is_empty())
                    .map(time_ago)
                    .unwrap_or_else(|| "unknown".to_string());
                println!("  \u{26a0} {} (started {ago})", s.name);
            }
        }
    }
    Ok(0)
}
