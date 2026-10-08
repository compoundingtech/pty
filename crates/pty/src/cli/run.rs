//! `pty run`: create a session and, unless `-d`, attach to it.
//!
//! node: src/cli.ts:767-982 (dispatch and the display-name rules),
//! 1664-1769 (`cmdRun`)

use pty_client as client;
use pty_core::registry::{self, EnvMap, TagMap};

use super::{CliResult, SpawnParams};
use pty_lifecycle::StartupLeaseOptions;

/// `pty run [--id X] [--name X] [--cwd D] [--tag k=v] [--env K=V | K]
/// [--unset-env K] [--isolate-env] [--rows R] [--cols C] -- <cmd...>`
pub fn run(args: &[String]) -> CliResult {
    let mut id: Option<String> = None;
    let mut display_name: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut rows: Option<u16> = None;
    let mut cols: Option<u16> = None;
    let mut background = false;
    let mut force = false;
    let mut ephemeral = false;
    let mut attach_existing = false;
    let mut no_display_name = false;
    let mut isolate_env = false;
    let mut tags = TagMap::new();
    let mut extra_env = EnvMap::new();
    let mut unset_env: Vec<String> = Vec::new();
    let mut startup_timeout_ms: Option<u64> = None;
    let mut lifecycle_tag: Option<String> = None;
    let mut i = 0;
    let mut command: Vec<String> = Vec::new();
    while i < args.len() {
        match args[i].as_str() {
            "--id" => {
                id = args.get(i + 1).cloned();
                i += 2;
            }
            "--name" => {
                display_name = args.get(i + 1).cloned();
                i += 2;
            }
            "--cwd" => {
                cwd = args.get(i + 1).cloned();
                i += 2;
            }
            "--rows" => {
                rows = args.get(i + 1).and_then(|s| s.parse().ok());
                i += 2;
            }
            "--cols" => {
                cols = args.get(i + 1).and_then(|s| s.parse().ok());
                i += 2;
            }
            "-d" | "--detach" => {
                background = true;
                i += 1;
            }
            "--force" => {
                // --force creates even from inside a pty session (bypasses
                // the nesting guard), symmetric with attach's --force.
                force = true;
                i += 1;
            }
            "-e" | "--ephemeral" => {
                ephemeral = true;
                i += 1;
            }
            "--startup-timeout-ms" => {
                let value = args.get(i + 1).and_then(|value| value.parse::<u64>().ok());
                if value.is_none_or(|value| value == 0 || value > 9_007_199_254_740_991) {
                    eprintln!("pty run: --startup-timeout-ms requires a positive integer.");
                    return Ok(1);
                }
                startup_timeout_ms = value;
                i += 2;
            }
            "--lifecycle-tag" => {
                let Some(value) = args.get(i + 1).filter(|value| !value.is_empty()) else {
                    eprintln!("pty run: --lifecycle-tag requires a non-empty key.");
                    return Ok(1);
                };
                lifecycle_tag = Some(value.clone());
                i += 2;
            }
            "--tag" => {
                match args.get(i + 1).and_then(|kv| kv.split_once('=')) {
                    Some((k, v)) => {
                        tags.insert(k.to_string(), v.to_string());
                    }
                    None => {
                        let tok = args.get(i + 1).cloned().unwrap_or_default();
                        eprintln!("Invalid tag format: \"{tok}\". Use --tag key=value");
                        return Ok(1);
                    }
                }
                i += 2;
            }
            // An assignment whose `=` is missing or leading is rejected;
            // `KEY=` with an empty value is accepted. A repeated key keeps
            // its first position and takes the last value. A bare `KEY` is
            // the Rust-only form that reads the value from this process's
            // environment, so a launcher can name a variable without putting
            // its value on a command line.
            //
            // node: src/cli.ts:811-814 (assignment form only)
            "--env" => {
                let tok = args.get(i + 1).cloned().unwrap_or_default();
                if let Err(message) =
                    insert_env_token(&mut extra_env, &tok, &|name| std::env::var_os(name))
                {
                    eprintln!("{message}");
                    return Ok(1);
                }
                i += 2;
            }
            // De-duplicated, order preserved.
            //
            // node: src/cli.ts:820-823
            "--unset-env" => {
                let tok = args.get(i + 1).cloned().unwrap_or_default();
                if tok.is_empty() || tok.contains('=') {
                    eprintln!("Invalid env key: \"{tok}\". Use --unset-env KEY");
                    return Ok(1);
                }
                if !unset_env.contains(&tok) {
                    unset_env.push(tok);
                }
                i += 2;
            }
            "--isolate-env" => {
                isolate_env = true;
                i += 1;
            }
            "-a" | "--attach" => {
                attach_existing = true;
                i += 1;
            }
            "--no-display-name" => {
                no_display_name = true;
                i += 1;
            }
            "--" => {
                command = args[i + 1..].to_vec();
                break;
            }
            _ => {
                // Bare command without `--`.
                command = args[i..].to_vec();
                break;
            }
        }
    }
    if startup_timeout_ms.is_some() && lifecycle_tag.is_none() {
        eprintln!("pty run: --startup-timeout-ms requires --lifecycle-tag.");
        return Ok(1);
    }
    if lifecycle_tag.as_deref() == Some("") {
        eprintln!("pty run: --lifecycle-tag must not be empty.");
        return Ok(1);
    }
    if command.is_empty() {
        eprintln!(
            "Usage: pty run [--id <id>] [--name <displayName>] [-d] [-a] -- <command> [args...]"
        );
        return Ok(1);
    }

    // Nesting prevention: running `pty run` from inside a pty session would
    // create a session-inside-a-session. Run the command directly unless
    // `-d` or `--force` explicitly asks for a real nested session.
    if !background
        && !force
        && std::env::var("PTY_SESSION")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    {
        // A plain nested `run` executes in place. `-a` is narrower: it asked
        // to attach if the target is already running, and attaching would
        // nest a client, so that case refuses instead.
        if attach_existing && let Some(reference) = id.as_ref().or(display_name.as_ref()) {
            let existing = match &id {
                Some(explicit) => registry::get_session_by_name(explicit),
                None => registry::get_session(reference).ok().flatten(),
            };
            if existing.is_some_and(|s| s.status == registry::SessionStatus::Running)
                && let Some(code) = super::ensure_not_nested(
                    "run -a",
                    false,
                    Some(&format!(
                        "  Target session \"{reference}\" is already running; attaching would nest a client inside the current session.\n  Pass --force to attach anyway, or detach first (Ctrl+\\) and re-run from outside."
                    )),
                )
            {
                return Ok(code);
            }
        }
        let nested = std::env::var("PTY_SESSION").unwrap_or_default();
        eprintln!("Already inside pty session \"{nested}\", running directly.");
        let (program, pargs) = command.split_first().unwrap();
        let mut c = std::process::Command::new(program);
        c.args(pargs);
        // The child inherits this process's environment, minus the
        // `--unset-env` keys, plus the `--env` overlays. An assignment beats
        // a removal of the same key, whatever order the flags came in.
        //
        // node: src/cli.ts:907-917
        for key in &unset_env {
            c.env_remove(key);
        }
        for (key, value) in &extra_env {
            c.env(key, value);
        }
        if let Some(dir) = &cwd {
            c.current_dir(dir);
        }
        return Ok(c.status().ok().and_then(|s| s.code()).unwrap_or(1));
    }

    let name = id.clone().unwrap_or_else(registry::generate_id);
    if let Err(e) = registry::validate_name(&name) {
        eprintln!("{e}");
        return Ok(1);
    }
    // Under `-a` a collision with an existing session is the expected path,
    // so the uniqueness check is left to the create step below.
    if !attach_existing && registry::session_exists(&name) && client::is_alive(&name) {
        eprintln!("Session id \"{name}\" is already in use.");
        return Ok(1);
    }

    // Display name precedence: --no-display-name wins, then --name, then a
    // label built from the working directory and the command.
    //
    // node: src/cli.ts:955-974
    let display_name = if no_display_name {
        None
    } else if let Some(explicit) = display_name {
        if let Err(msg) = registry::validate_display_name(&explicit) {
            eprintln!("Invalid displayName: {msg}");
            return Ok(1);
        }
        Some(explicit)
    } else {
        Some(auto_display_name(&command))
    };

    let (program, pargs) = command.split_first().unwrap();
    let mut params = SpawnParams::new(&name, program, pargs);
    params.cwd = cwd
        .clone()
        .unwrap_or_else(|| crate::daemon::default_cwd().to_string_lossy().into_owned());
    if let Some(r) = rows {
        params.rows = r;
    }
    if let Some(c) = cols {
        params.cols = c;
    }
    params.ephemeral = ephemeral;
    params.tags = tags;
    params.display_name = display_name;
    params.isolate_env = isolate_env;
    params.extra_env = extra_env;
    params.unset_env = unset_env;
    params.startup_lease = lifecycle_tag.map(|lifecycle_tag| StartupLeaseOptions {
        timeout_ms: startup_timeout_ms,
        lifecycle_tag,
    });

    create_or_attach(&name, params, cwd.is_some(), background, attach_existing)
}

/// A label built from the working directory and the command, the way Node
/// builds one when `--name` is absent.
///
/// node: src/cli.ts:651-668 (`autoName`), 971-972 (the sanitize pass)
fn auto_display_name(command: &[String]) -> String {
    let dir_part = std::env::current_dir()
        .ok()
        .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let cmd_base = base_name(&command[0]);
    let mut cmd_part = cmd_base.clone();
    if let Some(first_arg) = command[1..]
        .iter()
        .find(|a| !a.starts_with('-') && a.chars().count() < 30)
    {
        let arg_base = strip_extension(&base_name(first_arg));
        if !arg_base.is_empty()
            && arg_base
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            cmd_part = format!("{cmd_base}-{arg_base}");
        }
    }
    sanitize_label(&format!("{dir_part}-{cmd_part}"))
}

fn base_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// Drop a trailing `.ext`, as `replace(/\.[^.]+$/, "")` does.
fn strip_extension(name: &str) -> String {
    match name.rfind('.') {
        Some(at) if at > 0 && !name[at + 1..].contains('.') => name[..at].to_string(),
        _ => name.to_string(),
    }
}

/// Everything outside `[a-zA-Z0-9._-]` becomes `-`, runs of `-` collapse,
/// and leading and trailing `-` go.
fn sanitize_label(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
        } else {
            out.push('-');
        }
    }
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').to_string()
}

/// `cmdRun`: take the creation lock, decide between attaching to what is
/// already there and creating a session, and carry a gone session's settings
/// into its replacement.
///
/// node: src/cli.ts:1664-1769
fn create_or_attach(
    name: &str,
    mut params: SpawnParams,
    explicit_cwd: bool,
    detach: bool,
    attach_existing: bool,
) -> CliResult {
    // A caller that already holds the creation lock for us (the bundled
    // library's CLI fallback) passes its pid. It is a one-hop control value,
    // never session environment.
    let delegated_owner: Option<i32> = std::env::var("PTY_CREATION_LOCK_OWNER_PID")
        .ok()
        .and_then(|v| v.parse().ok());
    let inherited_creation_lock =
        delegated_owner.is_some_and(|pid| registry::is_lock_owned_by_pid(name, pid));
    // SAFETY: single-threaded at this point.
    unsafe { std::env::remove_var("PTY_CREATION_LOCK_OWNER_PID") };

    let mut session = registry::get_session_by_name(name);
    let mut event_lock = None;
    let mut creation_lock = None;
    if !inherited_creation_lock {
        let guard = match registry::lock_or_refusal(&registry::event_lock_path(name)) {
            Ok(guard) => guard,
            Err(registry::LockRefusal::Busy) => {
                eprintln!("Session \"{name}\" event log is busy. Try again.");
                return Ok(1);
            }
            // Nothing here is worth a "try again": the lock file could not
            // be created and the next attempt meets the same wall.
            Err(registry::LockRefusal::Unavailable(cause)) => {
                eprintln!("{cause}");
                return Ok(1);
            }
        };
        event_lock = Some(guard);
        let guard = match registry::lock_or_refusal(&registry::lock_path(name)) {
            Ok(guard) => guard,
            Err(registry::LockRefusal::Busy) => {
                // Release through the guard: unlinking the path while the
                // guard is still armed would leave the return to unlink
                // whatever is there then.
                event_lock.take().map(registry::LockGuard::release);
                eprintln!("Session \"{name}\" is being created by another process. Try again.");
                return Ok(1);
            }
            Err(registry::LockRefusal::Unavailable(cause)) => {
                event_lock.take().map(registry::LockGuard::release);
                eprintln!("{cause}");
                return Ok(1);
            }
        };
        creation_lock = Some(guard);
        session = registry::get_session_by_name(name);
    }

    if session
        .as_ref()
        .is_some_and(|s| s.status == registry::SessionStatus::Running)
    {
        drop(creation_lock);
        event_lock.take().map(registry::LockGuard::release);
        if attach_existing {
            println!("Session \"{name}\" already running, attaching.");
            return Ok(super::attach::do_attach(name, None));
        }
        eprintln!("Session \"{name}\" is already running. Use \"pty attach {name}\" to connect.");
        return Ok(1);
    }

    // Recreating a session that has stopped keeps what the last incarnation
    // was given, so `run -a` brings it back feeling the same. Anything the
    // command line supplied wins.
    let gone = session
        .as_ref()
        .filter(|s| s.is_gone())
        .and_then(|s| s.metadata.clone());
    if let Some(previous) = &gone {
        if !explicit_cwd && !previous.cwd.is_empty() {
            params.cwd = previous.cwd.clone();
        }
        if params.tags.is_empty()
            && let Some(previous_tags) = &previous.tags
        {
            params.tags = previous_tags.clone();
        }
        if params.display_name.is_none() {
            params.display_name = previous.display_name.clone();
        }
        if params.extra_env.is_empty()
            && let Some(previous_env) = &previous.extra_env
        {
            params.extra_env = previous_env.clone();
        }
        if params.unset_env.is_empty()
            && let Some(previous_unset) = &previous.unset_env
        {
            params.unset_env = previous_unset.clone();
        }
    }
    if gone.is_some() && !inherited_creation_lock {
        registry::cleanup_all_while_locked(name);
    }
    event_lock.take().map(registry::LockGuard::release);

    params.creation_lock_owner_pid = Some(match delegated_owner {
        Some(pid) if inherited_creation_lock => pid,
        _ => std::process::id() as i32,
    });
    let spawned = super::spawn_daemon(&params);
    drop(creation_lock);
    if let Err(msg) = spawned {
        eprintln!("pty run: {msg}");
        return Ok(1);
    }

    println!("Session \"{name}\" created.");
    if detach {
        return Ok(0);
    }
    Ok(super::attach::do_attach(name, None))
}

/// Fold one `--env` token into `extra_env`. `KEY=VALUE` assigns the value
/// verbatim; `KEY` alone resolves the value through `inherited`, so the value
/// travels in the process environment instead of on the command line. A key
/// that names no inherited value is a loud refusal — never a silently dropped
/// variable.
fn insert_env_token(
    extra_env: &mut EnvMap,
    token: &str,
    inherited: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<(), String> {
    if let Some(eq) = token.find('=') {
        if eq == 0 {
            return Err(format!(
                "Invalid env format: \"{token}\". Use --env KEY=VALUE or --env KEY"
            ));
        }
        extra_env.insert(token[..eq].to_string(), token[eq + 1..].to_string());
        return Ok(());
    }
    if token.is_empty() {
        return Err(format!(
            "Invalid env format: \"{token}\". Use --env KEY=VALUE or --env KEY"
        ));
    }
    match inherited(token) {
        Some(value) => {
            extra_env.insert(token.to_string(), value.to_string_lossy().into_owned());
            Ok(())
        }
        None => Err(format!(
            "pty run: --env {token} names no value in the inherited environment; \
             pass --env {token}=VALUE"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve<'a>(
        pairs: &'a [(&'a str, &'a str)],
    ) -> impl Fn(&str) -> Option<std::ffi::OsString> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| std::ffi::OsString::from(value))
        }
    }

    #[test]
    fn an_assignment_token_sets_the_value_verbatim() {
        let mut env = EnvMap::new();
        insert_env_token(&mut env, "PORT=3000", &resolve(&[])).unwrap();
        insert_env_token(&mut env, "EMPTY=", &resolve(&[])).unwrap();
        insert_env_token(&mut env, "PORT=4000", &resolve(&[])).unwrap();
        assert_eq!(env.get("PORT").map(String::as_str), Some("4000"));
        assert_eq!(env.get("EMPTY").map(String::as_str), Some(""));
    }

    #[test]
    fn a_name_only_token_reads_the_inherited_value() {
        let mut env = EnvMap::new();
        insert_env_token(
            &mut env,
            "SEAT_TOKEN",
            &resolve(&[("SEAT_TOKEN", "kept-off-argv")]),
        )
        .unwrap();
        assert_eq!(
            env.get("SEAT_TOKEN").map(String::as_str),
            Some("kept-off-argv")
        );
    }

    #[test]
    fn a_name_only_token_without_an_inherited_value_refuses_loudly() {
        let mut env = EnvMap::new();
        let error = insert_env_token(&mut env, "SEAT_ABSENT", &resolve(&[])).unwrap_err();
        assert!(
            error.contains("--env SEAT_ABSENT names no value"),
            "{error}"
        );
        assert!(env.is_empty());
    }

    #[test]
    fn malformed_tokens_are_rejected() {
        let mut env = EnvMap::new();
        for token in ["", "=VALUE"] {
            assert!(insert_env_token(&mut env, token, &resolve(&[])).is_err());
        }
        assert!(env.is_empty());
    }
}
