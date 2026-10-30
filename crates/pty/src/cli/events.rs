//! `pty events [--all] [--recent] [--json] [--type <type>] [--wait <type>]
//! [-t <sec>] [<ref>]`: print a session's recent events, optionally filtered
//! by exact type, follow one log (or every log with `--all`), or block until
//! one event type appears.
//!
//! node: src/cli.ts:1219-1248 (parsing), 3965-4051 (`cmdEvents`)

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

/// Lifecycle waits also observe durable exit state: the follower deliberately
/// starts existing logs at EOF, so a completed exit is not a new event.
fn print_recorded_exit(name: &str, wait_type: &str, json: bool) -> bool {
    if wait_type != pty_core::events::event_type::SESSION_EXIT {
        return false;
    }
    let Some(metadata) = registry::read_metadata(name) else {
        return false;
    };
    if !metadata.has_exited() && metadata.exit_code.is_none() {
        return false;
    }
    if let Some(event) = read_retained_events_of_type(name, wait_type).pop() {
        println!("{}", render(&event, json));
    } else if let Some(code) = metadata.exit_code {
        // Retention or cleanup can remove the log while exit metadata remains.
        let mut event = Event::session_exit(name, code, None);
        if let Some(timestamp) = metadata.exited_at {
            event.ts = timestamp;
        }
        println!("{}", render(&event, json));
    }
    true
}

fn check_wait_target(name: &str, wait_type: &str) -> Result<(), CliError> {
    match registry::get_session(name).map_err(CliError)? {
        None => Err(CliError(format!("Session \"{name}\" not found."))),
        Some(session)
            if wait_type == pty_core::events::event_type::SESSION_EXIT
                && session.status == registry::SessionStatus::Vanished =>
        {
            Err(CliError(format!("Session \"{name}\" vanished without a recorded exit.")))
        }
        Some(_) => Ok(()),
    }
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
            _ => break,
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
        if events.is_empty() && event_type.is_none() {
            println!("No recent events for \"{name}\".");
            return Ok(0);
        }
        for event in &events {
            println!("{}", render(event, json));
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
        if print_recorded_exit(name, wait_type, json) {
            return Ok(0);
        }
        check_wait_target(name, wait_type)?;
        exit_zero_on_sigint();
        let (_follower, rx) = EventFollower::channel(FollowerOptions::names(vec![name.to_string()]));
        // Close the state-read/subscribe race, including removal before the
        // follower observed the registry entry.
        if print_recorded_exit(name, wait_type, json) {
            return Ok(0);
        }
        check_wait_target(name, wait_type)?;
        loop {
            let recv = match deadline {
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        Err(RecvTimeoutError::Timeout)
                    } else {
                        rx.recv_timeout(d - now)
                    }
                }
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match recv {
                Ok(event) => {
                    if event.r#type == wait_type {
                        println!("{}", render(&event, json));
                        return Ok(0);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    eprintln!(
                        "Timed out after {}s waiting for \"{wait_type}\" event.",
                        js_number(timeout)
                    );
                    return Ok(1);
                }
                Err(RecvTimeoutError::Disconnected) => {
                    if print_recorded_exit(name, wait_type, json) {
                        return Ok(0);
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
        let recv = match deadline {
            Some(d) => {
                let now = Instant::now();
                if now >= d {
                    Err(RecvTimeoutError::Timeout)
                } else {
                    rx.recv_timeout(d - now)
                }
            }
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match recv {
            Ok(event) => println!("{}", render(&event, json)),
            Err(RecvTimeoutError::Timeout) => {
                eprintln!("Timed out after {}s following events.", js_number(timeout));
                return Ok(1);
            }
            Err(RecvTimeoutError::Disconnected) => return Ok(0),
        }
    }
}
