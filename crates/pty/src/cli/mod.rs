//! The `pty` CLI: argv dispatch and one module per verb.
//!
//! [`dispatch`] is Node's `main()` (cli.ts:670-760): the global `--root`
//! scan, the root-length backstop, subcommand detection, the interactive
//! flags, the per-command `--help` interceptor, the command switch, and
//! git-style forwarding to `pty-<cmd>` executables. Every verb returns a
//! [`CliResult`]; an `Err` is printed to stderr and exits 1, the way Node's
//! `main().catch` prints `err.message` (cli.ts:4132-4135).
//!
//! `peek` is still an interim port kept from the v0 binary; every other verb
//! speaks Node's texts. The interactive picker is the one thing left that
//! reports itself unimplemented.

pub mod argv;
pub mod ask;
pub mod completions;
pub mod deferred;
pub mod down;
pub mod emit;
pub mod events;
pub mod evidence;
pub mod gc;
pub mod help;
pub mod list;
pub mod metadata;
pub mod readiness;
pub mod remote_serve;
pub mod rename;
pub mod rm;
pub mod tag;
pub mod tag_multi;
pub mod up;
pub mod version;

// Verbs that talk to a session socket.
pub mod attach;
pub mod exec;
pub mod kill;
pub mod peek;
pub mod restart;
pub mod run;
pub mod send;
pub mod stats;

use std::fmt;
use std::io::Write;

use pty_core::registry::{self, TagMap};

/// A failed command: `Display` is the exact text Node prints to stderr
/// before exiting 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError(pub String);

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CliError {}

impl From<String> for CliError {
    fn from(s: String) -> Self {
        CliError(s)
    }
}

impl From<&str> for CliError {
    fn from(s: &str) -> Self {
        CliError(s.to_string())
    }
}

/// What a verb returns: the exit code, or a message that exits 1.
pub type CliResult = Result<i32, CliError>;

/// Resolve a user-supplied reference (stable id or unique display name) to
/// the stable id. Not found → `Session "<ref>" not found.`; several display
/// name matches → the registry's ambiguity text. Both exit 1.
///
/// node: src/cli.ts:610-617 (`resolveRef`), src/sessions.ts:1351-1363
pub fn resolve_ref(reference: &str) -> Result<String, CliError> {
    match registry::get_session(reference) {
        Ok(Some(s)) => Ok(s.name),
        Ok(None) => Err(CliError(format!("Session \"{reference}\" not found."))),
        Err(ambiguous) => Err(CliError(ambiguous)),
    }
}

/// The first argument as a reference, resolved; `usage` is the error when
/// there is none (`Usage: pty kill <name>` and friends).
pub fn require_ref(args: &[String], usage: &str) -> Result<String, CliError> {
    match args.first() {
        Some(r) => resolve_ref(r),
        None => Err(CliError(usage.to_string())),
    }
}

/// The variables a restarted session must not inherit from whoever asked
/// for the restart.
///
/// node: src/cli.ts:3869
pub const RESTART_SCRUBBED_ENV: [&str; 2] = ["ST_AGENT", "ST_ROOT"];

/// Refuse a command that would nest a client inside the current session.
/// Returns `Some(1)` when it refused, having printed the reason and the
/// hint; `None` means carry on.
///
/// node: src/cli.ts:626-637 (`ensureNotNested`)
pub fn ensure_not_nested(cmd: &str, force: bool, hint: Option<&str>) -> Option<i32> {
    if force {
        return None;
    }
    let nested = std::env::var("PTY_SESSION").unwrap_or_default();
    if nested.is_empty() {
        return None;
    }
    eprintln!("pty {cmd}: already inside pty session \"{nested}\".");
    match hint {
        Some(h) => eprintln!("{h}"),
        None => eprintln!("  Pass --force to override."),
    }
    Some(1)
}

/// Options the interactive picker is opened with.
#[derive(Debug, Clone, Default)]
pub struct InteractiveOptions {
    pub preselect_new: bool,
    pub filter_tags: TagMap,
    pub force: bool,
}

/// `pty` / `pty i` / `pty interactive`: the nesting guard, then the TUI.
/// The picker itself is supplied by the TUI crate; until it lands this
/// reports the absence and exits 1.
///
/// node: src/cli.ts:84-93 (`runInteractive` wrapper)
pub fn interactive(opts: InteractiveOptions) -> CliResult {
    if !opts.force
        && let Ok(nested) = std::env::var("PTY_SESSION")
        && !nested.is_empty()
    {
        eprintln!("pty interactive: already inside pty session \"{nested}\".");
        eprintln!(
            "  The interactive picker would render inside your current session and detach would route to the outer client."
        );
        eprintln!(
            "  Detach first (Ctrl+\\) and run `pty` from outside, or pass --force to open the picker anyway."
        );
        return Ok(1);
    }
    run_interactive(opts)
}

/// The picker itself.
fn run_interactive(opts: InteractiveOptions) -> CliResult {
    Ok(crate::interactive::run(opts))
}

/// Node's `main()`: returns the process exit code.
///
/// node: src/cli.ts:670-760, 1626-1660
pub fn dispatch(mut args: Vec<String>) -> i32 {
    // Global `--root <path>` can follow the subcommand, but `--` hands the
    // remaining arguments to the child of `run` unchanged.
    let command_end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    if let Some(idx) = args[..command_end].iter().position(|a| a == "--root") {
        match args.get(idx + 1) {
            Some(val) if !val.starts_with('-') => {
                // SAFETY: single-threaded at this point; nothing else reads
                // the environment concurrently.
                unsafe { std::env::set_var("PTY_ROOT", val) };
                args.drain(idx..idx + 2);
            }
            _ => {
                eprintln!("pty: --root requires a path (e.g. pty --root /var/lib/pty-eval list)");
                return 1;
            }
        }
    }

    // Root-length backstop before any subcommand runs (cli.ts:703-717).
    if let Some(msg) = registry::root_length_check() {
        eprintln!("{msg}");
        return 1;
    }

    // Subcommand detection: the first token that is not a flag, skipping the
    // value after `--filter-tag` (cli.ts:726-733).
    let mut subcommand = "";
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--filter-tag" {
            i += 2;
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        subcommand = a;
        break;
    }
    let subcommand = subcommand.to_string();

    let mut interactive_opts = InteractiveOptions::default();
    if subcommand.is_empty() || subcommand == "i" || subcommand == "interactive" {
        interactive_opts.preselect_new = args.iter().any(|a| a == "--preselect-new");
        interactive_opts.force = args.iter().any(|a| a == "--force");
        match registry::extract_filter_tags(&mut args) {
            Ok(tags) => interactive_opts.filter_tags = tags,
            Err(msg) => {
                eprintln!("{msg}");
                return 1;
            }
        }
    }
    let dispatch_args: Vec<&String> = args
        .iter()
        .filter(|a| *a != "--preselect-new" && *a != "--force")
        .collect();

    if dispatch_args.is_empty() {
        return finish(interactive(interactive_opts));
    }
    let command = dispatch_args[0].clone();

    // A subcommand's own `--help` / `-h` in the first position after the
    // command prints the focused help and exits 0 (cli.ts:756-758).
    if matches!(args.get(1).map(String::as_str), Some("-h") | Some("--help"))
        && let Some(text) = help::command_help(&command)
    {
        print_stdout(text);
        return 0;
    }

    // Subcommands parse `args` (which still holds `--force`), not
    // `dispatch_args`, as Node does.
    let rest = if args.len() > 1 { &args[1..] } else { &[][..] };
    let result = match command.as_str() {
        "__daemon" => Ok(crate::daemon::daemon_main()),
        "interactive" | "i" => interactive(interactive_opts),
        "run" => run::run(rest),
        "attach" | "a" => attach::run(rest),
        "exec" => exec::run(rest),
        "peek" => peek::run(rest),
        "send" => send::run(rest),
        "events" => events::run(rest),
        "list" | "ls" => list::run(&args),
        "remote-serve" => remote_serve::run(rest),
        "stats" => stats::run(rest),
        "restart" => restart::run(rest),
        "kill" => kill::run(rest),
        "recover" => deferred::run("recover"),
        "gc" => gc::run(rest),
        "tag" => tag::run(rest),
        "tag-multi" => tag_multi::run(rest),
        "emit" => emit::run(rest),
        "up" => up::run(rest),
        "down" => down::run(rest),
        "rename" => rename::run(rest),
        "metadata" => metadata::run(rest),
        "readiness" => readiness::run(rest),
        "evidence" => evidence::run(rest),
        "rm" | "remove" => rm::run(rest),
        "test" => deferred::run("test"),
        "completions" => Ok(completions::run(rest)),
        "version" | "--version" | "-v" | "-V" => version::run(),
        "help" | "--help" | "-h" => {
            help::print_usage();
            Ok(0)
        }
        other => forward_or_unknown(other, rest),
    };
    finish(result)
}

/// Print an `Err` the way `main().catch` does and turn it into exit 1.
fn finish(result: CliResult) -> i32 {
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

fn print_stdout(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

/// Git-style forwarding: `which pty-<cmd>`; found → run it with inherited
/// stdio and the unfiltered args, exit with its status (`?? 1`); else
/// `Unknown command: <cmd>` on stderr and the full usage on stdout, exit 1.
///
/// node: src/cli.ts:1641-1660
fn forward_or_unknown(command: &str, rest: &[String]) -> CliResult {
    if let Some(path) = find_on_path(&format!("pty-{command}")) {
        let status = std::process::Command::new(path).args(rest).status();
        return Ok(status.ok().and_then(|s| s.code()).unwrap_or(1));
    }
    eprintln!("Unknown command: {command}");
    help::print_usage();
    Ok(1)
}

/// The first executable named `name` on `$PATH`.
///
/// Node shells out to `which` here. This walks `$PATH` instead, because
/// `which` is not everywhere: a Nix build sandbox has no `which` at all, and
/// there the shell-out failed quietly and every `pty-<cmd>` extension read as
/// an unknown command. An empty `$PATH` entry means the working directory,
/// the way execvp treats it.
fn find_on_path(name: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = if dir.as_os_str().is_empty() {
            std::path::PathBuf::from(name)
        } else {
            dir.join(name)
        };
        let Ok(meta) = std::fs::metadata(&candidate) else {
            continue;
        };
        if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
            return Some(candidate);
        }
    }
    None
}

// ── Daemon launch ───────────────────────────────────────────────────────

pub use pty_lifecycle::{SpawnParams, apply_persisted_launch_options};

/// Resolve the command, spawn the detached session daemon and wait for it
/// to publish through `pty-lifecycle`.
///
/// node: src/spawn.ts:372-393, 164-243
pub fn spawn_daemon(p: &SpawnParams) -> Result<(), String> {
    let mut params = p.clone();
    params.command = pty_core::spawn::resolve_command(&p.command)?;
    let executable =
        std::env::current_exe().map_err(|e| format!("cannot find own executable: {e}"))?;
    // A long-lived picker can outlive an atomic package upgrade. On Linux
    // `/proc/self/exe` then names the old, unlinked inode with ` (deleted)`
    // appended. Start new daemons from the replacement at its install path.
    let executable = if executable.exists() {
        executable
    } else {
        let path = executable.to_string_lossy();
        let replacement = path
            .strip_suffix(" (deleted)")
            .map(std::path::PathBuf::from)
            .filter(|candidate| candidate.is_file())
            .ok_or_else(|| format!("cannot find a replacement for own executable: {path}"))?;
        replacement
    };
    pty_lifecycle::spawn_daemon(&executable, params)
        .map(|_| ())
        .map_err(|e| e.to_string())
}
