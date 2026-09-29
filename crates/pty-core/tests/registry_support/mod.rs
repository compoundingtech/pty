//! Shared scaffolding for the registry and events tests: one isolated
//! `PTY_ROOT` per test binary (set once, before any registry read, so the
//! process-wide environment is never mutated while another thread reads
//! it), unique session names, and the Node `pty` oracle when it is on PATH.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Remove test directories under the temporary directory whose creating
/// process is gone.
///
/// **Five rigs create these and they use three different name shapes** —
/// `pty-<exe>-<pid>`, `pty-e2e-<pid>-<n>`, `pty-ptyfile-<tag>-<pid>-<n>` — so
/// this matches on the pid wherever it appears rather than on any one shape.
/// Whichever rig runs first cleans up after all of them.
///
/// **Only `ESRCH` counts as gone.** `EPERM` means the pid exists and is
/// somebody else's, and a still-running test binary needs its directory.
/// Leaving one behind is the safe error; deleting a live process's registry
/// out from under it is not.
///
/// Measured on 2026-09-05: one full workspace run left 22 directories behind,
/// and nothing ever removed them. Statics do not drop, and a test binary that
/// is killed would not run a destructor anyway, so this cleans up on the way
/// IN rather than on the way out.
fn definitely_gone(pid: i32) -> bool {
    // SAFETY: signal 0 only checks for existence and permission.
    let rc = unsafe { libc::kill(pid, 0) };
    rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

fn reap_dead_roots(tmp: &Path) {
    let Ok(entries) = std::fs::read_dir(tmp) else {
        return;
    };
    let me = std::process::id() as i32;
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name.strip_prefix("pty-") else {
            continue;
        };
        // **The FIRST all-digit component, not any of them.** Every shape puts
        // the pid there and some put a counter after it, and a counter that
        // happened to equal a dead pid would otherwise delete a live test's
        // directory.
        let dead = rest
            .split('-')
            .find_map(|c| c.parse::<i32>().ok())
            .is_some_and(|pid| pid > 0 && pid != me && definitely_gone(pid));
        if dead {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// The registry root for this test process. The first call sets `PTY_ROOT`
/// (and silences the legacy notices); every registry call must go through
/// this first so the environment is settled before any thread reads it.
pub fn root() -> PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "t".to_string());
        let short: String = exe.chars().take(12).collect();
        let tmp = std::env::temp_dir();
        // **Reap the roots of test processes that are gone.** A static never
        // drops, and a test binary that is killed would not get to run a
        // destructor anyway, so this cleans up on the way IN rather than on
        // the way out. Without it every run of every test binary left a
        // directory behind: measured on 2026-09-05, one full workspace run
        // leaked 22.
        reap_dead_roots(&tmp);
        // **The pid comes first.** It used to trail the executable name, and
        // `take(12)` can cut a cargo hash suffix mid-way and leave a digit
        // standing alone: `pty-events_log-8-3021034` made `8` the first
        // numeric component. Pid 8 is a kernel thread, `kill(8, 0)` returns
        // EPERM rather than ESRCH, and the reaper below correctly refused to
        // delete on a pid it could not prove dead -- so those directories
        // accumulated forever. Putting the pid first removes the ambiguity
        // rather than teaching the reaper to guess.
        let dir = tmp.join(format!("pty-{}-{short}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test root");
        // Private like Node's `ensureSessionDir`, so a Node daemon under it
        // advertises its recovery capability.
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        // SAFETY: the OnceLock guarantees this runs exactly once and every
        // test enters through `root()` before touching the registry, so no
        // other thread is reading the environment concurrently.
        unsafe {
            std::env::set_var("PTY_ROOT", &dir);
            std::env::set_var("PTY_ROOT_LEGACY_SILENT", "1");
            std::env::remove_var("PTY_SESSION_DIR");
            std::env::remove_var("PTY_SESSION");
        }
        // Say this once, here, rather than letting the tests that bind a
        // socket fail with "path must be shorter than SUN_LEN" and look like
        // a defect in the port.
        //
        // The budget is the rig's, not the product's. `pty` generates short
        // ids and its own check allows for those; a test picks its own name,
        // and the longest in the suite is 16 characters plus a counter. So a
        // root that satisfies the product can still be too long here, which
        // is exactly what made this confusing the first time.
        //
        // macOS caps the socket path at 104 bytes where Linux allows 108,
        // and macOS's own temp directory already spends about 49 of them.
        // Anything that nests inside it eats the rest: a `nix develop` shell
        // took 16 more and turned twenty tests red on 2026-09-02.
        let longest = dir.join("a-session-name-of-the-length-tests-use.sock");
        assert!(
            longest.as_os_str().len() <= pty_core::registry::SUN_PATH_MAX,
            "the test root is too long for a unix socket path: {} bytes of {} at
  {}
  \
             This is the test harness, not the software. Point TMPDIR at a SHORTER path.\n  \
             Shorter is the whole requirement, and a short path on a disk-backed \
             filesystem is the safe answer: on some machines /tmp is a RAM disk \
             shared with everything else on the box.",
            longest.as_os_str().len(),
            pty_core::registry::SUN_PATH_MAX,
            dir.display()
        );
        dir
    })
    .clone()
}

/// A session name unique within this process.
pub fn unique_name(prefix: &str) -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}{n}-{:x}", std::process::id() % 0xffff)
}

/// Every registry file of `name` (ignores missing).
pub fn remove_session_files(name: &str) {
    let root = root();
    if let Ok(entries) = std::fs::read_dir(&root) {
        for e in entries.flatten() {
            let fname = e.file_name().to_string_lossy().into_owned();
            if fname.starts_with(&format!("{name}.")) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// A pid no live process can own (above Linux's `pid_max`).
pub const DEAD_PID: i32 = 2_147_483_646;

/// The socket a dead daemon leaves behind: a real socket file that refuses
/// every connect.
///
/// **It is bound and never listened on.** Binding a `UnixListener` and
/// dropping it looks the same, but for a moment it is not: the tests run on
/// parallel threads, and some of them spawn processes (`ps` for a start
/// token on macOS, `true` in `process_helpers`). A child forked while the
/// listener is open holds a copy of it until its exec finishes, and on macOS
/// exec includes checking the new binary's code signature, which is slow
/// under load. A probe that connects in that window is accepted, and the
/// session reads as `Running`. On a Mac on 2026-09-28 that failed 44 runs
/// of the test binary in 1000.
/// A socket that was never put into the listening state refuses connections
/// whichever process holds it, so the stale socket is stale from the moment
/// it exists.
pub fn stale_socket(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::FileTypeExt;

    let bytes = path.as_os_str().as_bytes();
    // SAFETY: plain socket calls on a descriptor this function owns and
    // closes; `addr` is zeroed and `bytes` fits with its terminating NUL.
    let (rc, err) = unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        assert!(bytes.len() < addr.sun_path.len(), "{} is too long", path.display());
        for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
            *dst = *src as libc::c_char;
        }
        let rc = libc::bind(
            fd,
            (&raw const addr).cast::<libc::sockaddr>(),
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        );
        let err = std::io::Error::last_os_error();
        libc::close(fd);
        (rc, err)
    };
    assert_eq!(rc, 0, "bind {}: {err}", path.display());

    // The two properties the tests rely on, checked here rather than assumed.
    let file_type = std::fs::symlink_metadata(path).unwrap().file_type();
    assert!(file_type.is_socket(), "{} is not a socket", path.display());
    let refused = std::os::unix::net::UnixStream::connect(path).unwrap_err();
    assert_eq!(
        refused.kind(),
        std::io::ErrorKind::ConnectionRefused,
        "a stale socket must refuse a connect"
    );
}

/// Parsed lines of `<name>.events.jsonl` (empty when missing).
pub fn read_events(name: &str) -> Vec<serde_json::Value> {
    let path = root().join(format!("{name}.events.jsonl"));
    match std::fs::read_to_string(path) {
        Ok(s) => s
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).expect("valid JSONL line"))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// The Node `pty` binary (the parity oracle) when it is on PATH and is the
/// pinned version; `None` skips oracle tests.
pub fn node_pty() -> Option<PathBuf> {
    static BIN: OnceLock<Option<PathBuf>> = OnceLock::new();
    BIN.get_or_init(|| {
        let out = Command::new("pty").arg("--version").output().ok()?;
        let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !version.starts_with("0.12.") {
            return None;
        }
        let which = Command::new("sh")
            .args(["-c", "command -v pty"])
            .output()
            .ok()?;
        let path = String::from_utf8_lossy(&which.stdout).trim().to_string();
        (!path.is_empty()).then(|| PathBuf::from(path))
    })
    .clone()
}

/// Run the Node `pty` against a root.
pub fn run_node_pty(bin: &Path, root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(bin)
        .args(args)
        .env("PTY_ROOT", root)
        .env("PTY_ROOT_LEGACY_SILENT", "1")
        .env_remove("PTY_SESSION")
        .env_remove("PTY_SESSION_DIR")
        .output()
        .expect("spawn node pty")
}

/// Wait until `pred` holds, polling every 20 ms for up to `ms`.
pub fn wait_for(ms: u64, mut pred: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    while std::time::Instant::now() < deadline {
        if pred() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    pred()
}

/// Can `ps` here answer the fields this port asks it for?
///
/// Off Linux the port reads a process start token, a process state and a
/// memory/CPU pair out of `ps`. A machine without one, or with a build that
/// refuses a field, cannot exercise those paths — and a test that fails for
/// that reason blames the code for its environment.
///
/// **So say which it is.** A build sandbox has no `ps` at all, and
/// nixpkgs' darwin `ps` is entitlement-limited: it refuses `rss` and returns
/// a blank state for a live process, while answering `lstart` correctly. A
/// test that passed under it would have measured nothing. Measured
/// 2026-09-02.
///
/// Returns `None` when `ps` can answer, and the reason to print when it
/// cannot.
pub fn ps_cannot_answer(fields: &str) -> Option<String> {
    let me = std::process::id().to_string();
    match std::process::Command::new("ps")
        .args(["-o", fields, "-p", &me])
        .output()
    {
        Err(e) => Some(format!("no `ps` on this machine ({e})")),
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            // One field per requested key, for this very process, which is
            // alive by construction.
            let wanted = fields.split(',').filter(|f| !f.is_empty()).count();
            let got = stdout.split_whitespace().count();
            if !out.status.success() || got < wanted {
                return Some(format!(
                    "`ps -o {fields}` gave {got} of {wanted} fields for a live process\n                         stdout: {stdout:?}\n    stderr: {stderr:?}"
                ));
            }
            None
        }
    }
}

/// `ps_cannot_answer`, as a test guard: prints the reason and says whether to
/// stop. Only meaningful off Linux, which reads `/proc` instead.
pub fn skip_without_ps(fields: &str) -> bool {
    if cfg!(target_os = "linux") {
        return false;
    }
    match ps_cannot_answer(fields) {
        Some(why) => {
            eprintln!("skipped: {why}");
            true
        }
        None => false,
    }
}
