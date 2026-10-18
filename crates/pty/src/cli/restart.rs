//! `pty restart [-y] [--force] <name>`: stop a session and start it again
//! from its stored command.
//!
//! node: src/cli.ts:1358-1382 (dispatch), 3850-3857 (`statefulAgentReason`),
//! 3886-3963 (`cmdRestart`)

use std::time::{Duration, Instant};

use pty_core::registry::{self, SessionMetadata, SessionStatus};

use super::{
    CliResult, RESTART_SCRUBBED_ENV, SpawnParams, apply_persisted_launch_options, ask, resolve_ref,
};

const USAGE: &str = "Usage: pty restart [-y] [--force] <name>";

/// How long restart waits for the stopped daemon to exit. Its graceful
/// shutdown is bounded by its own deadline (`PTY_SHUTDOWN_DEADLINE_MS`, 5 s by
/// default), after which it forces its exit; this allows margin over that.
const OLD_DAEMON_EXIT_WAIT: Duration = Duration::from_secs(10);
const OLD_DAEMON_EXIT_POLL: Duration = Duration::from_millis(20);

/// Wait until `pid` has exited (a zombie counts), or `within` has passed.
fn wait_for_exit(pid: i32, within: Duration) -> bool {
    let start = Instant::now();
    loop {
        if registry::has_process_exited_for_reap(pid) {
            return true;
        }
        if start.elapsed() >= within {
            return false;
        }
        std::thread::sleep(OLD_DAEMON_EXIT_POLL);
    }
}

/// Why this session should not be blindly re-run. `pty restart` repeats the
/// stored argv, which is fine for a daemon and a footgun for an interactive
/// agent holding state.
///
/// node: src/cli.ts:3850-3857
fn stateful_agent_reason(meta: &SessionMetadata) -> Option<&'static str> {
    if meta.tags.as_ref().and_then(|t| t.get("role")).map(String::as_str) == Some("agent") {
        return Some("role=agent tag");
    }
    let argv = std::iter::once(meta.command.clone())
        .chain(meta.args.iter().cloned())
        .chain(std::iter::once(meta.display_command.clone()))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if mentions_claude(&argv) && mentions_resume(&argv) {
        return Some("claude --resume command");
    }
    None
}

/// `/(^|\s|\/)claude(\s|$)/`
fn mentions_claude(argv: &str) -> bool {
    argv.match_indices("claude").any(|(at, _)| {
        let before = argv[..at].chars().next_back();
        let after = argv[at + "claude".len()..].chars().next();
        matches!(before, None | Some(' ') | Some('\t') | Some('/'))
            && matches!(after, None | Some(' ') | Some('\t'))
    })
}

/// `/(^|\s)--resume(\s|=|$)/`
fn mentions_resume(argv: &str) -> bool {
    argv.match_indices("--resume").any(|(at, _)| {
        let before = argv[..at].chars().next_back();
        let after = argv[at + "--resume".len()..].chars().next();
        matches!(before, None | Some(' ') | Some('\t'))
            && matches!(after, None | Some(' ') | Some('\t') | Some('='))
    })
}

/// `pty restart` dispatch and `cmdRestart`.
pub fn run(args: &[String]) -> CliResult {
    let mut yes = false;
    let mut force_nested = false;
    let mut name: Option<String> = None;
    for a in args {
        match a.as_str() {
            "-y" | "--yes" => yes = true,
            "--force" => force_nested = true,
            other if name.is_none() => name = Some(other.to_string()),
            other => {
                eprintln!("pty restart: unexpected argument \"{other}\"");
                return Ok(1);
            }
        }
    }
    let Some(reference) = name else {
        eprintln!("{USAGE}");
        return Ok(1);
    };
    let name = resolve_ref(&reference)?;

    let Some(session) = registry::get_session_by_name(&name) else {
        eprintln!("Session \"{name}\" not found.");
        return Ok(1);
    };
    let Some(meta) = session.metadata.clone() else {
        eprintln!("Session \"{name}\" has no metadata — cannot restart.");
        let _ = registry::cleanup_all(&name);
        return Ok(1);
    };

    if let Some(reason) = stateful_agent_reason(&meta)
        && !force_nested
    {
        eprintln!("Session \"{name}\" looks like a stateful agent ({reason}).");
        eprintln!(
            "`pty restart` kills its in-progress work and can wedge a `claude --resume`. \
             Cycle it through its supervisor (e.g. `convoy up`) instead — or pass --force to restart anyway."
        );
        return Ok(1);
    }

    if session.status == SessionStatus::Running
        && !yes && ask::declined(&ask::ask(&format!(
            "Session \"{name}\" is running. Kill and restart? [Y/n] "
        )))
    {
        return Ok(0);
    }
    let mut event_guard = registry::take_event_lock(&name)?;
    let creation_guard = registry::take_metadata_lock(&name)?;
    let current = registry::read_metadata(&name)
        .ok_or_else(|| format!("Session \"{name}\" metadata became unreadable."))?;
    if !registry::metadata_matches_observation(&meta, &current) {
        return Err(format!("Session \"{name}\" changed before restart.").into());
    }
    let mut params = SpawnParams::new(&name, &meta.command, &meta.args);
    params.display_command = meta.display_command.clone();
    params.cwd = meta.cwd.clone();
    // An explicit retry drops flapping bookkeeping, never launch provenance.
    params.tags = registry::strip_gc_bookkeeping(meta.tags.as_ref()).unwrap_or_default();
    params.display_name = meta.display_name.clone();
    apply_persisted_launch_options(&mut params, &meta);
    params.scrub_env = RESTART_SCRUBBED_ENV.iter().map(|s| s.to_string()).collect();
    pty_lifecycle::config_for(&params).check_publication()?;
    let publisher = super::prepare_daemon(&mut params)?;
    params.creation_lock_delegate = Some(creation_guard.delegate().map_err(|error| error.to_string())?);
    if session.status == SessionStatus::Running
        && let Some(pid) = session.pid
    {
        // SAFETY: the exact observed generation remains under name ownership.
        unsafe { libc::kill(pid, libc::SIGTERM) };
        // The stopping daemon still writes its exit events under the event
        // lock and owns its pid and socket until it exits. Starting the new
        // daemon before then finds the event log busy, so let go of the event
        // lock, wait for the old pid to be gone, and take the lock back. The
        // creation lock, and so name ownership, stays held throughout.
        event_guard.release();
        if !wait_for_exit(pid, OLD_DAEMON_EXIT_WAIT) {
            eprintln!(
                "pty restart: session \"{name}\" (pid {pid}) did not exit within {}s of SIGTERM; nothing was restarted. Stop it with `pty kill {name}` and retry.",
                OLD_DAEMON_EXIT_WAIT.as_secs()
            );
            return Ok(1);
        }
        event_guard = registry::wait_for_event_lock(&name, registry::lock::EVENT_LOCK_WAIT)?;
    }
    registry::cleanup_all_while_locked(&name);
    event_guard.release();
    let spawned = publisher.spawn(params);
    creation_guard.release();
    if let Err(error) = spawned {
        eprintln!("pty restart: {error}");
        return Ok(1);
    }
    println!("Session \"{name}\" restarted.");

    // Restarting from inside a session is fine; attaching afterwards would
    // nest a client, so say what was skipped and stop.
    let nested = std::env::var("PTY_SESSION").unwrap_or_default();
    if !nested.is_empty() && !force_nested {
        println!(
            "  (not attached: already inside pty session \"{nested}\". Pass --force to attach anyway.)"
        );
        return Ok(0);
    }
    Ok(super::attach::do_attach(&name, None))
}
