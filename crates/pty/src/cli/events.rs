//! `pty events [--all] [--recent] [--json] [--type <type>] [--wait <type>]
//! [-t <sec>] [<ref>]`: print a session's recent events, optionally filtered
//! by exact type, follow one log (or every log with `--all`), or block until
//! one event type appears.
//!
//! node: src/cli.ts:1219-1248 (parsing), 3965-4051 (`cmdEvents`)

use std::io::{self, Write};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use pty_core::events::follow::{EventFollower, FollowerOptions};
use pty_core::events::{
    DEFAULT_RECENT_EVENTS, Event, format_event, read_recent_events, read_retained_events_of_type,
};
use pty_core::registry;

use super::argv::{Argv, js_number, js_parse_float};
use super::{CliError, CliResult, resolve_ref};

const USAGE: &str = "Usage: pty events [--all] [--recent] [--json] [--type <type>] [--wait <type>] [-t <seconds>] [<name>]";

fn render(event: &Event, json: bool) -> String {
    if json {
        event.to_json()
    } else {
        format_event(event)
    }
}

extern "C" fn on_sigint(_sig: libc::c_int) {
    // SAFETY: `_exit` is async-signal-safe; Node exits 0 on SIGINT here.
    unsafe { libc::_exit(0) }
}

/// SIGINT ends a follow with exit 0.
///
/// node: src/cli.ts:4028-4031, 4044-4047
fn exit_zero_on_sigint() {
    // SAFETY: installing a handler that only calls `_exit`.
    let handler: extern "C" fn(libc::c_int) = on_sigint;
    unsafe {
        libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
    }
}

const OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const VANISHED_EXIT_CODE: i32 = 2;

/// Polling with no requested events still reports a pipe's lost reader.
/// Unlike a zero-byte write, this detects closure even while events are idle.
fn output_closed() -> bool {
    let mut fd = libc::pollfd { fd: libc::STDOUT_FILENO, events: 0, revents: 0 };
    // SAFETY: `fd` points to one initialized poll descriptor.
    (unsafe { libc::poll(&mut fd, 1, 0) }) > 0
        && fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
}

fn print_event(event: &Event, json: bool) -> Result<bool, CliError> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    match writeln!(output, "{}", render(event, json)).and_then(|()| output.flush()) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(error) => Err(CliError(format!("Could not write events: {error}"))),
    }
}

fn current_event(event: &Event, metadata: &registry::SessionMetadata) -> bool {
    event.ts >= metadata.created_at
}

/// Logs are cleared on replacement publication; the creation timestamp also
/// fences retained legacy records. Pin the opaque generation across reads so a
/// replacement with the same stable name cannot satisfy an old observer.
fn recorded_event(
    name: &str,
    wait_type: &str,
    fence: &registry::SessionMetadata,
) -> Option<Event> {
    let metadata = registry::read_metadata(name)?;
    if metadata.generation != fence.generation || metadata.created_at != fence.created_at {
        return None;
    }
    let event = read_retained_events_of_type(name, wait_type)
        .into_iter()
        .rev()
        .find(|event| current_event(event, fence))
        .or_else(|| {
            if wait_type != pty_core::events::event_type::SESSION_EXIT {
                return None;
            }
            let code = metadata.exit_code?;
            let timestamp = metadata.exited_at.as_ref()?;
            let event = Event::session_exit(name, code, None).with_ts(timestamp);
            current_event(&event, fence).then_some(event)
        });
    let after = registry::read_metadata(name)?;
    (after.generation == fence.generation && after.created_at == fence.created_at)
        .then_some(event)
        .flatten()
}

fn target_vanished(name: &str, fence: &registry::SessionMetadata) -> bool {
    registry::get_session_by_name(name).is_none_or(|session| {
        session.metadata.as_ref().is_none_or(|metadata| {
            metadata.generation != fence.generation || metadata.created_at != fence.created_at
        }) || session.status == registry::SessionStatus::Vanished
    })
}

fn vanished(name: &str) -> CliResult {
    eprintln!("Session \"{name}\" vanished without a recorded exit.");
    Ok(VANISHED_EXIT_CODE)
}

/// Parse and run.
pub fn run(args: &[String]) -> CliResult {
    let mut all = false;
    let mut recent = false;
    let mut json = false;
    let mut wait_type: Option<String> = None;
    let mut event_type: Option<&str> = None;
    let mut timeout = 0f64;
    let mut cur = Argv::new(args);
    let mut reference = None;
    // Options may precede or follow the session reference.
    while !cur.is_empty() {
        match cur.peek() {
            Some("--all") => {
                all = true;
                cur.next();
            }
            Some("--recent") => {
                recent = true;
                cur.next();
            }
            Some("--json") => {
                json = true;
                cur.next();
            }
            Some("--type") if cur.has_next() => {
                event_type = cur.take_value();
            }
            Some("--type") => return Err("--type requires an event type.".into()),
            Some("--wait") if cur.has_next() => {
                wait_type = cur.take_value().map(str::to_string);
            }
            Some("-t") | Some("--timeout") if cur.has_next() => {
                timeout = js_parse_float(cur.take_value().unwrap_or_default());
            }
            _ if reference.is_none() => reference = cur.next(),
            // Ignoring it would also drop every option after it, such as a
            // deadline (`--timeout=60`) or the wait itself (`--wait=<type>`).
            // An unknown dash token taken as the reference is the stray one
            // when a name follows it (`--timeout=60 NAME`).
            Some(unexpected) => {
                let stray = reference.filter(|r| r.starts_with('-')).unwrap_or(unexpected);
                return Err(format!("Unexpected argument: {stray}").into());
            }
            None => break,
        }
    }

    if !all && reference.is_none() {
        return Err(USAGE.into());
    }
    let name = match reference {
        Some(r) => Some(resolve_ref(r)?),
        None => None,
    };
    cmd_events(
        name.as_deref(),
        all,
        recent,
        json,
        event_type,
        wait_type.as_deref(),
        timeout,
    )
}

/// `cmdEvents`.
fn cmd_events(
    name: Option<&str>,
    all: bool,
    recent: bool,
    json: bool,
    event_type: Option<&str>,
    wait_type: Option<&str>,
    timeout: f64,
) -> CliResult {
    if event_type.is_some() && !recent {
        return Err("--type requires --recent.".into());
    }
    if recent {
        let Some(name) = name else {
            return Err("--recent requires a session name.".into());
        };
        let events = match event_type {
            Some(expected) => read_retained_events_of_type(name, expected),
            None => read_recent_events(name, DEFAULT_RECENT_EVENTS),
        };
        let metadata = registry::read_metadata(name);
        let events: Vec<_> = events.into_iter().filter(|event| {
            metadata.as_ref().is_none_or(|metadata| current_event(event, metadata))
        }).collect();
        if events.is_empty() && event_type.is_none() {
            println!("No recent events for \"{name}\".");
            return Ok(0);
        }
        for event in &events {
            if !print_event(event, json)? {
                return Ok(0);
            }
        }
        return Ok(0);
    }

    // Follow mode: re-check the session exists when a name was given.
    if let Some(name) = name
        && registry::get_session(name).map_err(CliError)?.is_none()
    {
        return Err(CliError(format!("Session \"{name}\" not found.")));
    }

    let deadline = (timeout > 0.0).then(|| {
        Instant::now() + Duration::from_secs_f64(timeout.min(u32::MAX as f64))
    });

    if let Some(wait_type) = wait_type {
        let Some(name) = name else {
            return Err("--wait requires a session name.".into());
        };
        let Some(fence) = registry::read_metadata(name) else {
            return vanished(name);
        };
        exit_zero_on_sigint();
        // The channel constructor waits for the follower to establish its EOF
        // offsets. Checking durable history only after this handshake leaves no
        // gap between replay and subscription.
        let (_follower, rx) = EventFollower::channel(FollowerOptions::names(vec![name.to_string()]));
        if let Some(event) = recorded_event(name, wait_type, &fence) {
            print_event(&event, json)?;
            return Ok(0);
        }
        loop {
            if output_closed() {
                return Ok(0);
            }
            if target_vanished(name, &fence) {
                if let Some(event) = recorded_event(name, wait_type, &fence) {
                    print_event(&event, json)?;
                    return Ok(0);
                }
                return vanished(name);
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                // Recheck durable history before declaring a timeout:
                // filesystem notifications can be delayed.
                if let Some(event) = recorded_event(name, wait_type, &fence) {
                    print_event(&event, json)?;
                    return Ok(0);
                }
                if target_vanished(name, &fence) {
                    return vanished(name);
                }
                eprintln!(
                    "Timed out after {}s waiting for \"{wait_type}\" event.",
                    js_number(timeout)
                );
                return Ok(1);
            }
            let interval = deadline.map_or(OUTPUT_POLL_INTERVAL, |deadline| {
                deadline.saturating_duration_since(Instant::now()).min(OUTPUT_POLL_INTERVAL)
            });
            match rx.recv_timeout(interval) {
                Ok(event) if event.r#type == wait_type && current_event(&event, &fence) => {
                    // A stable name may have been republished while the
                    // event was queued. Never satisfy the original waiter
                    // with a replacement incarnation.
                    if target_vanished(name, &fence) {
                        return vanished(name);
                    }
                    print_event(&event, json)?;
                    return Ok(0);
                }
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    if let Some(event) = recorded_event(name, wait_type, &fence) {
                        print_event(&event, json)?;
                        return Ok(0);
                    }
                    if target_vanished(name, &fence) {
                        return vanished(name);
                    }
                    eprintln!("Session \"{name}\" ended before a \"{wait_type}\" event was observed.");
                    return Ok(1);
                }
            }
        }
    }

    exit_zero_on_sigint();
    let options = match (all, name) {
        (false, Some(name)) => FollowerOptions::names(vec![name.to_string()]),
        _ => FollowerOptions::all(),
    };
    let (_follower, rx) = EventFollower::channel(options);
    loop {
        if output_closed() {
            return Ok(0);
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            eprintln!("Timed out after {}s following events.", js_number(timeout));
            return Ok(1);
        }
        let interval = deadline.map_or(OUTPUT_POLL_INTERVAL, |deadline| {
            deadline.saturating_duration_since(Instant::now()).min(OUTPUT_POLL_INTERVAL)
        });
        match rx.recv_timeout(interval) {
            Ok(event) => {
                if !print_event(&event, json)? {
                    return Ok(0);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(0),
        }
    }
}
