//! `pty restart [-y] [--force] <name>`: stop a session and start it again
//! from its stored command.
//!
//! node: src/cli.ts:1358-1382 (dispatch), 3850-3857 (`statefulAgentReason`),
//! 3886-3963 (`cmdRestart`)

use pty_core::registry::{self, SessionMetadata, SessionStatus};

use super::{
    CliResult, RESTART_SCRUBBED_ENV, SpawnParams, apply_persisted_launch_options, ask, resolve_ref,
};

const USAGE: &str = "Usage: pty restart [-y] [--force] <name>";

/// Why this session should not be blindly re-run. `pty restart` repeats the
/// stored argv, which is fine for a daemon and a footgun for an interactive
/// agent holding state.
///
/// node: src/cli.ts:3850-3857
fn stateful_agent_reason(meta: &SessionMetadata) -> Option<&'static str> {
    if meta
        .tags
        .as_ref()
        .and_then(|t| t.get("role"))
        .map(String::as_str)
        == Some("agent")
    {
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
    if session.identity == registry::DaemonIdentityCheck::Unknown {
        eprintln!("pty restart: Session \"{name}\" daemon identity is unknown.");
        return Ok(1);
    }
    let Some(meta) = session.metadata.clone() else {
        eprintln!("Session \"{name}\" has no metadata — cannot restart.");
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
        && !yes
        && ask::declined(&ask::ask(&format!(
            "Session \"{name}\" is running. Kill and restart? [Y/n] "
        )))
    {
        return Ok(0);
    }
    let mut params = SpawnParams::new(&name, &meta.command, &meta.args);
    params.display_command = meta.display_command.clone();
    params.cwd = meta.cwd.clone();
    // An explicit retry drops flapping bookkeeping, never launch provenance.
    params.tags = registry::strip_gc_bookkeeping(meta.tags.as_ref()).unwrap_or_default();
    params.display_name = meta.display_name.clone();
    apply_persisted_launch_options(&mut params, &meta);
    params.scrub_env = RESTART_SCRUBBED_ENV.iter().map(|s| s.to_string()).collect();
    // Refuse a revoked or unverifiable launch before stopping the survivor.
    pty_lifecycle::config_for(&params).check_publication()?;
    let publisher = super::prepare_daemon(&mut params)?;

    if session.status == SessionStatus::Running {
        let stopped = match pty_client::stop(&name, meta.generation.as_deref()) {
            Ok(stopped) => stopped,
            Err(error) => {
                eprintln!("pty restart: {error}");
                return Ok(1);
            }
        };
        if !stopped.verified_empty() {
            eprintln!(
                "pty restart: Session \"{name}\" daemon stopped, but process tree could not be verified empty; not restarted."
            );
            return Ok(1);
        }
    }

    // `stop` waited for the daemon it signalled. An exited session's daemon
    // can still be finishing, and it needs the event lock to finish, so wait
    // for it before taking the locks that keep the name through the respawn.
    if let Err(error) = pty_client::wait_for_recorded_owner_loss(&name, &meta) {
        eprintln!("pty restart: {error}");
        return Ok(1);
    }
    let event_guard = registry::take_event_lock(&name)?;
    let creation_guard = registry::take_metadata_lock(&name)?;
    let current = registry::read_metadata(&name)
        .ok_or_else(|| format!("Session \"{name}\" metadata became unreadable."))?;
    if !registry::metadata_matches_observation(&meta, &current) {
        return Err(format!("Session \"{name}\" changed before restart.").into());
    }
    pty_lifecycle::config_for(&params).check_publication()?;
    params.creation_lock_delegate = Some(creation_guard.delegate().map_err(|error| error.to_string())?);
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
