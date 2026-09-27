//! `pty send`: write bytes into a session's terminal.
//!
//! Two shapes: one positional string, or an ordered list of `--seq` chunks
//! where a chunk may name a key. Every argument is checked before the
//! session is resolved, so a bad flag reports the flag rather than a
//! missing session.
//!
//! node: src/cli.ts:1109-1217, src/client.ts:221-288

use pty_client as client;
use pty_core::keys::parse_seq_value;

use super::{CliResult, resolve_ref};

const USAGE: &str = "Usage: pty send [--remote <peer>] <name> \"text\"  or  pty send <name> --seq \"text\" --seq key:return";

/// The flags people reach for when they mean "and press Enter".
///
/// node: src/cli.ts:1150-1158
const ENTER_FLAGS: [&str; 4] = ["--enter", "--newline", "--return", "--cr"];

/// `cmdSend`.
pub fn run(args: &[String]) -> CliResult {
    let mut rest: Vec<String> = args.to_vec();

    // 1. `--remote <peer>` comes out from anywhere.
    let mut remote: Option<String> = None;
    if let Some(idx) = rest.iter().position(|a| a == "--remote") {
        let Some(peer) = rest.get(idx + 1).cloned().filter(|p| !p.starts_with('-')) else {
            return Err("pty send --remote requires a <peer>.".into());
        };
        remote = Some(peer);
        rest.drain(idx..idx + 2);
    }

    // 2. The first token left is the session reference.
    if rest.is_empty() {
        return Err(USAGE.into());
    }
    let reference = rest.remove(0);

    // 3. `--paste` comes out from anywhere after the reference.
    let paste = rest.iter().any(|a| a == "--paste");
    rest.retain(|a| a != "--paste");

    // 4. `--with-delay` counts only as the first token after the reference.
    //    Anywhere else it is an unexpected argument.
    let mut delay_secs: Option<f64> = None;
    if rest.first().map(String::as_str) == Some("--with-delay") {
        let value = rest.get(1).cloned().unwrap_or_default();
        match value.parse::<f64>() {
            Ok(v) if v >= 0.0 && v.is_finite() => delay_secs = Some(v),
            _ => {
                return Err("--with-delay requires a non-negative number (seconds).".into());
            }
        }
        rest.drain(0..2.min(rest.len()));
    }

    // 5. The two shapes are exclusive. A single-dash token counts as text.
    let has_seq = rest.iter().any(|a| a == "--seq");
    let has_positional = rest.first().is_some_and(|t| !t.starts_with("--"));
    if has_seq && has_positional {
        return Err("Cannot mix positional text with --seq flags.".into());
    }

    // 6. The "press Enter for me" flags do not exist; say what does.
    if let Some(flag) = rest.iter().find(|a| ENTER_FLAGS.contains(&a.as_str())) {
        return Err(format!(
            "Unknown flag \"{flag}\". Use `--seq \"<text>\" --seq key:return` to send text followed by Enter."
        )
        .into());
    }

    // 7 and 8. Collect the payload, rejecting anything unexpected. Every
    //    `--seq` value is resolved before one byte is delivered.
    let mut items: Vec<Vec<u8>> = Vec::new();
    if has_seq {
        let mut i = 0;
        while i < rest.len() {
            if rest[i] == "--seq" {
                let Some(value) = rest.get(i + 1) else {
                    return Err("--seq requires a value.".into());
                };
                match parse_seq_value(value) {
                    Ok(bytes) => items.push(bytes.into_bytes()),
                    Err(e) => return Err(e.to_string().into()),
                }
                i += 2;
            } else {
                return Err(format!("Unexpected argument: {}", rest[i]).into());
            }
        }
    } else if has_positional {
        if let Some(extra) = rest.get(1) {
            return Err(format!("Unexpected argument: {extra}").into());
        }
        items.push(rest[0].clone().into_bytes());
    } else if let Some(unexpected) = rest.first() {
        return Err(format!("Unexpected argument: {unexpected}").into());
    }

    // 9. Nothing to write is an error, not a silent success.
    if items.is_empty() {
        return Err("Nothing to send.".into());
    }

    // 10. No `--with-delay` means 300 ms between items; `--with-delay 0`
    //     streams them with no gap.
    let opts = client::SendOptions {
        delay_ms: client::resolve_seq_delay_ms(delay_secs),
        paste,
    };

    // 11. Only now does the reference have to name a real session.
    if let Some(peer) = remote {
        // The reference names a session on the PEER, so it is never resolved
        // here; the peer's own registry does that.
        let socket = match client::remote::dial_and_route(&peer, &reference) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("pty send --remote {peer}: {e}");
                return Ok(1);
            }
        };
        return match client::send_over(socket, &reference, true, &items, opts) {
            Ok(()) => Ok(0),
            Err(e) => {
                eprintln!("{e}");
                Ok(1)
            }
        };
    }
    let name = resolve_ref(&reference)?;
    match client::send(&name, &items, opts) {
        Ok(()) => Ok(0),
        Err(e) => {
            eprintln!("{e}");
            Ok(1)
        }
    }
}
