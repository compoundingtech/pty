//! The conformance rig: an isolated `PTY_ROOT`, a scrubbed environment, and
//! helpers to run the binary under test, host daemons, and speak the wire
//! protocol to them directly.
//!
//! Everything here is black-box: the rig only knows the path of a `pty`
//! binary and the on-disk registry layout (`<root>/<id>.{sock,pid,json}`).

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use pty_core::protocol::{self, MessageType, Packet, PacketReader};
use pty_testkit::{Session, SpawnOptions};
use serde_json::Value;

/// Environment variables scrubbed from every process the rig starts, so the
/// suite behaves the same whether or not it runs inside a pty session.
pub const SCRUBBED_ENV: &[&str] = &[
    "PTY_SESSION",
    "PTY_SESSION_GENERATION",
    "PTY_SESSION_DIR",
    "PTY_REAP_ON_EXIT",
    "PTY_SERVER_CONFIG",
    "ST_AGENT",
    "ST_ROOT",
    "NO_COLOR",
    "PTY_ROOT",
];

/// Per-invocation timeout for a CLI call.
pub const CLI_TIMEOUT: Duration = Duration::from_secs(30);

/// Result of one CLI invocation.
#[derive(Debug, Clone)]
pub struct Out {
    /// Exit status; `-1` when the process was killed by the timeout or died
    /// from a signal.
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Set when the 30 s timeout fired.
    pub timed_out: bool,
}

impl Out {
    /// stdout as (lossy) UTF-8.
    pub fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// stderr as (lossy) UTF-8.
    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    /// A one-line summary for assertion messages.
    pub fn summary(&self) -> String {
        format!(
            "status={} timed_out={}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.status,
            self.timed_out,
            self.stdout(),
            self.stderr()
        )
    }
}

/// Where the binary under test comes from.
pub fn pty_bin() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        if let Ok(p) = std::env::var("PTY_TEST_BIN")
            && !p.is_empty()
        {
            let p = PathBuf::from(p);
            assert!(p.is_absolute(), "PTY_TEST_BIN must be an absolute path: {}", p.display());
            assert!(p.exists(), "PTY_TEST_BIN does not exist: {}", p.display());
            return p;
        }
        let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(profile)
            .join("pty");
        let p = p.canonicalize().unwrap_or(p);
        assert!(
            p.exists(),
            "no pty binary at {} — run `cargo build -p pty` or set PTY_TEST_BIN",
            p.display()
        );
        p
    })
}

/// The version string printed by the binary under test.
pub fn pty_version() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        let out = Command::new(pty_bin())
            .arg("--version")
            .env("PTY_ROOT_LEGACY_SILENT", "1")
            .output()
            .expect("run pty --version");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    })
}

/// True when the binary under test is the Rust port (`-rust` version marker).
pub fn is_rust() -> bool {
    pty_version().contains("-rust")
}

/// True when the binary under test is the Node reference implementation.
pub fn is_node() -> bool {
    !is_rust()
}

/// Default deadline for polls: 10 s, doubled by `PC_SLOW=1`.
pub fn deadline() -> Duration {
    let base = Duration::from_secs(10);
    if std::env::var("PC_SLOW").map(|v| v == "1").unwrap_or(false) {
        base * 2
    } else {
        base
    }
}

/// Is the binary under test the workspace's own unoptimized build?
///
/// `cargo test` defaults to the debug profile, and the debug binary is what
/// [`pty_bin`] then picks. That matters only for the few fixtures that state
/// a THROUGHPUT bound: the VT parser is the hot path and it is not optimized
/// in a debug build.
///
/// Measured 2026-09-02, one megabyte of child output through a session with
/// no clients attached: debug Rust 9.4 s, release Rust 0.22 s, Node 0.25 s.
/// So a debug build misses a bound the shipped binary beats, and a fixture
/// budget written for the shipped binary reads as a product defect.
///
/// False when `PTY_TEST_BIN` names a binary, because then the profile is the
/// caller's business.
pub fn unoptimized_binary_under_test() -> bool {
    cfg!(debug_assertions) && std::env::var("PTY_TEST_BIN").map(|v| v.is_empty()).unwrap_or(true)
}

/// A fixture's throughput budget, widened for an unoptimized build.
///
/// The factor is smaller than the 43x above, on purpose. A debug build needs
/// most of a 10 s budget for one megabyte, so eight times leaves room without
/// letting a real starvation sit undetected for minutes.
pub fn throughput_budget(base: Duration) -> Duration {
    if unoptimized_binary_under_test() {
        base * 8
    } else {
        base
    }
}

/// Poll `cond` every 10 ms until it returns true or the default deadline
/// passes. Panics with `what` on timeout.
///
/// # Wait on the thing, not on a sign of it
///
/// **A test may only wait on the condition it is about to assert. If the only
/// available signal is a precursor, say so in a comment and explain why it is
/// sufficient.**
///
/// The second sentence is the whole rule. There are legitimate precursors, and
/// a rule with no way out gets ignored. Requiring the author to write down
/// *why* the precursor is enough is the mechanism: usually they cannot, and
/// that is the moment they go and find the direct signal — which is often
/// already in hand. The rig knows every daemon's pid, and the tests that
/// flaked were watching its socket file instead.
///
/// Four flakes on this project came from breaking it, and each looked like a
/// slow machine on its own:
///
/// - waiting for the first `Screen` frame to mean the child had written its
///   escapes, when attach publishes the current screen and it may be empty;
/// - waiting for a losing `pty run` to notice its own daemon die, when the
///   answer — somebody else has published this name — was already on disk;
/// - waiting for a socket file to vanish to mean the daemon had finished,
///   when it unlinks the socket and keeps working.
///
/// See [`wait_for_process_gone`] for the third.
pub fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    wait_until_for(what, deadline(), &mut cond);
}

/// Poll `cond` until true or `timeout` passes.
pub fn wait_until_for(what: &str, timeout: Duration, cond: &mut dyn FnMut() -> bool) {
    let start = Instant::now();
    loop {
        if cond() {
            return;
        }
        if start.elapsed() > timeout {
            panic!("timed out after {timeout:?} waiting for: {what}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Poll `cond` until true or `timeout` passes; returns whether it became true.
pub fn poll_for(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if cond() {
            return true;
        }
        if start.elapsed() > timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// True if `pid` is alive (signal 0).
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill(2) with signal 0 only checks for existence/permission.
    let r = unsafe { libc::kill(pid, 0) };
    if r == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Has `pid` exited? A zombie counts: it has finished and only waits for
/// whoever adopted it to reap it.
///
/// **Check that a process is gone with this, not `!pid_alive`.** A daemon,
/// or a child that outlived it, is an orphan by the time it dies, and the
/// init process or nearest subreaper that adopted it reaps it whenever it
/// gets round to it. Until then it still answers `kill(pid, 0)`. `pty kill`
/// and `pty rm` wait for the exit, not for somebody else's reap, so asserting
/// `!pid_alive` right after them races that reaper. Measured on 2026-09-28:
/// `rm_immediate_reuse` failed 230 runs in 400 with 16 at once, and every
/// time the old daemon was in state `Z` and adopted by the user's systemd.
/// A process that is still running answers `false`, so this proves the exit
/// as strictly as before.
pub fn process_exited(pid: i32) -> bool {
    pid <= 0 || pty_core::registry::has_process_exited_for_reap(pid)
}

/// Wait until `pid` is gone: exited AND reaped, or exited and unreapable.
///
/// **This is the direct signal for "the daemon has finished".** A test that
/// waits for the socket file to disappear is waiting on a precursor: the daemon
/// unlinks its socket and then keeps going — it still flushes events and may
/// touch the lock files a test is about to write. Under load that gap is wide
/// enough to lose a race in.
///
/// `has_process_exited_for_reap` rather than `!pid_alive`: an unreaped child
/// still answers `kill(pid, 0)`, so the cheap predicate would wait out the
/// whole budget for a process that has already died.
pub fn wait_for_process_gone(pid: i32) {
    wait_until(&format!("process {pid} to exit"), || {
        pid <= 0 || pty_core::registry::has_process_exited_for_reap(pid)
    });
}

/// Send `sig` to `pid`, ignoring errors.
pub fn kill_pid(pid: i32, sig: i32) {
    if pid > 0 {
        // SAFETY: plain kill(2).
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

fn mkdtemp(prefix: &str) -> PathBuf {
    let template = CString::new(format!("{prefix}XXXXXX")).unwrap();
    let mut buf = template.into_bytes_with_nul();
    // SAFETY: buf is a NUL-terminated template ending in XXXXXX.
    let p = unsafe { libc::mkdtemp(buf.as_mut_ptr() as *mut libc::c_char) };
    assert!(!p.is_null(), "mkdtemp failed: {}", std::io::Error::last_os_error());
    // SAFETY: mkdtemp rewrote the template in place; it is still NUL-terminated.
    let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string();
    PathBuf::from(s)
}

/// Options for [`Rig::daemon`].
#[derive(Debug, Clone, Default)]
pub struct DaemonOpts {
    pub no_display_name: bool,
    pub display_name: Option<String>,
    pub tags: Vec<(String, String)>,
    pub ephemeral: bool,
    /// `--env K=V` (persisted overlay).
    pub env: Vec<(String, String)>,
    /// `--unset-env K`.
    pub unset_env: Vec<String>,
    pub isolate_env: bool,
    pub cwd: Option<PathBuf>,
    /// Shortcut for `--tag keep=true`.
    pub keep: bool,
    /// Extra process environment for the `pty run` invocation itself
    /// (not the persisted overlay).
    pub invoke_env: Vec<(String, String)>,
    /// Environment variables removed from the `pty run` invocation.
    pub invoke_unset: Vec<String>,
}

impl DaemonOpts {
    pub fn no_display_name() -> Self {
        Self {
            no_display_name: true,
            ..Default::default()
        }
    }

    pub fn keep() -> Self {
        Self {
            no_display_name: true,
            keep: true,
            ..Default::default()
        }
    }

    pub fn tag(mut self, k: &str, v: &str) -> Self {
        self.tags.push((k.to_string(), v.to_string()));
        self
    }

    pub fn ephemeral(mut self) -> Self {
        self.ephemeral = true;
        self
    }

    pub fn with_env(mut self, k: &str, v: &str) -> Self {
        self.env.push((k.to_string(), v.to_string()));
        self
    }

    pub fn invoke_env(mut self, k: &str, v: &str) -> Self {
        self.invoke_env.push((k.to_string(), v.to_string()));
        self
    }
}

/// A daemon started by [`Rig::daemon`].
#[derive(Debug, Clone)]
pub struct Daemon {
    pub root: PathBuf,
    pub id: String,
    /// The `pty run -d` invocation's output.
    pub launch: Out,
}

impl Daemon {
    pub fn socket_path(&self) -> PathBuf {
        self.root.join(format!("{}.sock", self.id))
    }

    pub fn meta_path(&self) -> PathBuf {
        self.root.join(format!("{}.json", self.id))
    }

    pub fn pid_path(&self) -> PathBuf {
        self.root.join(format!("{}.pid", self.id))
    }

    /// Parsed `<id>.json` (untyped so unknown fields stay visible).
    pub fn meta(&self) -> Value {
        read_json(&self.meta_path()).unwrap_or_else(|| panic!("no metadata for {}", self.id))
    }

    /// The daemon pid from `<id>.pid`.
    pub fn pid(&self) -> i32 {
        read_pid_file(&self.pid_path()).unwrap_or_else(|| panic!("no pid for {}", self.id))
    }
}

/// Read and parse a JSON file; `None` if it is missing or not (yet) valid.
pub fn read_json(path: &Path) -> Option<Value> {
    let raw = std::fs::read(path).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// Read a pid file.
pub fn read_pid_file(path: &Path) -> Option<i32> {
    let raw = std::fs::read_to_string(path).ok()?;
    raw.trim().parse().ok()
}

/// One isolated registry (a temp `PTY_ROOT`) plus the environment policy.
pub struct Rig {
    tmp: PathBuf,
    root: PathBuf,
    home: PathBuf,
    extra_roots: std::sync::Mutex<Vec<PathBuf>>,
    path_prefix: std::sync::Mutex<Vec<PathBuf>>,
    keep_tmp: bool,
}

impl Rig {
    /// Create a fresh rig under `/tmp/pc-XXXXXX`.
    pub fn new() -> Rig {
        let tmp = mkdtemp("/tmp/pc-");
        let root = tmp.clone();
        let home = tmp.join("home");
        std::fs::create_dir_all(&home).unwrap();
        Rig {
            tmp,
            root,
            home,
            extra_roots: std::sync::Mutex::new(Vec::new()),
            path_prefix: std::sync::Mutex::new(Vec::new()),
            keep_tmp: std::env::var("PC_KEEP").map(|v| v == "1").unwrap_or(false),
        }
    }

    /// The `PTY_ROOT` this rig runs in.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The temp `HOME`.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The temp directory everything lives under.
    pub fn tmp(&self) -> &Path {
        &self.tmp
    }

    /// A fresh empty directory under the rig (short path); torn down with the
    /// rig, and scanned for pid files at teardown.
    pub fn make_root(&self) -> PathBuf {
        let dir = mkdtemp(&format!("{}/r-", self.tmp.display()));
        self.extra_roots.lock().unwrap().push(dir.clone());
        dir
    }

    /// A fresh empty directory under the rig (for cwd tests and the like).
    pub fn make_dir(&self, name: &str) -> PathBuf {
        let dir = self.tmp.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The rig's base environment: the ambient env minus [`SCRUBBED_ENV`],
    /// plus `PTY_ROOT`, `PTY_ROOT_LEGACY_SILENT=1`, `TERM`, `HOME`.
    pub fn base_env(&self) -> BTreeMap<String, String> {
        let mut env: BTreeMap<String, String> = std::env::vars().collect();
        for k in SCRUBBED_ENV {
            env.remove(*k);
        }
        env.insert("PTY_ROOT".into(), self.root.to_string_lossy().into_owned());
        env.insert("PTY_ROOT_LEGACY_SILENT".into(), "1".into());
        env.insert("TERM".into(), "xterm-256color".into());
        env.insert("HOME".into(), self.home.to_string_lossy().into_owned());
        env.insert("PATH".into(), self.path());
        env
    }

    /// `PATH` for processes the rig starts: every stub directory registered
    /// with [`Rig::stub_bin`] first, then the ambient `PATH`.
    pub fn path(&self) -> String {
        let ambient = std::env::var("PATH").unwrap_or_default();
        let prefix = self.path_prefix.lock().unwrap();
        if prefix.is_empty() {
            return ambient;
        }
        let mut parts: Vec<String> = prefix.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        parts.push(ambient);
        parts.join(":")
    }

    /// Install an executable script as `<name>` in a directory that is put
    /// at the front of `PATH` for everything the rig runs (stub `fabric`,
    /// fake commands). Returns the script's path.
    pub fn stub_bin(&self, name: &str, script: &str) -> PathBuf {
        let dir = self.tmp.join("stubbin");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut prefix = self.path_prefix.lock().unwrap();
        if !prefix.contains(&dir) {
            prefix.push(dir);
        }
        path
    }

    /// A `Command` for the binary under test with the rig's base environment.
    /// The cwd is the rig's temp dir.
    pub fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(pty_bin());
        cmd.args(args);
        cmd.env_clear();
        for (k, v) in self.base_env() {
            cmd.env(k, v);
        }
        cmd.current_dir(&self.tmp);
        cmd
    }

    /// A `Command` with an environment built from scratch: only `PATH`,
    /// `HOME` (the rig's), and `extra`. No `PTY_ROOT` unless `extra` sets it.
    pub fn command_clean(&self, extra: &[(&str, &str)], args: &[&str]) -> Command {
        let mut cmd = Command::new(pty_bin());
        cmd.args(args);
        cmd.env_clear();
        cmd.env("PATH", self.path());
        cmd.env("HOME", &self.home);
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd.current_dir(&self.tmp);
        cmd
    }

    /// Run the binary under test with the rig's base environment.
    pub fn pty(&self, args: &[&str]) -> Out {
        self.run(self.command(args), None)
    }

    /// Run with extra environment merged over the base environment.
    pub fn pty_env(&self, extra: &[(&str, &str)], args: &[&str]) -> Out {
        let mut cmd = self.command(args);
        for (k, v) in extra {
            cmd.env(k, v);
        }
        self.run(cmd, None)
    }

    /// Run with some variables removed from the base environment (and
    /// `extra` merged in).
    pub fn pty_env_unset(&self, unset: &[&str], extra: &[(&str, &str)], args: &[&str]) -> Out {
        let mut cmd = self.command(args);
        for k in unset {
            cmd.env_remove(k);
        }
        for (k, v) in extra {
            cmd.env(k, v);
        }
        self.run(cmd, None)
    }

    /// Run with a from-scratch environment (see [`Rig::command_clean`]).
    pub fn pty_clean(&self, extra: &[(&str, &str)], args: &[&str]) -> Out {
        self.run(self.command_clean(extra, args), None)
    }

    /// Run with `stdin` fed from `bytes`.
    pub fn pty_stdin(&self, bytes: &[u8], args: &[&str]) -> Out {
        self.run(self.command(args), Some(bytes.to_vec()))
    }

    /// Run with `stdin` fed from `bytes` and extra environment.
    pub fn pty_stdin_env(&self, bytes: &[u8], extra: &[(&str, &str)], args: &[&str]) -> Out {
        let mut cmd = self.command(args);
        for (k, v) in extra {
            cmd.env(k, v);
        }
        self.run(cmd, Some(bytes.to_vec()))
    }

    /// Run with the current directory set to `cwd`.
    pub fn pty_in(&self, cwd: &Path, args: &[&str]) -> Out {
        let mut cmd = self.command(args);
        cmd.current_dir(cwd);
        self.run(cmd, None)
    }

    /// Run an arbitrary prepared command with the 30 s timeout, collecting
    /// stdout and stderr. Detached daemons that inherit the pipes do not
    /// block collection: after the process exits the readers get two more
    /// seconds and whatever arrived by then is returned.
    pub fn run(&self, cmd: Command, stdin: Option<Vec<u8>>) -> Out {
        self.run_with_timeout(cmd, stdin, CLI_TIMEOUT)
    }

    /// [`Rig::run`] with a custom timeout, for commands that are expected
    /// to block (an attach without a tty) and are killed on purpose.
    pub fn run_with_timeout(&self, mut cmd: Command, stdin: Option<Vec<u8>>, timeout: Duration) -> Out {
        cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let mut child: Child = cmd.spawn().expect("spawn pty binary");
        if let Some(bytes) = stdin {
            let mut si = child.stdin.take().unwrap();
            std::thread::spawn(move || {
                let _ = si.write_all(&bytes);
            });
        }
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx_out, rx_out) = mpsc::channel();
        let (tx_err, rx_err) = mpsc::channel();
        std::thread::spawn(move || {
            let mut r = stdout;
            let mut buf = Vec::new();
            let _ = r.read_to_end(&mut buf);
            let _ = tx_out.send(buf);
        });
        std::thread::spawn(move || {
            let mut r = stderr;
            let mut buf = Vec::new();
            let _ = r.read_to_end(&mut buf);
            let _ = tx_err.send(buf);
        });
        let start = Instant::now();
        let mut timed_out = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break st.code().unwrap_or(-1),
                Ok(None) => {}
                Err(e) => panic!("wait on pty: {e}"),
            }
            if start.elapsed() > timeout {
                timed_out = true;
                let _ = child.kill();
                let _ = child.wait();
                break -1;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let grace = Duration::from_secs(2);
        let stdout = rx_out.recv_timeout(grace).unwrap_or_default();
        let stderr = rx_err.recv_timeout(grace).unwrap_or_default();
        Out {
            status,
            stdout,
            stderr,
            timed_out,
        }
    }

    /// Run the CLI inside a real terminal (for `attach`, `peek -f`, prompts).
    /// The environment is the rig's base environment; `extra` is merged in.
    pub fn pty_tty(&self, args: &[&str], rows: u16, cols: u16) -> Session {
        self.pty_tty_env(&[], args, rows, cols)
    }

    /// [`Rig::pty_tty`] with extra environment.
    pub fn pty_tty_env(&self, extra: &[(&str, &str)], args: &[&str], rows: u16, cols: u16) -> Session {
        // `env -u` scrubs the ambient variables the testkit would otherwise
        // pass through; the rig's fixed variables are set on the same line.
        let mut env_args: Vec<String> = Vec::new();
        for k in SCRUBBED_ENV {
            env_args.push("-u".into());
            env_args.push((*k).into());
        }
        let mut merged = self.base_env();
        for (k, v) in extra {
            merged.insert((*k).into(), (*v).into());
        }
        for k in ["PTY_ROOT", "PTY_ROOT_LEGACY_SILENT", "TERM", "HOME", "PATH"] {
            if let Some(v) = merged.get(k) {
                env_args.push(format!("{k}={v}"));
            }
        }
        for (k, v) in extra {
            env_args.push(format!("{k}={v}"));
        }
        env_args.push(pty_bin().to_string_lossy().into_owned());
        for a in args {
            env_args.push((*a).into());
        }
        let refs: Vec<&str> = env_args.iter().map(String::as_str).collect();
        Session::spawn(
            "env",
            &refs,
            SpawnOptions {
                rows: Some(rows),
                cols: Some(cols),
                cwd: Some(self.tmp.clone()),
                env: Vec::new(),
            },
        )
        .expect("spawn pty in a tty")
    }

    /// Run the CLI inside a real terminal and expose the raw bytes it writes
    /// (no terminal emulation), for tests about exact escape sequences
    /// (`attach` sanitize/exit trailers, `peek -f`). Environment as
    /// [`Rig::pty_tty_env`]; `unset` removes variables.
    pub fn pty_tty_raw(&self, extra: &[(&str, &str)], unset: &[&str], args: &[&str], rows: u16, cols: u16) -> TtyProc {
        let mut env = self.base_env();
        for (k, v) in extra {
            env.insert((*k).into(), (*v).into());
        }
        for k in unset {
            env.remove(*k);
        }
        TtyProc::spawn(pty_bin(), args, &env, &self.tmp, rows, cols)
    }

    /// Start a detached session with `pty run -d --id <id> ... -- cmd` and
    /// wait for its socket and metadata to appear.
    pub fn daemon(&self, id: &str, cmd: &[&str], opts: DaemonOpts) -> Daemon {
        let d = self.daemon_try(id, cmd, opts);
        assert_eq!(d.launch.status, 0, "pty run -d --id {id} failed: {}", d.launch.summary());
        self.wait_for_daemon(&d);
        d
    }

    /// Like [`Rig::daemon`] but returns whatever `pty run -d` did without
    /// asserting or waiting (for tests about failed launches).
    pub fn daemon_try(&self, id: &str, cmd: &[&str], opts: DaemonOpts) -> Daemon {
        let mut args: Vec<String> = vec!["run".into(), "-d".into(), "--id".into(), id.into()];
        if opts.no_display_name {
            args.push("--no-display-name".into());
        }
        if let Some(dn) = &opts.display_name {
            args.push("--name".into());
            args.push(dn.clone());
        }
        for (k, v) in &opts.tags {
            args.push("--tag".into());
            args.push(format!("{k}={v}"));
        }
        if opts.keep {
            args.push("--tag".into());
            args.push("keep=true".into());
        }
        if opts.ephemeral {
            args.push("-e".into());
        }
        for (k, v) in &opts.env {
            args.push("--env".into());
            args.push(format!("{k}={v}"));
        }
        for k in &opts.unset_env {
            args.push("--unset-env".into());
            args.push(k.clone());
        }
        if opts.isolate_env {
            args.push("--isolate-env".into());
        }
        if let Some(cwd) = &opts.cwd {
            args.push("--cwd".into());
            args.push(cwd.to_string_lossy().into_owned());
        }
        args.push("--".into());
        for c in cmd {
            args.push((*c).into());
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut command = self.command(&refs);
        for k in &opts.invoke_unset {
            command.env_remove(k);
        }
        for (k, v) in &opts.invoke_env {
            command.env(k, v);
        }
        let launch = self.run(command, None);
        Daemon {
            root: self.root.clone(),
            id: id.to_string(),
            launch,
        }
    }

    /// Wait (≤ 30 s) for `<id>.sock` and `<id>.json`. A command that exits
    /// at once may already have torn its socket down: metadata carrying an
    /// `exitCode` counts as published too.
    pub fn wait_for_daemon(&self, d: &Daemon) {
        let sock = d.socket_path();
        let meta = d.meta_path();
        wait_until_for(
            &format!("{} socket and metadata", d.id),
            Duration::from_secs(30),
            &mut || match read_json(&meta) {
                Some(m) => sock.exists() || m.get("exitCode").is_some(),
                None => false,
            },
        );
    }

    /// Path helpers for a session id in this rig's root.
    pub fn socket_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.sock"))
    }

    pub fn meta_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    pub fn pid_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.pid"))
    }

    /// Parsed metadata for `id`, if present.
    pub fn meta(&self, id: &str) -> Option<Value> {
        read_json(&self.meta_path(id))
    }

    /// The daemon pid for `id`, if present.
    pub fn pid(&self, id: &str) -> Option<i32> {
        read_pid_file(&self.pid_path(id))
    }

    /// `pty list --json` parsed (empty stdout → `[]`).
    pub fn list_json(&self) -> Vec<Value> {
        let out = self.pty(&["list", "--json"]);
        assert_eq!(out.status, 0, "list --json failed: {}", out.summary());
        let s = out.stdout();
        if s.trim().is_empty() {
            return Vec::new();
        }
        serde_json::from_str::<Value>(&s)
            .unwrap_or_else(|e| panic!("list --json is not JSON ({e}): {s}"))
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// The `list --json` entry named `id`, if any.
    pub fn list_entry(&self, id: &str) -> Option<Value> {
        self.list_json().into_iter().find(|e| e["name"] == id)
    }

    /// Wait until `<id>.json` carries an `exitCode` (the session exited and
    /// was preserved).
    pub fn wait_for_exit(&self, id: &str) {
        wait_until(&format!("{id} to exit"), || {
            self.meta(id).map(|m| m.get("exitCode").is_some()).unwrap_or(false)
        });
    }

    /// Wait until the session's registry files are gone.
    pub fn wait_for_gone(&self, id: &str) {
        wait_until(&format!("{id} to be removed"), || {
            !self.meta_path(id).exists() && !self.socket_path(id).exists()
        });
    }

    /// Wait until `list --json` reports `id` with `status`.
    pub fn wait_for_status(&self, id: &str, status: &str) {
        wait_until(&format!("{id} to be {status}"), || {
            self.list_entry(id).map(|e| e["status"] == status).unwrap_or(false)
        });
    }

    /// Open a raw protocol connection to `id`'s socket.
    pub fn connect(&self, id: &str) -> Conn {
        Conn::open(&self.socket_path(id))
    }

    /// Every pid recorded in a `*.pid` file under the rig (root and extra
    /// roots).
    pub fn recorded_pids(&self) -> Vec<i32> {
        let mut dirs = vec![self.root.clone()];
        dirs.extend(self.extra_roots.lock().unwrap().iter().cloned());
        let mut pids = Vec::new();
        let mut stack = dirs;
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().map(|x| x == "pid").unwrap_or(false)
                    && let Some(pid) = read_pid_file(&p)
                {
                    pids.push(pid);
                }
            }
        }
        pids.sort_unstable();
        pids.dedup();
        pids
    }

    /// Stop every recorded daemon PID except PID 1 and the test process.
    pub fn teardown_daemons(&self) {
        let me = std::process::id() as i32;
        let pids: Vec<i32> = self
            .recorded_pids()
            .into_iter()
            .filter(|&p| p > 1 && p != me)
            .collect();
        for &pid in &pids {
            kill_pid(pid, libc::SIGTERM);
        }
        let _ = poll_for(Duration::from_secs(2), || pids.iter().all(|&p| !pid_alive(p)));
        for &pid in &pids {
            if pid_alive(pid) {
                kill_pid(pid, libc::SIGKILL);
            }
        }
    }
}

impl Default for Rig {
    fn default() -> Self {
        Rig::new()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.teardown_daemons();
        if !self.keep_tmp {
            let _ = std::fs::remove_dir_all(&self.tmp);
        }
    }
}

/// A raw socket client speaking the daemon wire protocol.
pub struct Conn {
    stream: UnixStream,
    reader: PacketReader,
    queue: std::collections::VecDeque<Packet>,
    seen: Vec<MessageType>,
    eof: bool,
}

impl Conn {
    /// Connect to a daemon socket.
    pub fn open(path: &Path) -> Conn {
        let stream = UnixStream::connect(path)
            .unwrap_or_else(|e| panic!("connect {}: {e}", path.display()));
        Conn {
            stream,
            reader: PacketReader::new(),
            queue: Default::default(),
            seen: Vec::new(),
            eof: false,
        }
    }

    /// Try to connect; `None` on failure.
    pub fn try_open(path: &Path) -> Option<Conn> {
        let stream = UnixStream::connect(path).ok()?;
        Some(Conn {
            stream,
            reader: PacketReader::new(),
            queue: Default::default(),
            seen: Vec::new(),
            eof: false,
        })
    }

    /// The underlying stream.
    pub fn stream(&self) -> &UnixStream {
        &self.stream
    }

    /// Write raw bytes to the socket.
    pub fn write_raw(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.stream.write_all(bytes)?;
        self.stream.flush()
    }

    /// Send an ATTACH with a terminal size.
    pub fn attach(&mut self, rows: u16, cols: u16) {
        self.write_raw(&protocol::encode_attach(rows, cols)).expect("send ATTACH");
    }

    /// Join as a read-only client. Node has no read-only ATTACH: a PEEK makes
    /// the socket read-only, and the daemon keeps streaming DATA to it.
    pub fn attach_neutral(&mut self, _rows: u16, _cols: u16) {
        self.write_raw(&protocol::encode_peek(false, false)).expect("send PEEK");
    }

    /// Send a PEEK request.
    pub fn peek(&mut self, plain: bool, full: bool) {
        self.write_raw(&protocol::encode_peek(plain, full)).expect("send PEEK");
    }

    /// Send a RESIZE.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.write_raw(&protocol::encode_resize(rows, cols)).expect("send RESIZE");
    }

    /// Send DATA.
    pub fn data(&mut self, bytes: &[u8]) {
        self.write_raw(&protocol::encode_data(bytes)).expect("send DATA");
    }

    /// Send a DETACH.
    pub fn detach(&mut self) {
        let _ = self.write_raw(&protocol::encode_detach());
    }

    /// Send a STATUS request.
    pub fn status(&mut self) {
        self.write_raw(&protocol::encode_status()).expect("send STATUS");
    }

    /// Shut down the write half (signals EOF to the daemon).
    pub fn shutdown_write(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Write);
    }

    /// True once the daemon closed the connection.
    pub fn is_eof(&self) -> bool {
        self.eof
    }

    /// The next packet, waiting up to `timeout`. `None` on timeout or EOF.
    /// A framing error (declared length over the cap) panics with the error.
    pub fn next_packet(&mut self, timeout: Duration) -> Option<Packet> {
        match self.next_packet_result(timeout) {
            Ok(p) => p,
            Err(e) => panic!("protocol read error: {e}"),
        }
    }

    /// Like [`Conn::next_packet`] but surfaces framing errors.
    pub fn next_packet_result(&mut self, timeout: Duration) -> std::io::Result<Option<Packet>> {
        let start = Instant::now();
        loop {
            if let Some(p) = self.queue.pop_front() {
                self.seen.push(p.type_);
                return Ok(Some(p));
            }
            if self.eof {
                return Ok(None);
            }
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Ok(None);
            }
            self.stream
                .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))
                .ok();
            let mut buf = [0u8; 65536];
            match self.stream.read(&mut buf) {
                Ok(0) => {
                    self.eof = true;
                }
                Ok(n) => {
                    let packets = self.reader.feed(&buf[..n])?;
                    self.queue.extend(packets);
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    return Ok(None);
                }
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {
                    self.eof = true;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Wait for the next packet of `type_`, skipping others (which are still
    /// recorded in [`Conn::sequence`]).
    pub fn wait_for(&mut self, type_: MessageType, timeout: Duration) -> Option<Packet> {
        let start = Instant::now();
        loop {
            let remaining = timeout.saturating_sub(start.elapsed());
            let p = self.next_packet(remaining)?;
            if p.type_ == type_ {
                return Some(p);
            }
        }
    }

    /// Collect packets until an EXIT arrives, the daemon closes the socket,
    /// or `timeout` passes.
    pub fn collect_until_exit(&mut self, timeout: Duration) -> Vec<Packet> {
        let start = Instant::now();
        let mut out = Vec::new();
        loop {
            let remaining = timeout.saturating_sub(start.elapsed());
            match self.next_packet(remaining) {
                Some(p) => {
                    let is_exit = p.type_ == MessageType::Exit;
                    out.push(p);
                    if is_exit {
                        break;
                    }
                }
                None => break,
            }
        }
        out
    }

    /// Collect everything that arrives within `quiet` of silence.
    pub fn drain(&mut self, quiet: Duration) -> Vec<Packet> {
        let mut out = Vec::new();
        while let Some(p) = self.next_packet(quiet) {
            out.push(p);
        }
        out
    }

    /// Every packet type received so far, in order.
    pub fn sequence(&self) -> Vec<MessageType> {
        self.seen.clone()
    }

    /// Send STATUS and return the parsed JSON response.
    pub fn status_json(&mut self, timeout: Duration) -> Value {
        self.status();
        let p = self
            .wait_for(MessageType::Status, timeout)
            .expect("STATUS response");
        serde_json::from_slice(&p.payload).expect("STATUS payload is JSON")
    }
}

/// Concatenate the payloads of all DATA packets.
pub fn data_bytes(packets: &[Packet]) -> Vec<u8> {
    let mut v = Vec::new();
    for p in packets {
        if p.type_ == MessageType::Data {
            v.extend_from_slice(&p.payload);
        }
    }
    v
}

// ── Assertion helpers ──

/// Assert the exit status.
#[track_caller]
pub fn expect_status(out: &Out, status: i32) {
    assert_eq!(out.status, status, "unexpected exit status: {}", out.summary());
}

/// Assert a non-zero exit status.
#[track_caller]
pub fn expect_failure(out: &Out) {
    assert_ne!(out.status, 0, "expected failure: {}", out.summary());
}

/// stdout as a string (lossy).
pub fn stdout_str(out: &Out) -> String {
    out.stdout()
}

/// stderr as a string (lossy).
pub fn stderr_str(out: &Out) -> String {
    out.stderr()
}

/// Assert `hay` contains `needle`.
#[track_caller]
pub fn expect_contains(hay: &str, needle: &str) {
    assert!(hay.contains(needle), "expected {needle:?} in:\n{hay}");
}

/// Assert `hay` does not contain `needle`.
#[track_caller]
pub fn expect_not_contains(hay: &str, needle: &str) {
    assert!(!hay.contains(needle), "did not expect {needle:?} in:\n{hay}");
}

/// Assert `hay` matches the regex `re`.
#[track_caller]
pub fn expect_regex(hay: &str, re: &str) {
    let r = regex::Regex::new(re).expect("valid regex");
    assert!(r.is_match(hay), "expected /{re}/ to match:\n{hay}");
}

/// Assert `hay` does not match the regex `re`.
#[track_caller]
pub fn expect_not_regex(hay: &str, re: &str) {
    let r = regex::Regex::new(re).expect("valid regex");
    assert!(!r.is_match(hay), "did not expect /{re}/ to match:\n{hay}");
}

/// Number of non-overlapping matches of `re` in `hay`.
pub fn count_regex(hay: &str, re: &str) -> usize {
    regex::Regex::new(re).expect("valid regex").find_iter(hay).count()
}

/// Assert status 0 and parse stdout as JSON.
#[track_caller]
pub fn expect_json(out: &Out) -> Value {
    assert_eq!(out.status, 0, "expected success: {}", out.summary());
    let s = out.stdout();
    serde_json::from_str(&s).unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {}", out.summary()))
}

/// Parse stdout as JSON without checking the status.
#[track_caller]
pub fn parse_json(out: &Out) -> Value {
    let s = out.stdout();
    serde_json::from_str(&s).unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {}", out.summary()))
}

/// stdout split into lines (a trailing newline does not add an empty line).
pub fn expect_lines(out: &Out) -> Vec<String> {
    out.stdout().lines().map(|l| l.to_string()).collect()
}

/// Generate a short unique session id with `prefix`.
pub fn unique_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos()
        % 100_000;
    format!("{prefix}{t}{n}")
}

/// Path of the Node reference checkout, when `PTY_NODE_CHECKOUT` is set.
pub fn node_checkout() -> Option<PathBuf> {
    std::env::var("PTY_NODE_CHECKOUT").ok().filter(|s| !s.is_empty()).map(PathBuf::from)
}

/// The workspace root (two levels above this crate).
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// This crate's `fixtures/` directory.
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// An ISO-8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SS.mmmZ`, like
/// `Date#toISOString`) for now plus `offset_secs` (negative = the past).
pub fn iso_timestamp(offset_secs: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let millis = (now.as_millis() as i64) + offset_secs * 1000;
    let secs = millis.div_euclid(1000);
    let ms = millis.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// Fields for [`write_fake_metadata`].
#[derive(Debug, Clone, Default)]
pub struct FakeMeta {
    pub created_at: Option<String>,
    pub exited_at: Option<String>,
    pub exit_code: Option<i64>,
    pub tags: Option<Vec<(String, String)>>,
    pub display_name: Option<String>,
    pub command: Option<String>,
    pub extra: Vec<(String, Value)>,
}

impl FakeMeta {
    pub fn created(offset_secs: i64) -> Self {
        Self {
            created_at: Some(iso_timestamp(offset_secs)),
            ..Default::default()
        }
    }

    pub fn exited(mut self, offset_secs: i64, code: i64) -> Self {
        self.exited_at = Some(iso_timestamp(offset_secs));
        self.exit_code = Some(code);
        self
    }

    pub fn tag(mut self, k: &str, v: &str) -> Self {
        self.tags.get_or_insert_with(Vec::new).push((k.into(), v.into()));
        self
    }

    pub fn display_name(mut self, dn: &str) -> Self {
        self.display_name = Some(dn.into());
        self
    }

    pub fn extra(mut self, k: &str, v: Value) -> Self {
        self.extra.push((k.into(), v));
        self
    }
}

/// Write a daemon-less `<name>.json` record (`cat`, cwd `/tmp`) so list
/// semantics can be tested without waiting on wall-clock time.
pub fn write_fake_metadata(dir: &Path, name: &str, meta: FakeMeta) {
    let mut m = serde_json::Map::new();
    let command = meta.command.clone().unwrap_or_else(|| "cat".into());
    m.insert("command".into(), Value::String(command.clone()));
    m.insert("args".into(), Value::Array(vec![]));
    m.insert("displayCommand".into(), Value::String(command));
    m.insert("cwd".into(), Value::String("/tmp".into()));
    m.insert(
        "createdAt".into(),
        Value::String(meta.created_at.unwrap_or_else(|| iso_timestamp(0))),
    );
    if let Some(e) = meta.exited_at {
        m.insert("exitedAt".into(), Value::String(e));
    }
    if let Some(c) = meta.exit_code {
        m.insert("exitCode".into(), Value::from(c));
    }
    if let Some(tags) = meta.tags {
        let mut t = serde_json::Map::new();
        for (k, v) in tags {
            t.insert(k, Value::String(v));
        }
        m.insert("tags".into(), Value::Object(t));
    }
    if let Some(dn) = meta.display_name {
        m.insert("displayName".into(), Value::String(dn));
    }
    for (k, v) in meta.extra {
        m.insert(k, v);
    }
    std::fs::write(dir.join(format!("{name}.json")), Value::Object(m).to_string()).unwrap();
}

/// The GEOMETRY wire type (10).
pub const GEOMETRY: MessageType = MessageType::Geometry;

/// A readable name for a wire type in sequence assertions.
pub fn type_name(t: MessageType) -> &'static str {
    match t {
        MessageType::Data => "DATA",
        MessageType::Attach => "ATTACH",
        MessageType::Detach => "DETACH",
        MessageType::Resize => "RESIZE",
        MessageType::Exit => "EXIT",
        MessageType::Screen => "SCREEN",
        MessageType::Peek => "PEEK",
        MessageType::Status => "STATUS",
        MessageType::AcceptedSocketOwnership => "ACCEPTED_SOCKET_OWNERSHIP",
        MessageType::LifecycleCas => "LIFECYCLE_CAS",
        MessageType::Geometry => "GEOMETRY",
        MessageType::Unknown(_) => "UNKNOWN",
    }
}

/// Names of a packet-type sequence.
pub fn sequence_names(seq: &[MessageType]) -> Vec<&'static str> {
    seq.iter().map(|t| type_name(*t)).collect()
}

/// Parse a `"80 9f a0"` hex string into bytes.
pub fn parse_hex(s: &str) -> Vec<u8> {
    s.split_whitespace().map(|h| u8::from_str_radix(h, 16).expect("hex byte")).collect()
}

/// Render bytes as a `"80 9f a0"` hex string.
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

/// A process running inside a real pty, with its raw output captured
/// byte-for-byte (see [`Rig::pty_tty_raw`]).
pub struct TtyProc {
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<Vec<u8>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    output: Vec<u8>,
    exit: Option<i32>,
}

impl TtyProc {
    /// Spawn `bin args` in a fresh pty of `rows`×`cols` with exactly `env`.
    pub fn spawn(
        bin: &Path,
        args: &[&str],
        env: &BTreeMap<String, String>,
        cwd: &Path,
        rows: u16,
        cols: u16,
    ) -> TtyProc {
        use portable_pty::CommandBuilder;
        let pair = pty_spawn::open(rows, cols).expect("openpty");
        let mut cmd = CommandBuilder::new(bin);
        cmd.args(args);
        cmd.cwd(cwd);
        cmd.env_clear();
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd).expect("spawn in pty");
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        let writer = pair.master.take_writer().expect("pty writer");
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        TtyProc {
            master: pair.master,
            writer,
            rx,
            child,
            output: Vec::new(),
            exit: None,
        }
    }

    fn pump(&mut self) {
        while let Ok(chunk) = self.rx.try_recv() {
            self.output.extend_from_slice(&chunk);
        }
    }

    /// Write bytes to the process's terminal (as if typed).
    pub fn write(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    /// Everything the process has written so far.
    pub fn output(&mut self) -> Vec<u8> {
        self.pump();
        self.output.clone()
    }

    /// Output so far as lossy UTF-8.
    pub fn output_str(&mut self) -> String {
        String::from_utf8_lossy(&self.output()).into_owned()
    }

    /// Poll until the output contains `needle` or `timeout` passes.
    pub fn wait_for_bytes(&mut self, needle: &[u8], timeout: Duration) -> bool {
        let start = Instant::now();
        loop {
            self.pump();
            if self.output.windows(needle.len().max(1)).any(|w| w == needle) {
                return true;
            }
            if start.elapsed() > timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Poll until the output contains `needle`.
    pub fn wait_for_text(&mut self, needle: &str, timeout: Duration) -> bool {
        self.wait_for_bytes(needle.as_bytes(), timeout)
    }

    /// Resize the pty (the process gets SIGWINCH).
    pub fn resize(&mut self, rows: u16, cols: u16) {
        use portable_pty::PtySize;
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    /// The child's pid.
    pub fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// Exit code if the process has exited (`-1` for a signal death).
    pub fn try_exit(&mut self) -> Option<i32> {
        if let Some(code) = self.exit {
            return Some(code);
        }
        match self.child.try_wait() {
            Ok(Some(st)) => {
                let code = st.exit_code() as i32;
                self.exit = Some(code);
                Some(code)
            }
            _ => None,
        }
    }

    /// Wait up to `timeout` for the process to exit; returns the exit code
    /// (`None` on timeout). Output keeps being collected.
    pub fn wait_exit(&mut self, timeout: Duration) -> Option<i32> {
        let start = Instant::now();
        loop {
            self.pump();
            if let Some(c) = self.try_exit() {
                // Let the reader thread deliver the tail.
                let _ = poll_for(Duration::from_millis(300), || {
                    self.pump();
                    false
                });
                self.pump();
                return Some(c);
            }
            if start.elapsed() > timeout {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// SIGKILL the process.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for TtyProc {
    fn drop(&mut self) {
        if self.try_exit().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
