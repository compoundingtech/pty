//! `pty kill <name>`: stop a session's daemon and keep its exit evidence.
//! The stop itself is `pty_client::stop`; this prints what it verified.
//!
//! node: src/cli.ts:1384-1392 (dispatch), 2618-2671 (`cmdKill`)

use pty_client::Stopped;

use super::{CliResult, require_ref};

/// `cmdKill`.
pub fn run(args: &[String]) -> CliResult {
    let name = require_ref(args, "Usage: pty kill <name>")?;
    let stopped = match pty_client::stop(&name) {
        Ok(stopped) => stopped,
        Err(error) => {
            eprintln!("{error}");
            return Ok(1);
        }
    };
    let verified_empty = stopped.verified_empty();
    report(&stopped);

    if let Some(path) = &stopped.ptyfile {
        eprintln!("Note: this session is managed by {path}");
        eprintln!("The strategy tag will be restored on the next 'pty up'.");
    }
    // Anything left is a failure, and the status says so.
    Ok(if verified_empty { 0 } else { 1 })
}

/// Say what was verified, and nothing more.
///
/// `killed` is now a claim about the whole tree, so it is printed only when
/// every process in the snapshot is gone. Otherwise stdout gets the part that
/// was verified — the daemon stopped — and stderr gets what survived it.
fn report(stopped: &Stopped) {
    let Stopped {
        name,
        aftermath: after,
        escalated,
        ..
    } = stopped;
    let escalated = escalated.as_deref();
    if stopped.verified_empty() {
        match escalated {
            // The daemon left something behind and the escalation cleared it.
            // Say so: a silent success here would hide that the teardown needed
            // a second pass, which is the fact somebody debugging wants.
            Some(_) => {
                println!("Session \"{name}\" killed (the escalation stopped the remainder).")
            }
            None => println!("Session \"{name}\" killed."),
        }
        return;
    }
    println!("Session \"{name}\" daemon stopped.");
    if let Some(still_there) = escalated
        && !still_there.is_empty()
    {
        eprintln!(
            "Session \"{name}\": {} process(es) survived SIGKILL to their process group: {}",
            still_there.len(),
            join_pids(still_there)
        );
    }
    if !after.survived.is_empty() {
        eprintln!(
            "Session \"{name}\": {} process(es) survived the kill and are still running: {}",
            after.survived.len(),
            join_pids(&after.survived)
        );
    }
    if !after.unknown.is_empty() {
        eprintln!(
            "Session \"{name}\": {} process(es) may still be running: {}. \
             Their start tokens could not be read, so this is not a conclusion.",
            after.unknown.len(),
            join_pids(&after.unknown)
        );
    }
}

fn join_pids(pids: &[i32]) -> String {
    pids.iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pids_render_in_snapshot_order() {
        assert_eq!(join_pids(&[300, 200, 100]), "300, 200, 100");
    }
}
