//! `pty down [<dir>] [<name>...]`: stop the sessions a `pty.toml` declares,
//! matched by the (`ptyfile`, `ptyfile.session`) tag pair.
//!
//! node: src/cli.ts:1570-1587 (parsing), 3776-3845 (`cmdDown`)

use pty_core::registry;

use super::CliResult;
use super::up::{find_bound, label_of, read_manifest, split_dir_and_names, toml_path};

/// `cmdDown`.
pub fn run(args: &[String]) -> CliResult {
    let (dir, names) = split_dir_and_names(args);
    let file = read_manifest(dir.as_deref())?;
    let sessions: Vec<_> = file
        .sessions
        .iter()
        .filter(|s| {
            names.is_empty() || names.contains(&s.display_name) || names.contains(&s.short_name)
        })
        .collect();

    let toml_path = toml_path(&file);
    let existing = registry::list_sessions();
    let mut stopped = 0usize;
    let mut failed = false;

    for sess in &sessions {
        let Some(existing_session) = find_bound(&existing, &toml_path, &sess.short_name) else {
            continue;
        };
        let label = label_of(existing_session);
        if existing_session.is_running() {
            let generation = existing_session
                .metadata
                .as_ref()
                .and_then(|m| m.generation.as_deref());
            match pty_client::stop(&existing_session.name, generation) {
                Ok(result) if result.verified_empty() => {
                    println!(
                        "  \u{25cb} {label} (stopped{})",
                        if result.was_permanent {
                            ", removed from supervision"
                        } else {
                            ""
                        }
                    );
                    stopped += 1;
                }
                Ok(_) => {
                    eprintln!(
                        "  \u{2717} {label}: daemon stopped, but process tree could not be verified empty."
                    );
                    failed = true;
                }
                Err(error) => {
                    eprintln!("  \u{2717} {label}: {error}");
                    failed = true;
                }
            }
        } else if existing_session.is_gone() {
            match pty_client::remove(&existing_session.name) {
                Ok(()) => {
                    println!("  \u{25cb} {label} (cleaned up)");
                    stopped += 1;
                }
                Err(error) => {
                    eprintln!("  \u{2717} {label}: {error}");
                    failed = true;
                }
            }
        }
    }

    if stopped == 0 {
        println!("No sessions to stop.");
    } else {
        println!(
            "Stopped {stopped} session{}.",
            if stopped == 1 { "" } else { "s" }
        );
    }

    let any_toml_managed = sessions.iter().any(|sess| {
        find_bound(&existing, &toml_path, &sess.short_name)
            .and_then(|s| s.metadata.as_ref())
            .and_then(|m| m.tags.as_ref())
            .and_then(|t| t.get("ptyfile"))
            .is_some_and(|p| !p.is_empty())
    });
    if any_toml_managed && stopped > 0 {
        eprintln!("\nNote: strategy tags will be restored on the next 'pty up'.");
    }
    Ok(if failed { 1 } else { 0 })
}
