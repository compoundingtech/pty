//! The registry root (`$PTY_ROOT`) and the per-session file paths under it.
//!
//! Mirrors `src/sessions.ts:24-131` of the Node project: `PTY_ROOT` wins,
//! the deprecated `PTY_SESSION_DIR` is honoured with a one-time notice, and
//! otherwise sessions use a host-specific directory under
//! `~/.local/state/pty`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Largest pathname, excluding its trailing NUL, that the supported platform
/// can represent in `sockaddr_un.sun_path`.
pub const SUN_PATH_MAX: usize = if cfg!(target_os = "macos") { 103 } else { 104 };

static WARNED_LEGACY_ROOT_ENV: AtomicBool = AtomicBool::new(false);
static WARNED_ROOT_MASKS_LEGACY: AtomicBool = AtomicBool::new(false);

fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// A machine-local default registry root under the home state directory.
/// The hostname keeps sockets and records apart when homes are shared.
pub fn default_session_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let mut hostname = [0u8; 256];
    let host = if unsafe { libc::gethostname(hostname.as_mut_ptr().cast(), hostname.len()) } == 0 {
        let len = hostname.iter().position(|&b| b == 0).unwrap_or(hostname.len());
        &hostname[..len]
    } else {
        b"localhost"
    };
    default_session_dir_for_host(Path::new(&home), host)
}

fn default_session_dir_for_host(home: &Path, host: &[u8]) -> PathBuf {
    // A fixed-width FNV-1a digest keeps the socket path usable when the host
    // name is long, while retaining a stable directory per host.
    let hash = host.iter().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    });
    home.join(".local/state/pty").join(format!("h-{hash:016x}"))
}

std::thread_local! {
    static SCOPED_ROOT: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Run `f` with this thread's registry rooted at `root` instead of
/// `$PTY_ROOT`: every registry path `f` resolves on this thread — sockets,
/// pid files, metadata, locks, the events log — lies under `root`.
///
/// This is for a program that keeps a registry of its own, apart from its
/// process's `$PTY_ROOT`, and cannot set that variable because it runs other
/// threads. Scopes nest, and the previous root comes back when `f` returns
/// or unwinds.
///
/// Only the calling thread is scoped. A thread that `f` starts resolves the
/// root for itself, so a long-lived
/// [`EventWriter`](crate::events::EventWriter) or
/// [`EventFollower`](crate::events::follow::EventFollower) does not inherit
/// the scope.
pub fn with_root<T>(root: &Path, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<PathBuf>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SCOPED_ROOT.with(|scoped| *scoped.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(SCOPED_ROOT.with(|scoped| scoped.replace(Some(root.to_path_buf()))));
    f()
}

/// Where the effective registry root was selected. CLI `--root` provenance
/// is tracked by the dispatcher, which exports its value as `PTY_ROOT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootSource {
    /// An embedding application's thread-local [`with_root`] override.
    Scoped,
    PtyRoot,
    PtySessionDir,
    Default,
}

impl RootSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scoped => "scoped",
            Self::PtyRoot => "PTY_ROOT",
            Self::PtySessionDir => "PTY_SESSION_DIR",
            Self::Default => "default",
        }
    }
}

/// Resolve the session registry directory: the root of an enclosing
/// [`with_root`] on this thread, else `$PTY_ROOT`, else the deprecated
/// `$PTY_SESSION_DIR` (with a one-time notice on stderr unless
/// `PTY_ROOT_LEGACY_SILENT` is set), else [`default_session_dir`].
///
/// node: src/sessions.ts:82-110
pub fn session_dir() -> PathBuf {
    resolve_session_dir().0
}

/// Resolve the effective registry path and its source without touching the
/// registry. Paths are returned as selected, never canonicalized.
pub fn resolve_session_dir() -> (PathBuf, RootSource) {
    if let Some(root) = SCOPED_ROOT.with(|scoped| scoped.borrow().clone()) {
        return (root, RootSource::Scoped);
    }
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
        return (PathBuf::from(root), RootSource::PtyRoot);
    }
    if let Some(legacy) = legacy {
        if !silent && !WARNED_LEGACY_ROOT_ENV.swap(true, Ordering::SeqCst) {
            let _ = writeln!(
                std::io::stderr(),
                "pty: PTY_SESSION_DIR is deprecated; use PTY_ROOT (same shape, canonical name)."
            );
        }
        return (PathBuf::from(legacy), RootSource::PtySessionDir);
    }
    (default_session_dir(), RootSource::Default)
}

/// Create the session dir (mode 0700) if missing.
///
/// An existing root is left exactly as its owner set it — chmod would follow
/// a symlinked root and change its target. The files under it are owner-only
/// from their creating open ([`super::atomic::atomic_write`]), so a root
/// someone deliberately shares stays shared while its records stay
/// owner-only.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_hostname_keeps_default_socket_path_within_kernel_limit() {
        let home = Path::new("/home/example/shared/users/member");
        let host = b"build-node-01234567890123456789012345678";
        let root = default_session_dir_for_host(home, host);
        let socket = root.join("abcdefgh.sock");
        assert!(socket.as_os_str().len() <= SUN_PATH_MAX);
        assert_eq!(root, default_session_dir_for_host(home, host));
        assert_ne!(root, default_session_dir_for_host(home, b"other-host"));
    }

    #[test]
    fn shared_home_default_child() {
        let Ok(home) = std::env::var("PTY_TEST_SHARED_HOME_DEFAULT") else {
            return;
        };
        assert_eq!(
            default_session_dir().parent(),
            Some(Path::new(&home).join(".local/state/pty").as_path())
        );
    }

    #[test]
    fn default_registry_names_the_host_with_a_runtime_directory_present() {
        let base = std::env::temp_dir().join(format!(
            "pty-host-root-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(base.join("run")).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "registry::root::tests::shared_home_default_child"])
            .env("PTY_TEST_SHARED_HOME_DEFAULT", &base)
            .env("XDG_RUNTIME_DIR", base.join("run"))
            .env("HOME", &base)
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(base);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
    }

    #[test]
    fn a_scoped_root_wins_nests_and_is_restored() {
        let before = session_dir();
        let outer = Path::new("/tmp/pty-scoped-root-outer");
        let inner = Path::new("/tmp/pty-scoped-root-inner");
        with_root(outer, || {
            assert_eq!(session_dir(), outer);
            assert_eq!(socket_path("a"), outer.join("a.sock"));
            with_root(inner, || assert_eq!(session_dir(), inner));
            assert_eq!(session_dir(), outer, "the inner scope gives the outer root back");
        });
        assert_eq!(session_dir(), before);
    }

    #[test]
    fn a_panic_inside_the_scope_still_restores_the_root() {
        let before = session_dir();
        let unwound = std::panic::catch_unwind(|| {
            with_root(Path::new("/tmp/pty-scoped-root-panic"), || panic!("inside the scope"))
        });
        assert!(unwound.is_err());
        assert_eq!(session_dir(), before);
    }

    #[test]
    fn another_thread_is_not_scoped() {
        let scoped = Path::new("/tmp/pty-scoped-root-thread");
        let elsewhere = with_root(scoped, || std::thread::spawn(session_dir).join().unwrap());
        assert_ne!(elsewhere, scoped);
    }

    /// An existing root is never chmod'd — tightening would follow a symlinked
    /// root and change its target. Owner-only records come from the creating
    /// open, not from the directory.
    #[test]
    fn an_existing_root_keeps_its_mode_even_through_a_symlink() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let base = std::env::temp_dir().join(format!(
                "pty-existing-root-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let target = base.join("target");
            std::fs::create_dir_all(&target).unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
            let link = base.join("link");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            with_root(&link, || {
                ensure_session_dir().unwrap();
            });
            let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o755, "the symlinked root's target must keep its mode");
            let _ = std::fs::remove_dir_all(&base);
        }
    }
}
