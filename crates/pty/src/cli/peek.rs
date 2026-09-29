//! `pty peek` — interim port kept from the v0 binary. The Node-exact
//! rewrite (cli.ts:1068-1107, `cmdPeek` 1992-2014, `cmdPeekWait`
//! 1941-1990) replaces this module.

use pty_client as client;
use pty_client::summary;
use pty_core::registry;

use super::{CliResult, resolve_ref};

/// `pty peek [--plain] [--full] [-f] [--wait TEXT [-t SECS]] <ref>`
pub fn run(args: &[String]) -> CliResult {
    let mut plain = false;
    let mut full = false;
    let mut follow = false;
    let mut wait: Vec<String> = Vec::new();
    let mut timeout = 5f64;
    let mut reference: Option<String> = None;
    let mut remote: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--plain" | "-p" => {
                plain = true;
                i += 1;
            }
            "--full" => {
                full = true;
                i += 1;
            }
            "--follow" | "-f" => {
                follow = true;
                i += 1;
            }
            "--wait" => {
                if let Some(v) = args.get(i + 1) {
                    wait.push(v.clone());
                }
                i += 2;
            }
            "--remote" => {
                match args.get(i + 1).filter(|p| !p.starts_with('-')) {
                    Some(peer) => remote = Some(peer.clone()),
                    None => {
                        eprintln!("pty peek --remote requires a <peer>.");
                        return Ok(1);
                    }
                }
                i += 2;
            }
            "-t" | "--timeout" => {
                timeout = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(5.0);
                i += 2;
            }
            other => {
                reference = Some(other.to_string());
                i += 1;
            }
        }
    }
    let Some(reference) = reference else {
        eprintln!("Usage: pty peek [--plain] [--full] [-f] [--wait <text>] [-t <seconds>] <name>");
        return Ok(1);
    };
    if let Some(peer) = remote {
        // `--wait` polls the peer repeatedly, which the one-request route
        // cannot do; Node says so rather than hanging.
        if !wait.is_empty() {
            eprintln!("pty peek --wait is not supported with --remote yet.");
            return Ok(1);
        }
        // The reference belongs to the peer, so it stays unresolved here.
        // Only `-f` prints a trailer, so only `-f` asks the peer for the row.
        let routed = if follow {
            client::remote::dial_route_and_describe(&peer, &reference)
        } else {
            client::remote::dial_and_route(&peer, &reference).map(|s| (s, None))
        };
        let (socket, row) = match routed {
            Ok(routed) => routed,
            Err(e) => {
                eprintln!("pty peek --remote {peer}: {e}");
                return Ok(1);
            }
        };
        let params = client::PeekParams {
            name: &reference,
            plain,
            full,
            socket: Some(socket),
            summary: Some(summary::fixed_summary_provider(
                row.as_ref().map(summary::SessionSummary::from_remote_row),
            )),
            peer: Some(peer.clone()),
        };
        let io = client::ClientIo::default();
        let outcome = if follow {
            client::follow(params, &io)
        } else {
            client::peek(params, &io)
        };
        return match outcome {
            Ok(client::PeekOutcome::Exited(code)) => Ok(code.max(0)),
            Ok(_) => Ok(0),
            Err(e) => {
                eprintln!("{e}");
                Ok(1)
            }
        };
    }

    let name = resolve_ref(&reference)?;
    if follow {
        let mut params = client::PeekParams::new(&name);
        params.plain = plain;
        params.full = full;
        params.summary = Some(summary::local_summary_provider(&name));
        return match client::follow(params, &client::ClientIo::default()) {
            Ok(client::PeekOutcome::Exited(code)) => Ok(code.max(0)),
            Ok(_) => Ok(0),
            Err(e) => {
                eprintln!("{e}");
                Ok(1)
            }
        };
    }
    if !wait.is_empty() {
        return match client::peek_wait(&name, &wait, timeout, plain) {
            Ok(screen) => {
                if plain {
                    println!("{screen}");
                } else {
                    println!(
                        "{screen}{}{}",
                        client::TERMINAL_SANITIZE,
                        client::CURSOR_TO_BOTTOM
                    );
                }
                Ok(0)
            }
            Err(e) => {
                eprintln!("{e}");
                Ok(1)
            }
        };
    }
    let mut params = client::PeekParams::new(&name);
    params.plain = plain;
    params.full = full;
    match client::peek(params, &client::ClientIo::default()) {
        Ok(client::PeekOutcome::Exited(code)) => Ok(code.max(0)),
        Ok(_) => Ok(0),
        // Socket gone: node prints the `lastLines` an exited session saved,
        // or says there is no saved output. Both exit 0.
        //
        // Read the record ONCE. A guard that reads it and a body that reads
        // it again are two separate reads, and a `pty rm` between them left
        // the second one with nothing to unwrap. That panicked.
        Err(e @ client::ClientError::NotReachable { .. }) => match registry::read_metadata(&name) {
            Some(meta) => {
                match meta.last_lines.as_deref() {
                    Some(lines) if !lines.is_empty() => {
                        println!("{}", lines.join("\n"));
                    }
                    _ => {
                        let status = if meta.exited_at.is_some() { "exited" } else { "vanished" };
                        eprintln!("Session \"{name}\" has {status} with no saved output.");
                    }
                }
                Ok(0)
            }
            // The record went away under us. That is the same answer as a
            // session that was never there.
            None => {
                eprintln!("{e}");
                Ok(1)
            }
        },
        Err(e) => {
            eprintln!("{e}");
            Ok(1)
        }
    }
}
