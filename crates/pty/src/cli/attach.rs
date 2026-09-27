//! `pty attach [-r|--auto-restart|--no-restart] [--force] [--remote <peer>]
//! [--attach-stream-fd-v1 <fd>] <name>`: connect a terminal to a session.
//!
//! The nesting guard runs before the reference is resolved, so somebody who
//! should not be attaching at all hears that first rather than fixing a typo
//! and then being refused.
//!
//! node: src/cli.ts:984-1053 (dispatch), 1773-1806 (`cmdAttach`),
//! 1808-1853 (`handleDeadSession`)

use pty_client as client;
use pty_client::summary::{
    SessionSummary, attach_banner, fixed_summary_provider, local_summary_provider,
};
use pty_core::registry::{self, SessionMetadata, SessionStatus};

use super::{
    CliResult, RESTART_SCRUBBED_ENV, SpawnParams, apply_persisted_launch_options, ask,
    ensure_not_nested, resolve_ref,
};

const USAGE: &str =
    "Usage: pty attach [-r|--auto-restart|--no-restart] [--force] [--remote <peer>] <name>";

const NESTING_HINT: &str = "  Attaching now would nest a client inside the current session — detach keys route to the outer client and get tangled.\n  Detach first (Ctrl+\\) or, from inside pty-layout, use ^]n to pick a session.\n  Pass --force to attach anyway (nested clients are usually a mistake).";

/// What to do when the session is not running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartPolicy {
    /// Refuse: never turn later input into permission to run stored
    /// launch metadata.
    Never,
    Prompt,
    Always,
}

/// `pty attach` dispatch and `cmdAttach`.
pub fn run(args: &[String]) -> CliResult {
    let mut auto_restart = false;
    let mut no_restart = false;
    let mut force = false;
    let mut name: Option<String> = None;
    let mut remote: Option<String> = None;
    let mut stream_fd_token: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--auto-restart" | "-r" => auto_restart = true,
            "--no-restart" => no_restart = true,
            "--force" => force = true,
            "--remote" if i + 1 < args.len() => {
                remote = Some(args[i + 1].clone());
                i += 1;
            }
            "--attach-stream-fd-v1" => {
                if i + 1 >= args.len() {
                    eprintln!("pty attach: --attach-stream-fd-v1 requires a file descriptor");
                    return Ok(1);
                }
                stream_fd_token = Some(args[i + 1].clone());
                i += 1;
            }
            _ if name.is_none() => name = Some(a.to_string()),
            _ => {
                eprintln!("pty attach: unexpected argument \"{a}\"");
                return Ok(1);
            }
        }
        i += 1;
    }

    let Some(name) = name else {
        eprintln!("{USAGE}");
        return Ok(1);
    };
    if auto_restart && no_restart {
        eprintln!("pty attach: --auto-restart and --no-restart are mutually exclusive");
        return Ok(1);
    }

    let mut stream_fd = None;
    if let Some(token) = &stream_fd_token {
        match client::parse_attach_stream_fd_token(token) {
            Ok(fd) => stream_fd = Some(fd),
            Err(msg) => {
                eprintln!("pty attach: {msg}");
                return Ok(1);
            }
        }
        if auto_restart {
            eprintln!(
                "pty attach: --attach-stream-fd-v1 and --auto-restart are mutually exclusive"
            );
            return Ok(1);
        }
    }

    // Before the reference is resolved, and for `--remote` too: a nested
    // remote attach tangles the detach keys just the same.
    if let Some(code) = ensure_not_nested("attach", force, Some(NESTING_HINT)) {
        return Ok(code);
    }

    if let Some(peer) = remote {
        // The name belongs to the peer, so it is never resolved here.
        return attach_remote(&peer, &name, stream_fd);
    }

    let resolved = resolve_ref(&name)?;
    let policy = if stream_fd.is_some() || no_restart {
        RestartPolicy::Never
    } else if auto_restart {
        RestartPolicy::Always
    } else {
        RestartPolicy::Prompt
    };
    attach_session(&resolved, policy, stream_fd)
}

/// `cmdAttach`.
fn attach_session(
    name: &str,
    policy: RestartPolicy,
    stream_fd: Option<std::os::fd::RawFd>,
) -> CliResult {
    let Some(session) = registry::get_session_by_name(name) else {
        eprintln!("Session \"{name}\" not found.");
        return Ok(1);
    };

    if session.status == SessionStatus::Running {
        return Ok(do_attach(name, stream_fd));
    }

    // A relay or a supervisor asked to attach only. Refuse before the dead
    // session's stored command is ever considered.
    if policy == RestartPolicy::Never {
        eprintln!(
            "Session \"{name}\" is not running (status: {}).",
            session.status.as_str()
        );
        return Ok(1);
    }

    handle_dead_session(name, session.metadata, policy == RestartPolicy::Always)
}

/// Show what the session last printed, then offer to start it again.
///
/// node: src/cli.ts:1808-1853
fn handle_dead_session(
    name: &str,
    meta: Option<SessionMetadata>,
    auto_restart: bool,
) -> CliResult {
    let Some(meta) = meta else {
        eprintln!("Session \"{name}\" exited (no metadata available).");
        let _ = registry::cleanup_all(name);
        return Ok(1);
    };

    if let Some(lines) = &meta.last_lines
        && !lines.is_empty()
    {
        println!();
        for line in lines {
            println!("  {line}");
        }
        println!();
    }

    let code = meta
        .exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("Session \"{name}\" exited with code {code}.");

    // Node joins displayCommand with args, which repeats them for a record
    // whose displayCommand already contains them. Keep the quirk: relays
    // read this line.
    let command = std::iter::once(meta.display_command.clone())
        .chain(meta.args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    println!("Command was: {command}");
    println!();

    if !auto_restart && super::ask::declined(&ask::ask("Restart? [Y/n] ")) {
        return Ok(0);
    }

    let _ = registry::cleanup_all(name);
    let mut params = SpawnParams::new(name, &meta.command, &meta.args);
    params.display_command = meta.display_command.clone();
    params.cwd = meta.cwd.clone();
    params.tags = meta.tags.clone().unwrap_or_default();
    params.display_name = meta.display_name.clone();
    apply_persisted_launch_options(&mut params, &meta);
    params.scrub_env = RESTART_SCRUBBED_ENV.iter().map(|s| s.to_string()).collect();
    if let Err(msg) = super::spawn_daemon(&params) {
        eprintln!("pty attach: {msg}");
        return Ok(1);
    }
    println!("Session \"{name}\" restarted.");
    Ok(do_attach(name, None))
}

/// `pty attach --remote <peer> <name>`: dial the peer, route to the session,
/// and attach over the tunnel.
///
/// A transport failure is retried, because the peer may simply be away. A
/// route the peer REFUSES is final: the session is gone and no amount of
/// waiting brings it back.
///
/// node: src/cli.ts:1046-1048, `cmdAttachRemote`
fn attach_remote(peer: &str, name: &str, stream_fd: Option<std::os::fd::RawFd>) -> CliResult {
    let (socket, row) = match client::remote::dial_route_and_describe(peer, name) {
        Ok(routed) => routed,
        Err(e) => {
            eprintln!("pty attach --remote {peer}: {e}");
            return Ok(1);
        }
    };
    let summary = row.as_ref().map(SessionSummary::from_remote_row);
    let peer = peer.to_string();
    let target = name.to_string();
    let mut params = client::AttachParams::new(name, socket);
    params.remote = true;
    params.stream_fd = stream_fd;
    params.peer = Some(peer.clone());
    params.reconnect = Some(Box::new(move || {
        match client::remote::dial_and_route(&peer, &target) {
            Ok(s) => Ok(Some(s)),
            Err(client::remote::RemoteError::Refused(refusal)) => Err(refusal),
            // Anything else is the transport being unavailable for now.
            Err(_) => Ok(None),
        }
    }));
    if stream_fd.is_none() {
        eprint!("{}", attach_banner(name, summary.as_ref()));
    }
    params.summary = Some(fixed_summary_provider(summary));
    Ok(client::attach(params, &client::ClientIo::default()).exit_code())
}

/// Connect a terminal to a running session. `restart` reuses this for the
/// attach it does after a successful respawn.
pub fn do_attach(name: &str, stream_fd: Option<std::os::fd::RawFd>) -> i32 {
    let socket = match client::connect_session(name) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let mut params = client::AttachParams::new(name, socket);
    params.stream_fd = stream_fd;
    let mut summary = local_summary_provider(name);
    if stream_fd.is_none() {
        eprint!("{}", attach_banner(name, summary().as_ref()));
    }
    params.summary = Some(summary);
    client::attach(params, &client::ClientIo::default()).exit_code()
}
