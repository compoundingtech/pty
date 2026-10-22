//! `pty up [<dir>] [<name>...]`: start the sessions a `pty.toml` declares.
//! An existing session is bound to a manifest entry by the tag pair
//! (`ptyfile`, `ptyfile.session`), not by its name; a bound running session
//! has its tags synced from the manifest instead of being restarted.
//!
//! node: src/cli.ts:1551-1568 (parsing), 3586-3593 (`hasPtyFile`), 3594-3774 (`cmdUp`)

use std::path::Path;

use pty_core::ptyfile::{self, PtyFile, PtySessionDef};
use pty_core::registry::{
    self, GC_BOOKKEEPING_KEYS, SessionInfo, TagMap, all_session_names, random_session_name,
    update_tags, validate_display_name, validate_name,
};

use super::{CliError, CliResult, SpawnParams};

/// Is `<dir>/pty.toml` a file?
///
/// node: src/cli.ts:3586-3593
pub fn has_pty_file(dir: &str) -> bool {
    let resolved = if Path::new(dir).is_absolute() {
        Path::new(dir).to_path_buf()
    } else {
        std::env::current_dir()
            .map(|c| c.join(dir))
            .unwrap_or_else(|_| Path::new(dir).to_path_buf())
    };
    resolved.join("pty.toml").is_file()
}

/// `pty up [dir] [name...]`: the first token is the manifest dir only when
/// it holds a `pty.toml`; the scan stops at the first dash token.
///
/// node: src/cli.ts:1551-1568, 1570-1587
pub fn split_dir_and_names(args: &[String]) -> (Option<String>, Vec<String>) {
    let mut dir: Option<String> = None;
    let mut names: Vec<String> = Vec::new();
    for arg in args {
        if arg.starts_with('-') {
            break;
        }
        if dir.is_none() && names.is_empty() && has_pty_file(arg) {
            dir = Some(arg.clone());
        } else {
            names.push(arg.clone());
        }
    }
    (dir, names)
}

/// Read the manifest, printing its error and exiting 1 like Node's
/// `readPtyFile` catch.
pub(crate) fn read_manifest(dir: Option<&str>) -> Result<PtyFile, CliError> {
    ptyfile::read_pty_file(dir.map(Path::new)).map_err(CliError)
}

/// The manifest path as the tags record it.
pub(crate) fn toml_path(file: &PtyFile) -> String {
    file.dir.join("pty.toml").to_string_lossy().into_owned()
}

/// The session bound to `(toml_path, short_name)` by its tags.
pub(crate) fn find_bound<'a>(
    existing: &'a [SessionInfo],
    toml_path: &str,
    short_name: &str,
) -> Option<&'a SessionInfo> {
    existing.iter().find(|s| {
        let tags = s.metadata.as_ref().and_then(|m| m.tags.as_ref());
        tags.and_then(|t| t.get("ptyfile")).map(String::as_str) == Some(toml_path)
            && tags
                .and_then(|t| t.get("ptyfile.session"))
                .map(String::as_str)
                == Some(short_name)
    })
}

/// `displayName ?? name`.
pub(crate) fn label_of(s: &SessionInfo) -> String {
    s.display_name()
        .map(str::to_string)
        .unwrap_or_else(|| s.name.clone())
}

/// `cmdUp`.
pub fn run(args: &[String]) -> CliResult {
    let (dir, names) = split_dir_and_names(args);
    cmd_up(dir.as_deref(), &names)
}

fn cmd_up(dir: Option<&str>, names: &[String]) -> CliResult {
    let file = read_manifest(dir)?;
    let mut sessions: Vec<&PtySessionDef> = file.sessions.iter().collect();
    if !names.is_empty() {
        let unknown: Vec<&String> = names
            .iter()
            .filter(|n| {
                !sessions
                    .iter()
                    .any(|s| &s.display_name == *n || &s.short_name == *n)
            })
            .collect();
        if !unknown.is_empty() {
            eprintln!(
                "Unknown session{}: {}",
                if unknown.len() > 1 { "s" } else { "" },
                unknown
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            eprintln!(
                "Available: {}",
                sessions
                    .iter()
                    .map(|s| s.short_name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            return Ok(1);
        }
        sessions.retain(|s| names.contains(&s.display_name) || names.contains(&s.short_name));
    }

    let toml_path = toml_path(&file);
    let existing = registry::list_sessions();
    let mut all_names = all_session_names();

    let mut started = 0usize;
    let mut skipped = 0usize;

    for sess in &sessions {
        let mut user_toml_keys: Vec<String> = sess
            .tags
            .as_ref()
            .map(|t| t.keys().cloned().collect())
            .unwrap_or_default();
        user_toml_keys.sort();
        let mut toml_tags: TagMap = sess.tags.clone().unwrap_or_default();
        toml_tags.insert("ptyfile".to_string(), toml_path.clone());
        toml_tags.insert("ptyfile.session".to_string(), sess.short_name.clone());
        toml_tags.insert("ptyfile.tags".to_string(), user_toml_keys.join(","));

        let bound = find_bound(&existing, &toml_path, &sess.short_name);
        if let Some(bound) = bound
            && bound.identity == registry::DaemonIdentityCheck::Unknown
        {
            eprintln!("  \u{2717} {}: daemon identity is unknown.", label_of(bound));
            skipped += 1;
            continue;
        }

        if let Some(bound) = bound
            && bound.is_running()
        {
            // Sync tags from the manifest to the running session. Keys the
            // previous `ptyfile.tags` listed but the manifest no longer
            // declares are removed; manually added tags are preserved.
            let current: TagMap = bound
                .metadata
                .as_ref()
                .and_then(|m| m.tags.clone())
                .unwrap_or_default();
            let updates: TagMap = toml_tags
                .iter()
                .filter(|(k, v)| current.get(*k) != Some(*v))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let mut removals: Vec<String> = current
                .get("ptyfile.tags")
                .map(String::as_str)
                .unwrap_or("")
                .split(',')
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .filter(|k| !user_toml_keys.iter().any(|u| u == k))
                .map(str::to_string)
                .collect();
            // A manual `pty up` is an operator reset: drop gc bookkeeping.
            for k in GC_BOOKKEEPING_KEYS {
                if current.contains_key(*k) && !removals.iter().any(|r| r == k) {
                    removals.push((*k).to_string());
                }
            }
            let label = label_of(bound);
            if !updates.is_empty() || !removals.is_empty() {
                match update_tags(&bound.name, &updates, &removals) {
                    Ok(_) => {
                        let mut changed: Vec<String> = updates
                            .iter()
                            .filter(|(k, _)| {
                                k.as_str() != "ptyfile"
                                    && k.as_str() != "ptyfile.session"
                                    && k.as_str() != "ptyfile.tags"
                            })
                            .map(|(k, v)| format!("{k}={v}"))
                            .collect();
                        changed.extend(removals.iter().map(|k| format!("-{k}")));
                        if changed.is_empty() {
                            println!("  \u{25cf} {label} (already running)");
                        } else {
                            println!(
                                "  \u{25cf} {label} (already running, updated tags: {})",
                                changed.join(", ")
                            );
                        }
                    }
                    Err(_) => println!("  \u{25cf} {label} (already running)"),
                }
            } else {
                println!("  \u{25cf} {label} (already running)");
            }
            skipped += 1;
            continue;
        }

        // Clean up an exited bound session so its slot can be reused.
        if let Some(bound) = bound
            && bound.is_gone()
        {
            let label = label_of(bound);
            if let Err(error) = pty_client::remove(&bound.name) {
                eprintln!("  \u{2717} {label}: {error}");
                skipped += 1;
                continue;
            }
        }

        // The on-disk id: the manifest's `id`, else a random one.
        let name = match &sess.id {
            Some(id) => {
                if let Err(e) = validate_name(id) {
                    eprintln!("  \u{2717} {}: {e}", sess.display_name);
                    continue;
                }
                if all_names.contains(id) {
                    eprintln!(
                        "  \u{2717} {}: id \"{id}\" is already in use.",
                        sess.display_name
                    );
                    continue;
                }
                all_names.insert(id.clone());
                id.clone()
            }
            None => {
                let mut candidate: Option<String> = None;
                for _ in 0..8 {
                    let c = random_session_name();
                    if !all_names.contains(&c) {
                        all_names.insert(c.clone());
                        candidate = Some(c);
                        break;
                    }
                }
                let Some(c) = candidate else {
                    eprintln!(
                        "  \u{2717} {}: could not generate a unique session id after 8 attempts.",
                        sess.display_name
                    );
                    continue;
                };
                c
            }
        };

        if let Err(e) = validate_display_name(&sess.display_name) {
            eprintln!("  \u{2717} {}: {e}", sess.display_name);
            continue;
        }

        let mut params =
            SpawnParams::new(&name, "/bin/sh", &["-c".to_string(), sess.command.clone()]);
        params.display_command = sess.command.clone();
        params.cwd = sess
            .cwd
            .clone()
            .unwrap_or_else(|| file.dir.to_string_lossy().into_owned());
        params.tags = toml_tags;
        params.display_name = Some(sess.display_name.clone());
        if let Some(env) = &sess.env
            && !env.is_empty()
        {
            params.extra_env = env.clone();
        }
        match super::spawn_daemon(&params) {
            Ok(()) => {
                println!("  \u{25cf} {} (started)", sess.display_name);
                started += 1;
            }
            Err(e) => eprintln!("  \u{2717} {}: {e}", sess.display_name),
        }
    }

    if started == 0 && skipped == sessions.len() {
        println!("All sessions already running.");
    } else if started > 0 {
        println!(
            "Started {started} session{}.",
            if started == 1 { "" } else { "s" }
        );
    }
    Ok(0)
}
