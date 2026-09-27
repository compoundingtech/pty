//! The registry root (`$PTY_ROOT`) and the per-session file paths under it.
//!
//! Mirrors `src/sessions.ts:24-131` of the Node project: `PTY_ROOT` wins,
//! the deprecated `PTY_SESSION_DIR` is honoured with a one-time notice, and
//! everything else lands in `~/.local/state/pty`.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// Largest pathname, excluding its trailing NUL, that the supported platform
/// can represent in `sockaddr_un.sun_path`.
pub const SUN_PATH_MAX: usize = if cfg!(target_os = "macos") { 103 } else { 104 };

static WARNED_LEGACY_ROOT_ENV: AtomicBool = AtomicBool::new(false);
static WARNED_ROOT_MASKS_LEGACY: AtomicBool = AtomicBool::new(false);

fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// The default registry root, `~/.local/state/pty`.
pub fn default_session_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".local").join("state").join("pty")
}

/// Resolve the session registry directory: `$PTY_ROOT`, else the deprecated
/// `$PTY_SESSION_DIR` (with a one-time notice on stderr unless
/// `PTY_ROOT_LEGACY_SILENT` is set), else `~/.local/state/pty`.
///
/// node: src/sessions.ts:82-110
pub fn session_dir() -> PathBuf {
    let root = env_non_empty("PTY_ROOT");
    let legacy = env_non_empty("PTY_SESSION_DIR");
    let silent = std::env::var_os("PTY_ROOT_LEGACY_SILENT").is_some();
    if let Some(root) = root {
        if let Some(legacy) = legacy
            && !silent
            && !WARNED_ROOT_MASKS_LEGACY.swap(true, Ordering::SeqCst)
        {
            let _ = writeln!(
                std::io::stderr(),
                "pty: both PTY_ROOT and PTY_SESSION_DIR are set — using PTY_ROOT ({root}); PTY_SESSION_DIR ({legacy}) is ignored (deprecated). For isolation, set PTY_ROOT."
            );
        }
        return PathBuf::from(root);
    }
    if let Some(legacy) = legacy {
        if !silent && !WARNED_LEGACY_ROOT_ENV.swap(true, Ordering::SeqCst) {
            let _ = writeln!(
                std::io::stderr(),
                "pty: PTY_SESSION_DIR is deprecated; use PTY_ROOT (same shape, canonical name)."
            );
        }
        return PathBuf::from(legacy);
    }
    default_session_dir()
}

/// Create the session dir (mode 0700) if missing.
///
/// node: src/sessions.ts:112-114
pub fn ensure_session_dir() -> std::io::Result<PathBuf> {
    let dir = session_dir();
    if !dir.is_dir() {
        std::fs::create_dir_all(&dir)?;
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(dir)
}

/// Path to a session's unix socket, `<root>/<name>.sock`.
pub fn socket_path(name: &str) -> PathBuf {
    session_dir().join(format!("{name}.sock"))
}

/// Path to a session's pid file, `<root>/<name>.pid`.
pub fn pid_path(name: &str) -> PathBuf {
    session_dir().join(format!("{name}.pid"))
}

/// Path to a session's metadata JSON, `<root>/<name>.json`.
pub fn metadata_path(name: &str) -> PathBuf {
    session_dir().join(format!("{name}.json"))
}

/// Path to a session's events log, `<root>/<name>.events.jsonl`.
pub fn events_path(name: &str) -> PathBuf {
    session_dir().join(format!("{name}.events.jsonl"))
}

/// Path to a session's creation/metadata lock, `<root>/<name>.lock`.
pub fn lock_path(name: &str) -> PathBuf {
    session_dir().join(format!("{name}.lock"))
}

/// Path to a session's event lock, `<root>/<name>.events.lock`.
pub fn event_lock_path(name: &str) -> PathBuf {
    session_dir().join(format!("{name}.events.lock"))
}

/// Path to the signed recovery revision, `<root>/.recovery/<name>.revision.json`.
///
/// node: src/recovery.ts:80-98
pub fn recovery_revision_path(name: &str) -> PathBuf {
    session_dir()
        .join(".recovery")
        .join(format!("{name}.revision.json"))
}

/// Path to a session's output-activity sidecar,
/// `<root>/.activity/<name>.json`. It lives in a subdirectory, as the
/// recovery revisions do, so that a watcher of the registry root does not see
/// the daemon's once-a-second activity writes, and so that neither the Node
/// nor the Rust listing mistakes it for a session (both take `<name>.json`
/// from the root's own entries only). See docs/decisions/0015.
pub fn output_activity_path(name: &str) -> PathBuf {
    session_dir().join(".activity").join(format!("{name}.json"))
}

/// The CLI's startup backstop for an over-long root: when the raw
/// `PTY_ROOT` (or `PTY_SESSION_DIR`) plus the 14 bytes of `/xxxxxxxx.sock`
/// cannot fit `sun_path`, return Node's three-line message (no trailing
/// newline) so the caller can print it and exit 1.
///
/// node: src/cli.ts:688-717
pub fn root_length_check() -> Option<String> {
    let resolved = env_non_empty("PTY_ROOT").or_else(|| env_non_empty("PTY_SESSION_DIR"))?;
    const SOCK_SUFFIX_BYTES: usize = 1 + 8 + 5;
    let root_bytes = resolved.len();
    if root_bytes + SOCK_SUFFIX_BYTES > SUN_PATH_MAX {
        let usable = SUN_PATH_MAX - SOCK_SUFFIX_BYTES;
        return Some(format!(
            "pty: PTY_ROOT is too long — {root_bytes} bytes; must be ≤ {usable} bytes for the socket path to fit the {SUN_PATH_MAX}-byte kernel limit.\n  root: {resolved}\n  Shorten the root (or use `pty --root <shorter-path>` for a one-off)."
        ));
    }
    None
}
