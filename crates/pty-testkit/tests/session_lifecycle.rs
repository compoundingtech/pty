//! What a session's life owes the processes around it: the daemon reaps what
//! it starts and holds no more descriptors than it needs; a session outlives
//! every client, however the client goes; a client leaves cleanly however
//! its terminal goes; stopping a session hangs up before it escalates and
//! frees the name at once; a live session stays listed, reachable and
//! serviced; a fault or a shortage in one daemon never takes a session down;
//! an event follower sees each new event exactly once; and replacing the
//! binary on disk breaks nothing that is running.
//!
//! These drive the `pty` binary built from this tree against a private
//! registry per test. Several read `/proc`, so they describe Linux.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pty_core::protocol::{encode_data, encode_detach};
use pty_spawn::{Child, CommandBuilder, MasterPty};
use pty_testkit::{Session, SpawnOptions};

/// Point the testkit at the `pty` built from this workspace.
fn use_local_pty() {
    // CARGO_BIN_EXE_ is only set for the crate that owns the binary, so find
    // it beside this test binary instead.
    let mut dir = std::env::current_exe().expect("test binary path");
    dir.pop(); // deps/
    dir.pop(); // debug/
    let bin = dir.join("pty");
    if bin.exists() {
        // SAFETY: set before any thread that reads it; the tests run this
        // first and never change it afterwards.
        unsafe { std::env::set_var("PTY_BIN", &bin) };
    }
}

/// The local binary, and never whatever `pty` happens to be on PATH.
fn local_bin() -> PathBuf {
    use_local_pty();
    let bin = PathBuf::from(std::env::var("PTY_BIN").unwrap_or_default());
    assert!(
        bin.is_file() && bin.parent().is_some_and(|d| d.ends_with("debug") || d.ends_with("release")),
        "build the binary first: cargo build -p pty (looked for {})",
        bin.display()
    );
    bin
}

/// The caller's own session identity must not leak into these commands, or
/// the nesting guard, the creation-lock delegation or the reap policy would
/// act on it.
const SCRUBBED_ENV: &[&str] = &[
    "PTY_SESSION",
    "PTY_SESSION_GENERATION",
    "PTY_SESSION_DIR",
    "PTY_SERVER_CONFIG",
    "PTY_CREATION_LOCK_OWNER_PID",
    "PTY_REAP_ON_EXIT",
    "PTY_SPAWNER_PID",
];

// ── a private registry ──────────────────────────────────────────────────

struct Registry {
    root: PathBuf,
    bin: PathBuf,
}

impl Registry {
    fn new() -> Registry {
        let bin = local_bin();
        // Short: a session socket path has to fit 104 bytes.
        let root = std::env::temp_dir().join(format!("pl-{}", pty_testkit::server::random_id()));
        std::fs::create_dir_all(&root).expect("create the registry");
        Registry { root, bin }
    }

    fn scrub(&self, cmd: &mut Command) {
        for key in SCRUBBED_ENV {
            cmd.env_remove(key);
        }
        cmd.env("PTY_ROOT", &self.root);
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args).stdin(Stdio::null());
        self.scrub(&mut cmd);
        cmd
    }

    fn pty(&self, args: &[&str]) -> Output {
        self.cmd(args).output().expect("run pty")
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.pty(args);
        assert!(
            out.status.success(),
            "pty {args:?} failed ({:?}): {}{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// `pty run -d` and the daemon's pid.
    fn start(&self, name: &str, argv: &[&str]) -> i32 {
        let mut args = vec!["run", "-d", "--id", name, "--no-display-name", "--"];
        args.extend_from_slice(argv);
        self.ok(&args);
        self.daemon_pid(name).expect("the daemon published its pid")
    }

    fn daemon_pid(&self, name: &str) -> Option<i32> {
        std::fs::read_to_string(self.root.join(format!("{name}.pid")))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    fn socket(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}.sock"))
    }

    fn metadata(&self, name: &str) -> Option<Json> {
        Json::parse(&std::fs::read_to_string(self.root.join(format!("{name}.json"))).ok()?)
    }

    fn list(&self) -> Vec<Json> {
        let text = self.ok(&["list", "--json"]);
        match Json::parse(&text) {
            Some(Json::Arr(rows)) => rows,
            other => panic!("pty list --json is not an array: {other:?} from {text:?}"),
        }
    }

    fn entry(&self, name: &str) -> Option<Json> {
        self.list()
            .into_iter()
            .find(|row| row.get("name").and_then(Json::str) == Some(name))
    }

    fn status(&self, name: &str) -> Option<String> {
        self.entry(name)?.get("status")?.str().map(str::to_string)
    }

    /// `pty peek --plain`, or the failure text when it fails.
    fn peek(&self, name: &str) -> String {
        let mut cmd = self.cmd(&["peek", "--plain", name]);
        let out = output_within(&mut cmd, Duration::from_secs(5));
        match out {
            Some(out) if out.status.success() => {
                let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                if !out.stderr.is_empty() {
                    text.push_str(&format!("<stderr: {}>", String::from_utf8_lossy(&out.stderr).trim()));
                }
                text
            }
            Some(out) => format!(
                "<peek failed: {}{}>",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
            None => "<peek did not answer within 5s>".to_string(),
        }
    }

    fn wait_peek(&self, name: &str, needle: &str, timeout: Duration) -> Result<(), String> {
        let mut last = String::new();
        if wait_until(timeout, || {
            last = self.peek(name);
            last.contains(needle)
        }) {
            Ok(())
        } else {
            Err(format!("{needle:?} never appeared in session {name}; last peek:\n{last}"))
        }
    }

    /// The pids of the clients the daemon counts as attached.
    fn attached_clients(&self, name: &str) -> Option<Vec<i64>> {
        let text = self.ok(&["list", "--json", "--clients"]);
        let rows = Json::parse(&text)?;
        let row = rows
            .arr()
            .iter()
            .find(|row| row.get("name").and_then(Json::str) == Some(name))?
            .clone();
        Some(
            row.get("clients")?
                .arr()
                .iter()
                .filter_map(|c| c.get("pid").and_then(Json::num).map(|p| p as i64))
                .collect(),
        )
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        let mut daemons = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let file = entry.file_name().to_string_lossy().into_owned();
                let Some(name) = file.strip_suffix(".json") else {
                    continue;
                };
                if let Some(pid) = self.daemon_pid(name) {
                    // A paused daemon cannot answer SIGTERM.
                    signal(pid, "CONT");
                    daemons.push(pid);
                } else if let Some(pid) = self.metadata(name).and_then(|m| m.get("daemonPid")?.num()) {
                    daemons.push(pid as i32);
                }
                let _ = self.cmd(&["kill", name]).stdout(Stdio::null()).stderr(Stdio::null()).status();
            }
        }
        // A daemon still delivering its exit writes into the registry on the
        // way out; let it finish before the directory goes.
        wait_until(Duration::from_secs(3), || daemons.iter().all(|&pid| !running(pid)));
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

// ── processes ───────────────────────────────────────────────────────────

fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Run to completion, or kill it at the deadline and say so with `None`.
fn output_within(cmd: &mut Command, timeout: Duration) -> Option<Output> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = stdout.read_to_end(&mut v);
        v
    });
    let err = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = stderr.read_to_end(&mut v);
        v
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Ok(Some(status)) = child.try_wait() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Some(Output {
        status,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

/// `(state, ppid)` from `/proc/<pid>/stat`.
fn proc_state(pid: i32) -> Option<(char, i32)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = &stat[stat.rfind(')')? + 2..];
    let mut fields = tail.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?.parse().ok()?;
    Some((state, ppid))
}

/// Present and not a corpse.
fn running(pid: i32) -> bool {
    matches!(proc_state(pid), Some((state, _)) if state != 'Z' && state != 'X')
}

/// Every process whose parent is `pid`, with its state.
fn children(pid: i32) -> Vec<(i32, char)> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Some(child) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else {
                continue;
            };
            if let Some((state, ppid)) = proc_state(child)
                && ppid == pid
            {
                out.push((child, state));
            }
        }
    }
    out
}

/// The one live child of a daemon: the session's process.
fn session_child(daemon: i32) -> i32 {
    let mut kids = Vec::new();
    assert!(
        wait_until(Duration::from_secs(5), || {
            kids = children(daemon);
            kids.len() == 1 && kids[0].1 != 'Z'
        }),
        "daemon {daemon} should have exactly one live child, has {kids:?}"
    );
    kids[0].0
}

fn fd_targets(pid: i32) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .map(|entries| {
            entries
                .flatten()
                .map(|e| {
                    std::fs::read_link(e.path())
                        .map(|t| t.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| "?".to_string())
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

fn thread_count(pid: i32) -> usize {
    std::fs::read_dir(format!("/proc/{pid}/task"))
        .map(|d| d.count())
        .unwrap_or(0)
}

fn cwd_of(pid: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

fn signal(pid: i32, sig: &str) {
    let _ = Command::new("kill")
        .args(["-s", sig, &pid.to_string()])
        .stderr(Stdio::null())
        .status();
}

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

// ── a terminal a test can close ─────────────────────────────────────────

#[cfg(target_os = "linux")]
const O_NOCTTY: i32 = 0o400;
#[cfg(target_os = "macos")]
const O_NOCTTY: i32 = 0x20000;

/// A real terminal around a client: what a terminal window is to the process
/// in it. Unlike a testkit session it can be closed without killing the
/// process, which is what a window closing does.
struct OuterTerm {
    master: Option<Box<dyn MasterPty + Send>>,
    child: Box<dyn Child + Send + Sync>,
    output: Arc<Mutex<Vec<u8>>>,
    slave: Option<PathBuf>,
    stop: Arc<AtomicBool>,
    reader_done: Arc<AtomicBool>,
}

impl OuterTerm {
    fn spawn(reg: &Registry, argv: &[&str]) -> OuterTerm {
        let pair = pty_spawn::open(24, 80).expect("open a terminal");
        let mut cmd = CommandBuilder::new(argv[0]);
        cmd.args(&argv[1..]);
        for key in SCRUBBED_ENV {
            cmd.env_remove(key);
        }
        cmd.env("PTY_ROOT", &reg.root);
        cmd.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(cmd).expect("spawn in the terminal");
        drop(pair.slave);
        let slave = pair.master.tty_name();
        // No writer is taken: portable-pty's writer types `\n` and EOF into
        // the terminal when it is dropped, which no closing window does.
        let mut reader = pair.master.try_clone_reader().expect("reader");
        let output = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let reader_done = Arc::new(AtomicBool::new(false));
        {
            let (output, stop, done) = (output.clone(), stop.clone(), reader_done.clone());
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                while !stop.load(Ordering::SeqCst) {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => output.lock().unwrap().extend_from_slice(&buf[..n]),
                    }
                }
                drop(reader);
                done.store(true, Ordering::SeqCst);
            });
        }
        OuterTerm {
            master: Some(pair.master),
            child,
            output,
            slave,
            stop,
            reader_done,
        }
    }

    fn pid(&self) -> i32 {
        self.child.process_id().expect("a pid") as i32
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    fn wait_for(&self, needle: &str, timeout: Duration) -> Result<(), String> {
        if wait_until(timeout, || self.text().contains(needle)) {
            Ok(())
        } else {
            Err(format!("{needle:?} never appeared; the terminal shows:\n{}", self.text()))
        }
    }

    /// Close the terminal: every master descriptor goes, as when a window
    /// closes, and the kernel hangs up the terminal behind the client.
    fn hang_up(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.master.take();
        // The reader holds the last master descriptor while it is blocked in
        // read(2). One byte through the slave wakes it so it lets go.
        if let Some(slave) = &self.slave {
            use std::os::unix::fs::OpenOptionsExt;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(O_NOCTTY)
                .open(slave)
            {
                let _ = f.write_all(b"\n");
            }
        }
        assert!(
            wait_until(Duration::from_secs(3), || self.reader_done.load(Ordering::SeqCst)),
            "the terminal did not close"
        );
    }

    fn wait_exit(&mut self, timeout: Duration) -> Option<portable_pty::ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.wait_exit(Duration::from_secs(5));
    }
}

impl Drop for OuterTerm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.try_wait();
    }
}

// ── systemd scopes ──────────────────────────────────────────────────────

fn systemd_user_scopes_work() -> bool {
    Command::new("systemd-run")
        .args(["--user", "--scope", "-q", "true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// `pty run -d` inside a fresh transient scope named `unit`.
fn start_in_scope(reg: &Registry, unit: &str, properties: &[&str], name: &str, argv: &[&str]) -> i32 {
    let mut cmd = Command::new("systemd-run");
    cmd.args(["--user", "--scope", "-q", "--unit", unit]);
    for p in properties {
        cmd.args(["-p", p]);
    }
    cmd.arg(&reg.bin)
        .args(["run", "-d", "--id", name, "--no-display-name", "--"])
        .args(argv)
        .stdin(Stdio::null());
    reg.scrub(&mut cmd);
    let out = cmd.output().expect("systemd-run");
    assert!(
        out.status.success(),
        "systemd-run pty run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    reg.daemon_pid(name).expect("daemon pid")
}

fn stop_scope(unit: &str) {
    let _ = Command::new("systemctl")
        .args(["--user", "stop", &format!("{unit}.scope")])
        .stderr(Stdio::null())
        .status();
}

/// The cgroup directory of `pid`, which must be the scope `unit`.
fn scope_cgroup(pid: i32, unit: &str) -> PathBuf {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).expect("cgroup");
    let path = text
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .expect("a unified cgroup")
        .trim()
        .to_string();
    assert!(
        path.ends_with(&format!("/{unit}.scope")),
        "{pid} is in {path}, not in the scope this test made"
    );
    PathBuf::from(format!("/sys/fs/cgroup{path}"))
}

// ── zombie-reaping ──────────────────────────────────────────────────────

/// The daemon's only child is the session's process, and it is collected
/// before the exit is announced: while the daemon lingers to deliver EXIT,
/// nothing it started is left behind as a corpse.
#[test]
fn a_daemon_leaves_no_defunct_child_once_its_child_has_exited() {
    let reg = Registry::new();
    let daemon = reg.start("reaped", &["sh", "-c", "read line; exit 3"]);
    let child = session_child(daemon);
    assert!(running(child));

    let (tx, _rx) = mpsc::channel();
    let (mut socket, state) =
        pty_testkit::server::connect(&reg.root, "reaped", 24, 80, tx).expect("attach");
    socket.write_all(&encode_data(b"go\r")).unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || state.exited.load(Ordering::Acquire)),
        "no EXIT"
    );
    assert_eq!(state.exit_code.load(Ordering::Relaxed), 3, "EXIT carries the child's code");
    // The daemon is in its exit grace now, still running, still the parent
    // of anything it failed to collect.
    let leftovers = children(daemon);
    assert!(running(daemon), "the daemon should linger briefly to deliver EXIT");
    assert!(
        leftovers.is_empty(),
        "daemon {daemon} still has children after announcing the exit: {leftovers:?}"
    );
    assert!(wait_until(Duration::from_secs(5), || !running(daemon)), "daemon never exited");
}

// ── fd-hygiene ──────────────────────────────────────────────────────────

/// Hundreds of client comings and goings — attaches that detach, attaches
/// that just drop, peeks, stats, sends, a client listing, a client killed
/// mid-stream — leave the daemon with exactly the descriptors and threads
/// it had before.
#[test]
fn client_churn_leaves_the_daemon_with_the_descriptors_and_threads_it_started_with() {
    let reg = Registry::new();
    let daemon = reg.start("churn", &["sh", "-c", "while :; do echo tick; sleep 0.05; done"]);
    reg.wait_peek("churn", "tick", Duration::from_secs(5)).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let fds_before = fd_targets(daemon);
    let threads_before = thread_count(daemon);

    for i in 0..30 {
        let (tx, rx) = mpsc::channel();
        let (mut socket, _state) =
            pty_testkit::server::connect(&reg.root, "churn", 24, 80, tx).expect("attach");
        rx.recv_timeout(Duration::from_secs(5)).expect("the replay arrived");
        if i % 2 == 0 {
            let _ = socket.write_all(&encode_detach());
        }
        let _ = socket.shutdown(std::net::Shutdown::Both);
        drop(socket);
        reg.peek("churn");
        if i % 5 == 0 {
            reg.ok(&["stats", "--json", "churn"]);
            reg.ok(&["send", "churn", "x"]);
            reg.ok(&["list", "--json", "--clients"]);
        }
    }
    let mut client = OuterTerm::spawn(&reg, &[reg.bin.to_str().unwrap(), "attach", "churn"]);
    client.wait_for("tick", Duration::from_secs(5)).unwrap();
    client.kill();

    let mut fds_after = Vec::new();
    let settled = wait_until(Duration::from_secs(5), || {
        fds_after = fd_targets(daemon);
        fds_after.len() == fds_before.len() && thread_count(daemon) == threads_before
    });
    assert!(
        settled,
        "daemon {daemon} holds {} descriptors and {} threads after the churn, {} and {} before\n\
         before: {fds_before:?}\nafter: {fds_after:?}",
        fds_after.len(),
        thread_count(daemon),
        fds_before.len(),
        threads_before
    );
}

/// The session's process inherits its terminal and nothing else: no
/// session socket, no listener, no config pipe, no pty master.
#[test]
fn the_session_child_holds_only_its_terminal() {
    let reg = Registry::new();
    let first = reg.start("first", &["sleep", "300"]);
    let second = reg.start("second", &["sleep", "300"]);
    for daemon in [first, second] {
        let child = session_child(daemon);
        let fds = fd_targets(child);
        assert!(fds.len() >= 3, "child {child} has fds {fds:?}");
        assert!(
            fds.iter().all(|t| t.starts_with("/dev/pts/")),
            "session child {child} holds more than its terminal: {fds:?}"
        );
    }
}

/// A session is its own daemon, so the number of sessions never meets one
/// process's descriptor limit: two dozen sessions all start and run under
/// a limit far below what they hold between them.
#[test]
fn many_sessions_start_and_run_under_a_small_descriptor_limit() {
    let reg = Registry::new();
    let count = 24;
    let script = format!(
        "ulimit -n 64 || exit 90; i=1; while [ $i -le {count} ]; do \
           \"$0\" run -d --id many-$i --no-display-name -- sh -c \"echo UP-$i; exec sleep 300\" >/dev/null || exit $i; \
           i=$((i+1)); done"
    );
    let mut cmd = Command::new("sh");
    cmd.args(["-c", &script, reg.bin.to_str().unwrap()]).stdin(Stdio::null());
    reg.scrub(&mut cmd);
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "starting session {:?} under ulimit -n 64 failed: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let running_count = reg
        .list()
        .iter()
        .filter(|row| row.get("status").and_then(Json::str) == Some("running"))
        .count();
    assert_eq!(running_count, count, "every session is running");
    for i in [1, count / 2, count] {
        reg.wait_peek(&format!("many-{i}"), &format!("UP-{i}"), Duration::from_secs(5))
            .unwrap();
    }
}

// ── session-survives-client ─────────────────────────────────────────────

fn assert_session_untouched(reg: &Registry, name: &str, daemon: i32, child: i32) {
    assert!(running(daemon), "the daemon {daemon} of {name} died with its client");
    assert!(running(child), "the child {child} of {name} died with its client");
    assert_eq!(reg.status(name).as_deref(), Some("running"));
    reg.ok(&["send", name, "still-here\r"]);
    reg.wait_peek(name, "still-here", Duration::from_secs(5)).unwrap();
}

/// A terminal window closing hangs up the client in it. The client goes;
/// the session and its process do not.
#[test]
fn closing_the_attached_clients_terminal_leaves_the_session_running() {
    let reg = Registry::new();
    let daemon = reg.start("hup", &["sh", "-c", "echo HUP-READY; exec cat"]);
    let child = session_child(daemon);
    let mut client = OuterTerm::spawn(&reg, &[reg.bin.to_str().unwrap(), "attach", "hup"]);
    client.wait_for("HUP-READY", Duration::from_secs(5)).unwrap();

    client.hang_up();
    assert!(client.wait_exit(Duration::from_secs(5)).is_some(), "the client outlived its terminal");
    assert_session_untouched(&reg, "hup", daemon, child);
}

/// A client killed outright takes nothing with it.
#[test]
fn killing_the_attached_client_leaves_the_session_running() {
    let reg = Registry::new();
    let daemon = reg.start("killed", &["sh", "-c", "echo KILL-READY; exec cat"]);
    let child = session_child(daemon);
    let mut client = OuterTerm::spawn(&reg, &[reg.bin.to_str().unwrap(), "attach", "killed"]);
    client.wait_for("KILL-READY", Duration::from_secs(5)).unwrap();

    client.kill();
    assert_session_untouched(&reg, "killed", daemon, child);
}

/// A session created attached (`pty run` without `-d`) belongs to its
/// daemon, not to the terminal it was created from.
#[test]
fn a_session_created_attached_survives_its_terminal_closing() {
    let reg = Registry::new();
    let mut client = OuterTerm::spawn(
        &reg,
        &[
            reg.bin.to_str().unwrap(),
            "run",
            "--id",
            "fg",
            "--no-display-name",
            "--",
            "sh",
            "-c",
            "echo FG-READY; exec cat",
        ],
    );
    client.wait_for("FG-READY", Duration::from_secs(5)).unwrap();
    let daemon = reg.daemon_pid("fg").expect("daemon");
    let child = session_child(daemon);

    client.hang_up();
    assert!(client.wait_exit(Duration::from_secs(5)).is_some(), "the client outlived its terminal");
    assert_session_untouched(&reg, "fg", daemon, child);
}

/// A detached session started from a short-lived terminal (a shell that
/// then goes away with its window) keeps running after that terminal
/// closes.
#[test]
fn a_detached_session_survives_the_terminal_that_started_it() {
    let reg = Registry::new();
    let script = format!(
        "'{}' run -d --id bg --no-display-name -- sh -c 'echo BG-READY; exec cat' && echo LAUNCHED; exec cat",
        reg.bin.display()
    );
    let mut launcher = OuterTerm::spawn(&reg, &["sh", "-c", &script]);
    launcher.wait_for("LAUNCHED", Duration::from_secs(5)).unwrap();
    let daemon = reg.daemon_pid("bg").expect("daemon");
    let child = session_child(daemon);

    launcher.hang_up();
    assert!(launcher.wait_exit(Duration::from_secs(5)).is_some(), "the launcher outlived its terminal");
    std::thread::sleep(Duration::from_millis(200));
    assert_session_untouched(&reg, "bg", daemon, child);
}

/// A terminal emulator, a desktop shell restart or a logout under systemd
/// tears down the scope the terminal's processes live in. A session
/// started from that terminal must not be part of what is torn down.
#[test]
#[ignore = "fails on main: the daemon stays in the launching terminal's systemd scope and dies (143) when that scope is stopped"]
fn a_session_survives_the_scope_it_was_started_from_being_stopped() {
    if !systemd_user_scopes_work() {
        eprintln!("skipped: no systemd user manager to make a scope with");
        return;
    }
    let reg = Registry::new();
    let unit = format!("pty-lifecycle-{}", pty_testkit::server::random_id());
    let daemon = start_in_scope(&reg, &unit, &[], "scoped", &["sh", "-c", "echo SCOPE-READY; exec cat"]);
    let child = session_child(daemon);
    reg.wait_peek("scoped", "SCOPE-READY", Duration::from_secs(5)).unwrap();

    stop_scope(&unit);
    std::thread::sleep(Duration::from_millis(300));
    let status = reg.status("scoped");
    let exit_code = reg.metadata("scoped").and_then(|m| m.get("exitCode").and_then(Json::num));
    assert!(
        running(daemon) && running(child),
        "stopping the launcher's scope ended the session: status {status:?}, exit code {exit_code:?}, \
         daemon alive {}, child alive {}",
        running(daemon),
        running(child)
    );
    assert_session_untouched(&reg, "scoped", daemon, child);
}

// ── client-hangup-clean-exit ────────────────────────────────────────────

/// When its terminal goes away the client ends — by the hangup or on its
/// own — and never by aborting, and the daemon stops counting it.
#[test]
fn a_client_whose_terminal_hangs_up_ends_without_aborting() {
    let reg = Registry::new();
    reg.start("gone", &["sh", "-c", "echo GONE-READY; exec cat"]);
    let err = reg.root.join("client.err");
    let script = format!("exec '{}' attach gone 2>'{}'", reg.bin.display(), err.display());
    let mut client = OuterTerm::spawn(&reg, &["sh", "-c", &script]);
    client.wait_for("GONE-READY", Duration::from_secs(5)).unwrap();
    let pid = client.pid() as i64;
    assert_eq!(reg.attached_clients("gone"), Some(vec![pid]));

    client.hang_up();
    let status = client.wait_exit(Duration::from_secs(2)).expect("the client ended within 2s");
    assert_ne!(status.signal(), Some("Aborted"), "the client aborted: {status:?}");
    assert_ne!(status.exit_code(), 101, "the client panicked: {status:?}");
    let stderr = std::fs::read_to_string(&err).unwrap_or_default();
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(
        wait_until(Duration::from_secs(3), || reg.attached_clients("gone") == Some(vec![])),
        "the daemon still counts the departed client: {:?}",
        reg.attached_clients("gone")
    );
}

/// A client that does not die of the hangup signal — started under
/// `nohup`, or by a parent that ignores SIGHUP — must still notice that
/// its terminal is gone and leave. Otherwise it stays attached for ever,
/// holding the session to a size no terminal has.
#[test]
#[ignore = "fails on main: with SIGHUP ignored the client never exits after its terminal hangs up and stays attached"]
fn a_client_ignoring_the_hangup_signal_still_leaves_when_its_terminal_hangs_up() {
    let reg = Registry::new();
    reg.start("nohup", &["sh", "-c", "echo NOHUP-READY; exec cat"]);
    let err = reg.root.join("client.err");
    let script = format!(
        "trap '' HUP; exec '{}' attach nohup 2>'{}'",
        reg.bin.display(),
        err.display()
    );
    let mut client = OuterTerm::spawn(&reg, &["sh", "-c", &script]);
    client.wait_for("NOHUP-READY", Duration::from_secs(5)).unwrap();
    let pid = client.pid();

    client.hang_up();
    // Output for a client whose terminal is gone.
    reg.ok(&["send", "nohup", "after-the-hangup\r"]);
    let status = client.wait_exit(Duration::from_secs(3));
    let still_attached = reg.attached_clients("nohup");
    assert!(
        status.is_some(),
        "client {pid} is still running 3s after its terminal hung up (state {:?}); \
         the daemon still counts it as attached: {still_attached:?}",
        proc_state(pid)
    );
    let status = status.unwrap();
    assert_ne!(status.signal(), Some("Aborted"), "{status:?}");
    assert!(!std::fs::read_to_string(&err).unwrap_or_default().contains("panicked"));
}

/// Output piped into a reader that stops early (`| head -1`) ends the
/// command quietly, not with a panic and exit 101.
#[test]
fn output_into_a_reader_that_closes_early_ends_without_a_panic() {
    let reg = Registry::new();
    reg.start(
        "big",
        &["sh", "-c", "i=0; while [ $i -lt 3000 ]; do echo \"line $i of a long scrollback\"; i=$((i+1)); done; exec cat"],
    );
    reg.wait_peek("big", "line 2999", Duration::from_secs(8)).unwrap();

    for args in [
        &["peek", "--plain", "--full", "big"][..],
        &["peek", "--full", "big"][..],
        &["events", "--recent", "big"][..],
    ] {
        let mut child = reg
            .cmd(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut first = [0u8; 1];
        let _ = stdout.read(&mut first);
        drop(stdout);
        let out = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_ne!(out.status.code(), Some(101), "pty {args:?} panicked: {stderr}");
        assert!(!stderr.contains("panicked"), "pty {args:?}: {stderr}");
    }
}

/// With no terminal at all, attaching to a missing session fails with a
/// message and creates nothing; attaching to a live one streams it and
/// ends with the session's code. Neither panics.
#[test]
fn attaching_without_a_terminal_neither_panics_nor_creates_a_session() {
    let reg = Registry::new();
    let mut cmd = reg.cmd(&["attach", "nosuch"]);
    let out = output_within(&mut cmd, Duration::from_secs(5)).expect("attach answered");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert_ne!(out.status.code(), Some(101), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(stderr.contains("nosuch"), "the refusal names the session: {stderr}");
    assert!(reg.list().is_empty(), "attaching created a session: {:?}", reg.list());

    reg.start("streamed", &["sh", "-c", "read x; echo NO-TTY-OK; exit 4"]);
    let child = reg
        .cmd(&["attach", "streamed"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || reg.attached_clients("streamed").is_some_and(|c| c.len() == 1)),
        "the terminal-less client never attached"
    );
    reg.ok(&["send", "streamed", "go\r"]);
    let pid = child.id();
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(child.wait_with_output());
    });
    let out = done_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|_| {
            signal(pid as i32, "KILL");
            panic!("the terminal-less client did not end with its session")
        })
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert_eq!(out.status.code(), Some(4), "the session's code; stderr: {stderr}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("NO-TTY-OK"));
}

// ── foreground-process ──────────────────────────────────────────────────

/// The cwd a session reports is the directory it was started in. It is
/// never taken from a descendant that moved elsewhere, and `stats` names
/// the session's own process, not a descendant.
#[test]
fn the_reported_cwd_and_process_are_the_sessions_own_never_a_descendants() {
    let reg = Registry::new();
    let a = reg.root.join("a");
    let b = reg.root.join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let script = format!("(cd '{}' && exec sleep 300) & exec sleep 301", b.display());
    reg.ok(&[
        "run", "-d", "--id", "where", "--no-display-name", "--cwd", a.to_str().unwrap(), "--", "sh", "-c", &script,
    ]);
    let daemon = reg.daemon_pid("where").unwrap();
    let child = session_child(daemon);
    let mut descendant = None;
    assert!(wait_until(Duration::from_secs(5), || {
        descendant = children(child).first().map(|(p, _)| *p);
        descendant.is_some_and(|d| cwd_of(d).as_deref() == Some(b.as_path()))
    }));

    let entry = reg.entry("where").unwrap();
    assert_eq!(entry.get("cwd").and_then(Json::str), a.to_str());
    let stats = Json::parse(&reg.ok(&["stats", "--json", "where"])).unwrap();
    let pid = stats.get("process").and_then(|p| p.get("pid")).and_then(Json::num);
    assert_eq!(pid, Some(child as f64), "stats names the session's process, not {descendant:?}");
}

/// A session whose directory was deleted still reports a path, not the
/// kernel's `(deleted)` decoration of it.
#[test]
fn a_deleted_working_directory_is_reported_without_a_deleted_suffix() {
    let reg = Registry::new();
    let gone = reg.root.join("gone");
    std::fs::create_dir_all(&gone).unwrap();
    reg.ok(&[
        "run", "-d", "--id", "orphaned", "--no-display-name", "--cwd", gone.to_str().unwrap(), "--", "sleep", "300",
    ]);
    let child = session_child(reg.daemon_pid("orphaned").unwrap());
    std::fs::remove_dir_all(&gone).unwrap();
    assert!(wait_until(Duration::from_secs(2), || cwd_of(child)
        .is_some_and(|c| c.to_string_lossy().ends_with(" (deleted)"))));

    let cwd = reg.entry("orphaned").unwrap().get("cwd").and_then(Json::str).map(str::to_string);
    assert_eq!(cwd.as_deref(), gone.to_str());
    let stats = reg.ok(&["stats", "orphaned"]);
    assert!(!stats.contains("(deleted)"), "{stats}");
}

// ── stop-session-cleanly ────────────────────────────────────────────────

/// Stopping a session hangs up its shell first. A shell that exits on the
/// hangup is left to do so: it gets no other signal, is not killed, and the
/// stop does not wait out a grace period it did not need.
#[test]
fn stopping_a_session_hangs_up_a_shell_that_then_exits_by_itself() {
    let reg = Registry::new();
    let log = reg.root.join("signals.log");
    let ready = reg.root.join("ready");
    let shell = write_script(
        &reg.root,
        "shell.sh",
        "trap 'echo HUP >> \"$1\"; exit 0' HUP\n\
         trap 'echo TERM >> \"$1\"; exit 0' TERM\n\
         echo ready > \"$2\"\n\
         while :; do read line; done\n",
    );
    reg.start("hangup", &["sh", shell.to_str().unwrap(), log.to_str().unwrap(), ready.to_str().unwrap()]);
    assert!(wait_until(Duration::from_secs(5), || ready.exists()));

    let started = Instant::now();
    reg.ok(&["kill", "hangup"]);
    let took = started.elapsed();
    let signals = std::fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(signals.trim(), "HUP", "the shell should see a hangup and nothing else");
    let code = reg.metadata("hangup").and_then(|m| m.get("exitCode").and_then(Json::num));
    assert_eq!(code, Some(0.0), "the shell exited by itself, it was not killed");
    assert!(took < Duration::from_millis(1500), "the stop waited {took:?} for a shell that left at once");
}

/// A program running under the session's shell — the pty's foreground job
/// — gets the hangup too, the way it would when a terminal closes, and is
/// only terminated if it is still there after a grace period.
#[test]
#[ignore = "fails on main: the stop sends SIGTERM to every descendant at the same moment it hangs up the child, so a foreground job never sees a hangup or any grace"]
fn stopping_a_session_hangs_up_its_foreground_job_before_terminating_it() {
    let reg = Registry::new();
    let log = reg.root.join("job.log");
    let ready = reg.root.join("ready");
    let job = write_script(
        &reg.root,
        "job.sh",
        "trap 'echo HUP >> \"$1\"; exit 0' HUP\n\
         trap 'echo TERM >> \"$1\"; exit 0' TERM\n\
         echo ready > \"$2\"\n\
         while :; do read line; done\n",
    );
    // The shell handles the hangup itself (as an interactive shell or an
    // agent that saves its state does) rather than dying of it on the spot,
    // so its own exit does not race a hangup onto the job.
    let script = format!(
        "trap 'exit 0' HUP; '{}' '{}' '{}'; exit 0",
        job.display(),
        log.display(),
        ready.display()
    );
    reg.start("job", &["sh", "-c", &script]);
    assert!(wait_until(Duration::from_secs(5), || ready.exists()));

    // Note when the job first reacts, from outside it.
    let (seen_tx, seen_rx) = mpsc::channel();
    {
        let log = log.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if let Ok(text) = std::fs::read_to_string(&log)
                    && let Some(first) = text.lines().next()
                {
                    let _ = seen_tx.send((first.to_string(), Instant::now()));
                    return;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        });
    }
    let stop_at = Instant::now();
    reg.ok(&["kill", "job"]);
    let (first, seen_at) = seen_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the job saw some signal");
    let after = seen_at.duration_since(stop_at);
    assert!(
        first == "HUP" || after >= Duration::from_millis(1000),
        "the foreground job's first signal was {first}, {after:?} after the stop began: \
         it was terminated without a hangup and without a grace period"
    );
}

// ── session-id-stable ───────────────────────────────────────────────────

/// A session's identity — its id, generation, daemon and the name its
/// process sees — is fixed for its life. Stopping and removing other
/// sessions, before and after it in any order, renumbers nothing.
#[test]
fn closing_other_sessions_never_changes_a_sessions_identity() {
    let reg = Registry::new();
    let names = ["s-a", "s-b", "s-c", "s-d"];
    for name in names {
        reg.start(name, &["sh", "-c", "echo \"I-AM-$PTY_SESSION\"; exec cat"]);
    }
    let identity = |name: &str| {
        let m = reg.metadata(name).expect("metadata");
        (
            m.get("generation").and_then(Json::str).map(str::to_string),
            m.get("daemonPid").and_then(Json::num),
        )
    };
    let before_a = identity("s-a");
    let before_c = identity("s-c");

    for gone in ["s-b", "s-d"] {
        reg.ok(&["kill", gone]);
        reg.ok(&["rm", gone]);
    }
    reg.start("s-e", &["cat"]);
    reg.ok(&["kill", "s-e"]);

    assert_eq!(identity("s-a"), before_a);
    assert_eq!(identity("s-c"), before_c);
    let listed: Vec<String> = reg
        .list()
        .iter()
        .filter(|r| r.get("status").and_then(Json::str) == Some("running"))
        .filter_map(|r| r.get("name").and_then(Json::str).map(str::to_string))
        .collect();
    assert_eq!(listed.len(), 2, "{listed:?}");
    for name in ["s-a", "s-c"] {
        assert!(listed.iter().any(|n| n == name), "{name} missing from {listed:?}");
        reg.ok(&["send", name, &format!("to-{name}\r")]);
        reg.wait_peek(name, &format!("I-AM-{name}"), Duration::from_secs(5)).unwrap();
        reg.wait_peek(name, &format!("to-{name}"), Duration::from_secs(5)).unwrap();
    }
}

// ── live-session-listed ─────────────────────────────────────────────────

/// A session whose process runs is listed as running, and answers, on
/// every look: through tag and name writes, siblings coming and going, its
/// pid file vanishing, and its daemon being paused and resumed the way a
/// host sleep does.
#[test]
fn a_running_session_stays_listed_and_reachable_through_churn_and_a_pause() {
    let reg = Registry::new();
    let daemon = reg.start("live", &["sh", "-c", "echo LIVE-READY; exec cat"]);
    let child = session_child(daemon);
    let listed_running = || reg.status("live").as_deref() == Some("running");

    for i in 0..10 {
        reg.ok(&["tag", "live", &format!("round={i}")]);
        reg.ok(&["rename", "live", &format!("Live {i}")]);
        let sibling = format!("sib-{i}");
        reg.start(&sibling, &["sh", "-c", "exit 0"]);
        assert!(listed_running(), "live missing or not running in round {i}: {:?}", reg.entry("live"));
        let _ = reg.pty(&["kill", &sibling]);
        let _ = reg.pty(&["rm", &sibling]);
        assert!(listed_running(), "live missing or not running in round {i}: {:?}", reg.entry("live"));
    }

    std::fs::remove_file(reg.root.join("live.pid")).unwrap();
    assert!(listed_running(), "a missing pid file hid a live session: {:?}", reg.entry("live"));

    signal(daemon, "STOP");
    std::thread::sleep(Duration::from_millis(500));
    let while_paused = reg.status("live");
    signal(daemon, "CONT");
    assert_eq!(while_paused.as_deref(), Some("running"), "a paused daemon's session is still live");
    assert!(running(child));
    reg.wait_peek("live", "LIVE-READY", Duration::from_secs(5)).unwrap();
    reg.ok(&["send", "live", "answered\r"]);
    reg.wait_peek("live", "answered", Duration::from_secs(5)).unwrap();
}

// ── pty-master-held ─────────────────────────────────────────────────────

/// Clients coming and going, at other sizes, and one killed mid-stream,
/// never touch the session's terminal: the child reads and writes it
/// throughout and never sees it close.
#[test]
fn the_childs_terminal_stays_open_through_client_churn() {
    let reg = Registry::new();
    let daemon = reg.start(
        "held",
        &["sh", "-c", "stty -echo; while IFS= read -r l; do echo \"got:$l\"; done; echo READ-ENDED; exec sleep 300"],
    );
    let child = session_child(daemon);
    for i in 0..15u16 {
        let (tx, rx) = mpsc::channel();
        let (socket, _state) =
            pty_testkit::server::connect(&reg.root, "held", 10 + i, 40 + i, tx).expect("attach");
        rx.recv_timeout(Duration::from_secs(5)).expect("replay");
        let _ = socket.shutdown(std::net::Shutdown::Both);
        reg.ok(&["send", "held", &format!("round-{i}\r")]);
    }
    let mut client = OuterTerm::spawn(&reg, &[reg.bin.to_str().unwrap(), "attach", "held"]);
    client.wait_for("round-14", Duration::from_secs(5)).unwrap();
    client.kill();

    reg.ok(&["send", "held", "final\r"]);
    reg.wait_peek("held", "got:final", Duration::from_secs(5)).unwrap();
    assert!(!reg.peek("held").contains("READ-ENDED"), "the child lost its terminal");
    assert!(running(child));
    assert!(fd_targets(daemon).iter().any(|t| t == "/dev/ptmx"));
}

/// A child can close every descriptor on its terminal for a moment — a
/// program that detaches its standard streams and later reopens
/// `/dev/tty` — while it keeps running. The session must keep serving the
/// terminal, so what it writes after reopening is shown, or else say the
/// terminal is gone. It must not go on listing a running session whose
/// output nobody reads.
#[test]
#[ignore = "fails on main: the daemon stops reading the pty master for good after one EIO while the child lives; later output is never shown and the session still lists as running"]
fn output_after_the_child_reopens_its_terminal_still_reaches_the_session() {
    let reg = Registry::new();
    let daemon = reg.start(
        "reopen",
        &["sh", "-c", "echo BEFORE-CLOSE; exec 0<&- 1>&- 2>&-; sleep 1; exec 1>/dev/tty; echo AFTER-REOPEN; exec sleep 300"],
    );
    let child = session_child(daemon);
    reg.wait_peek("reopen", "BEFORE-CLOSE", Duration::from_secs(5)).unwrap();

    let mut screen = String::new();
    let mut status = None;
    let served_or_reported = wait_until(Duration::from_secs(5), || {
        screen = reg.peek("reopen");
        status = reg.status("reopen");
        screen.contains("AFTER-REOPEN") || status.as_deref() != Some("running")
    });
    assert!(
        served_or_reported,
        "child {child} (alive: {}) reopened its terminal and wrote to it, but the session shows \
         nothing new and still lists as {status:?}; screen:\n{screen}",
        running(child)
    );
}

// ── daemon-stays-responsive ─────────────────────────────────────────────

/// A daemon that runs out of descriptors keeps its listener, and serves
/// again as soon as descriptors come back.
#[test]
fn a_daemon_out_of_descriptors_serves_again_once_they_are_free() {
    let reg = Registry::new();
    let script = format!(
        "ulimit -n 24 && exec '{}' run -d --id emfile --no-display-name -- sh -c 'echo EMFILE-READY; exec cat'",
        reg.bin.display()
    );
    let mut cmd = Command::new("sh");
    cmd.args(["-c", &script]).stdin(Stdio::null()).stdout(Stdio::null());
    reg.scrub(&mut cmd);
    assert!(cmd.status().unwrap().success());
    let daemon = reg.daemon_pid("emfile").unwrap();
    reg.wait_peek("emfile", "EMFILE-READY", Duration::from_secs(5)).unwrap();

    let mut held = Vec::new();
    for _ in 0..20 {
        if let Ok(s) = std::os::unix::net::UnixStream::connect(reg.socket("emfile")) {
            held.push(s);
        }
    }
    let exhausted = wait_until(Duration::from_secs(3), || fd_targets(daemon).len() >= 23);
    assert!(exhausted, "the daemon never ran short: {:?}", fd_targets(daemon));
    std::thread::sleep(Duration::from_millis(200));
    for s in &held {
        let _ = s.shutdown(std::net::Shutdown::Both);
    }
    drop(held);

    assert!(running(daemon));
    reg.wait_peek("emfile", "EMFILE-READY", Duration::from_secs(5)).unwrap();
    let (tx, rx) = mpsc::channel();
    let _client = pty_testkit::server::connect(&reg.root, "emfile", 24, 80, tx).expect("attach");
    rx.recv_timeout(Duration::from_secs(5)).expect("a client is served after the shortage");
}

/// A daemon paused (as a sleeping host pauses it) answers the clients that
/// arrived meanwhile once it resumes, and takes new ones.
#[test]
fn a_paused_daemon_serves_the_clients_that_arrived_while_it_was_paused() {
    let reg = Registry::new();
    let daemon = reg.start("paused", &["sh", "-c", "echo PAUSE-READY; exec cat"]);
    reg.wait_peek("paused", "PAUSE-READY", Duration::from_secs(5)).unwrap();

    signal(daemon, "STOP");
    let waiting = reg
        .cmd(&["peek", "--plain", "paused"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1000));
    signal(daemon, "CONT");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(waiting.wait_with_output());
    });
    let out = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the peek that waited was answered")
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("PAUSE-READY"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    reg.ok(&["send", "paused", "resumed\r"]);
    reg.wait_peek("paused", "resumed", Duration::from_secs(5)).unwrap();
}

/// SIGTERM ends a daemon completely: the process and its socket both go.
#[test]
fn sigterm_ends_the_daemon_and_its_socket_together() {
    let reg = Registry::new();
    let daemon = reg.start("term", &["cat"]);
    signal(daemon, "TERM");
    assert!(wait_until(Duration::from_secs(5), || !running(daemon)), "daemon ignored SIGTERM");
    assert!(!reg.socket("term").exists(), "the socket outlived its daemon");
    assert_eq!(reg.status("term").as_deref(), Some("exited"));
}

// ── runtime-crash-free (and daemon-stays-responsive) ────────────────────

/// A daemon that cannot start a thread for a new client (its cgroup is at
/// `pids.max`, as a busy user slice can be) must refuse only that client.
/// It must still be serving once threads are available again — not alive,
/// listed as running and refusing every connection for the rest of its
/// life.
#[test]
#[ignore = "fails on main: a failed thread spawn panics the accept thread, which drops the listener; the daemon and child live on, listed as running, refusing every client"]
fn a_daemon_that_cannot_start_a_thread_keeps_serving_once_it_can() {
    if !systemd_user_scopes_work() {
        eprintln!("skipped: no systemd user manager to make a scope with");
        return;
    }
    let reg = Registry::new();
    let unit = format!("pty-lifecycle-{}", pty_testkit::server::random_id());
    let daemon = start_in_scope(&reg, &unit, &["TasksMax=200"], "tasks", &["sh", "-c", "echo TASKS-READY; exec cat"]);
    struct StopScope(String);
    impl Drop for StopScope {
        fn drop(&mut self) {
            stop_scope(&self.0);
        }
    }
    let _scope = StopScope(unit.clone());
    reg.wait_peek("tasks", "TASKS-READY", Duration::from_secs(5)).unwrap();
    let cgroup = scope_cgroup(daemon, &unit);
    std::thread::sleep(Duration::from_millis(300));
    let current: u64 = std::fs::read_to_string(cgroup.join("pids.current"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    // Room for one more thread: the next client's second thread cannot start.
    std::fs::write(cgroup.join("pids.max"), format!("{}", current + 1)).expect("lower pids.max");
    let mut held = Vec::new();
    for _ in 0..3 {
        if let Ok(s) = std::os::unix::net::UnixStream::connect(reg.socket("tasks")) {
            held.push(s);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    std::fs::write(cgroup.join("pids.max"), "max").expect("lift pids.max");
    drop(held);

    let mut screen = String::new();
    let served = wait_until(Duration::from_secs(5), || {
        screen = reg.peek("tasks");
        screen.contains("TASKS-READY")
    });
    let connect = std::os::unix::net::UnixStream::connect(reg.socket("tasks")).map(|_| "connected");
    assert!(
        served,
        "after a thread shortage the daemon {daemon} (alive: {}, listed as {:?}, socket file present: {}) \
         no longer serves: connect gives {connect:?}; peek gives {screen:?}",
        running(daemon),
        reg.status("tasks"),
        reg.socket("tasks").exists()
    );
}

/// Output mixing ZWJ sequences, flags, wide CJK and combining marks does
/// not take the daemon down: it keeps answering peeks and replays to new
/// clients at other sizes.
#[test]
fn complex_emoji_output_leaves_the_daemon_serving() {
    let reg = Registry::new();
    let line = "README 👨\u{200d}👩\u{200d}👧\u{200d}👦 🧑\u{200d}💻 ✅ ⚡ 漢字 café é 🏳\u{fe0f}\u{200d}🌈 🚀";
    let daemon = reg.start("emoji", &["sh", "-c", "i=0; while [ $i -lt 50 ]; do printf '%s\\n' \"$1\"; i=$((i+1)); done; echo EMOJI-DONE; exec cat", "sh", line]);
    reg.wait_peek("emoji", "EMOJI-DONE", Duration::from_secs(5)).unwrap();
    for (rows, cols) in [(24, 80), (10, 17), (40, 13), (24, 80)] {
        let (tx, rx) = mpsc::channel();
        let (socket, _state) =
            pty_testkit::server::connect(&reg.root, "emoji", rows, cols, tx).expect("attach");
        rx.recv_timeout(Duration::from_secs(5)).expect("replay");
        let _ = socket.shutdown(std::net::Shutdown::Both);
    }
    assert!(running(daemon), "the daemon died on the output");
    let screen = reg.peek("emoji");
    assert!(screen.contains("漢字") && screen.contains("café"), "{screen}");
    reg.ok(&["send", "emoji", "after-emoji\r"]);
    reg.wait_peek("emoji", "after-emoji", Duration::from_secs(5)).unwrap();
}

/// A daemon that crashes takes its own session and nothing else.
#[test]
fn one_daemon_crashing_leaves_every_other_session_running() {
    let reg = Registry::new();
    let doomed = reg.start("doomed", &["cat"]);
    let a = reg.start("other-a", &["sh", "-c", "echo A-READY; exec cat"]);
    let b = reg.start("other-b", &["sh", "-c", "echo B-READY; exec cat"]);
    signal(doomed, "KILL");
    assert!(wait_until(Duration::from_secs(5), || !running(doomed)));
    for (name, daemon, ready) in [("other-a", a, "A-READY"), ("other-b", b, "B-READY")] {
        assert!(running(daemon), "{name} went down with another session's daemon");
        reg.wait_peek(name, ready, Duration::from_secs(5)).unwrap();
        reg.ok(&["send", name, "unaffected\r"]);
        reg.wait_peek(name, "unaffected", Duration::from_secs(5)).unwrap();
    }
    assert_eq!(reg.status("doomed").as_deref(), Some("vanished"));
}

// ── event-stream-exact ──────────────────────────────────────────────────

/// A follower of a session's events gets every event after it started
/// following, once each and in order — also when the log crosses its
/// retention limit and is rewritten underneath the follower.
#[test]
#[ignore = "fails on main: the follower treats the retention rewrite as a truncation and replays the retained log, redelivering old and pre-subscription events"]
fn an_events_follower_gets_each_new_event_once_across_log_retention() {
    let reg = Registry::new();
    reg.start(
        "ev",
        &["sh", "-c", "stty -echo; while read n p; do i=0; while [ $i -lt $n ]; do printf '\\033]2;%s%s\\007' \"$p\" \"$i\"; i=$((i+1)); done; echo \"BATCH-$p-DONE\"; done"],
    );
    let log = reg.root.join("ev.events.jsonl");
    reg.ok(&["send", "ev", "700 a\r"]);
    assert!(
        wait_until(Duration::from_secs(10), || std::fs::read_to_string(&log)
            .is_ok_and(|t| t.contains("\"a699\""))),
        "the first batch never reached the log"
    );

    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut follower = reg
        .cmd(&["events", "--json", "ev"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    {
        let lines = lines.clone();
        let stdout = follower.stdout.take().unwrap();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                lines.lock().unwrap().push(line);
            }
        });
    }
    let has = |needle: &str| lines.lock().unwrap().iter().any(|l| l.contains(needle));
    // Following is live once an event emitted now arrives.
    let mut live = false;
    for _ in 0..20 {
        reg.ok(&["emit", "ev", "user.marker"]);
        if wait_until(Duration::from_millis(500), || has("user.marker")) {
            live = true;
            break;
        }
    }
    assert!(live, "the follower never delivered a new event");

    reg.ok(&["send", "ev", "400 b\r"]);
    let got_last = wait_until(Duration::from_secs(10), || has("\"b399\""));
    std::thread::sleep(Duration::from_millis(800));
    let _ = follower.kill();
    let _ = follower.wait();
    assert!(got_last, "the follower never delivered the last new event");

    let titles: Vec<String> = lines
        .lock()
        .unwrap()
        .iter()
        .filter_map(|l| Json::parse(l))
        .filter(|e| e.get("type").and_then(Json::str) == Some("title_change"))
        .filter_map(|e| e.get("value").and_then(Json::str).map(str::to_string))
        .collect();
    let expected: Vec<String> = (0..400).map(|i| format!("b{i}")).collect();
    let stale = titles.iter().filter(|t| t.starts_with('a')).count();
    assert!(
        titles == expected,
        "the follower got {} title events for 400 new ones ({stale} from before it subscribed); first ones: {:?}",
        titles.len(),
        &titles[..titles.len().min(8)]
    );
}

// ── version-skew ────────────────────────────────────────────────────────

/// A paste from the client's terminal reaches a child that asked for
/// bracketed paste with both markers intact, so a program that submits on
/// newline sees one pasted block rather than lines to run.
#[test]
fn a_bracketed_paste_reaches_the_child_intact_through_an_attached_client() {
    let reg = Registry::new();
    reg.start(
        "paste",
        &["sh", "-c", "stty -echo; printf '\\033[?2004h'; echo PASTE-READY; exec cat -v"],
    );
    let mut outer = Session::spawn(
        reg.bin.to_str().unwrap(),
        &["attach", "paste"],
        SpawnOptions {
            env: vec![
                ("PTY_ROOT".to_string(), reg.root.to_string_lossy().into_owned()),
                ("PTY_CREATION_LOCK_OWNER_PID".to_string(), String::new()),
            ],
            ..Default::default()
        },
    )
    .expect("attach in a terminal");
    outer.wait_for_text("PASTE-READY", 8000).unwrap();
    // What a terminal sends for a two-line paste into a 2004 application.
    outer.send_keys("\x1b[200~line 1\rline 2\x1b[201~\r");
    let screen = outer.wait_for_text("line 2^[[201~", 8000).unwrap();
    assert!(screen.text.contains("^[[200~line 1"), "{}", screen.text);
    outer.send_keys("\x1c");
}

// ── binary-replaced-on-disk ─────────────────────────────────────────────

/// Install `pty` as its own file, the way a package does.
fn install_copy(reg: &Registry) -> (PathBuf, PathBuf) {
    let dir = reg.root.join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    let installed = dir.join("pty");
    std::fs::copy(&reg.bin, &installed).expect("install a copy");
    (dir, installed)
}

/// Upgrade in place: write the new file beside it and rename over it, so
/// running processes keep the old, now unlinked, inode.
fn replace_copy(reg: &Registry, installed: &Path) {
    let staged = installed.with_extension("new");
    std::fs::copy(&reg.bin, &staged).unwrap();
    std::fs::rename(&staged, installed).unwrap();
}

/// A running session, and the `pty` commands its process runs, keep
/// working after the binary is replaced on disk; the new binary talks to
/// the old daemon.
#[test]
fn a_running_session_and_its_children_keep_working_after_the_binary_is_replaced() {
    let reg = Registry::new();
    let (dir, installed) = install_copy(&reg);
    let path = format!("{}:{}", dir.display(), std::env::var("PATH").unwrap_or_default());
    let mut cmd = Command::new(&installed);
    cmd.args([
        "run", "-d", "--id", "upgraded", "--no-display-name", "--",
        "sh", "-c", "stty -echo; read x; pty emit user.after-upgrade --text ok; echo \"EMIT-RC=$?\"; exec cat",
    ])
    .env("PATH", &path)
    .stdin(Stdio::null())
    .stdout(Stdio::null());
    reg.scrub(&mut cmd);
    assert!(cmd.status().unwrap().success());
    let daemon = reg.daemon_pid("upgraded").unwrap();

    replace_copy(&reg, &installed);
    let exe = std::fs::read_link(format!("/proc/{daemon}/exe")).unwrap();
    assert!(exe.to_string_lossy().ends_with(" (deleted)"), "the daemon runs the old inode: {exe:?}");

    let new = |args: &[&str]| {
        let mut c = Command::new(&installed);
        c.args(args).stdin(Stdio::null());
        reg.scrub(&mut c);
        c.output().unwrap()
    };
    assert!(new(&["send", "upgraded", "go\r"]).status.success());
    reg.wait_peek("upgraded", "EMIT-RC=0", Duration::from_secs(5)).unwrap();
    let events = std::fs::read_to_string(reg.root.join("upgraded.events.jsonl")).unwrap();
    assert!(events.contains("user.after-upgrade"), "{events}");
    let peek = new(&["peek", "--plain", "upgraded"]);
    assert!(String::from_utf8_lossy(&peek.stdout).contains("EMIT-RC=0"));
    assert!(running(daemon));
}

/// The session picker is a long-lived process. After the binary is
/// replaced under it, it must still be able to start a session: its own
/// executable path now names a deleted file and cannot be spawned.
#[test]
#[ignore = "fails on main: the picker spawns the daemon from current_exe(), which after an in-place upgrade is '<path> (deleted)', so creating a session fails with ENOENT"]
fn the_session_picker_starts_sessions_after_its_binary_is_replaced() {
    let reg = Registry::new();
    let (_dir, installed) = install_copy(&reg);
    let root = reg.root.to_string_lossy().into_owned();
    let mut picker = Session::spawn(
        installed.to_str().unwrap(),
        &["--preselect-new"],
        SpawnOptions {
            rows: Some(24),
            cols: Some(100),
            env: vec![
                ("PTY_ROOT".to_string(), root.clone()),
                ("HOME".to_string(), root.clone()),
                ("SHELL".to_string(), "/bin/sh".to_string()),
                ("PTY_CREATION_LOCK_OWNER_PID".to_string(), String::new()),
            ],
            ..Default::default()
        },
    )
    .expect("open the picker");
    picker.wait_for_text("+ Create new session...", 8000).unwrap();

    replace_copy(&reg, &installed);
    picker.type_str("\r");
    let mut screen = String::new();
    let created = wait_until(Duration::from_secs(8), || {
        screen = picker.screenshot().text;
        reg.list()
            .iter()
            .any(|r| r.get("status").and_then(Json::str) == Some("running"))
            || screen.contains("could not create")
    });
    let sessions = reg.list();
    assert!(
        created && !sessions.is_empty(),
        "no session was created after the upgrade; the picker shows:\n{screen}"
    );
    picker.type_str("\x1c");
}

// ── a small JSON reader ─────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn parse(text: &str) -> Option<Json> {
        let mut p = JsonReader { s: text.as_bytes(), i: 0 };
        let v = p.value()?;
        p.ws();
        (p.i == p.s.len()).then_some(v)
    }

    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    fn num(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }

    fn arr(&self) -> &[Json] {
        match self {
            Json::Arr(a) => a,
            _ => &[],
        }
    }
}

struct JsonReader<'a> {
    s: &'a [u8],
    i: usize,
}

impl JsonReader<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\n' | b'\r' | b'\t') {
            self.i += 1;
        }
    }

    fn eat(&mut self, lit: &str) -> bool {
        if self.s[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Option<Json> {
        self.ws();
        match *self.s.get(self.i)? {
            b'{' => {
                self.i += 1;
                let mut fields = Vec::new();
                self.ws();
                if self.eat("}") {
                    return Some(Json::Obj(fields));
                }
                loop {
                    self.ws();
                    let key = self.string()?;
                    self.ws();
                    if !self.eat(":") {
                        return None;
                    }
                    fields.push((key, self.value()?));
                    self.ws();
                    if self.eat(",") {
                        continue;
                    }
                    return self.eat("}").then_some(Json::Obj(fields));
                }
            }
            b'[' => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.eat("]") {
                    return Some(Json::Arr(items));
                }
                loop {
                    items.push(self.value()?);
                    self.ws();
                    if self.eat(",") {
                        continue;
                    }
                    return self.eat("]").then_some(Json::Arr(items));
                }
            }
            b'"' => self.string().map(Json::Str),
            b't' => self.eat("true").then_some(Json::Bool(true)),
            b'f' => self.eat("false").then_some(Json::Bool(false)),
            b'n' => self.eat("null").then_some(Json::Null),
            _ => {
                let start = self.i;
                while self.i < self.s.len()
                    && matches!(self.s[self.i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
                {
                    self.i += 1;
                }
                std::str::from_utf8(&self.s[start..self.i]).ok()?.parse().ok().map(Json::Num)
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let hex = std::str::from_utf8(self.s.get(self.i..self.i + 4)?).ok()?;
        self.i += 4;
        u32::from_str_radix(hex, 16).ok()
    }

    fn string(&mut self) -> Option<String> {
        if !self.eat("\"") {
            return None;
        }
        let mut out = String::new();
        loop {
            let c = *self.s.get(self.i)?;
            self.i += 1;
            match c {
                b'"' => return Some(out),
                b'\\' => {
                    let e = *self.s.get(self.i)?;
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let mut cp = self.hex4()?;
                            if (0xD800..0xDC00).contains(&cp) && self.eat("\\u") {
                                let lo = self.hex4()?;
                                cp = 0x10000 + ((cp - 0xD800) << 10) + (lo.wrapping_sub(0xDC00) & 0x3FF);
                            }
                            out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                        }
                        _ => return None,
                    }
                }
                _ => {
                    let start = self.i - 1;
                    let mut end = self.i;
                    while end < self.s.len() && self.s[end] != b'"' && self.s[end] != b'\\' {
                        end += 1;
                    }
                    out.push_str(std::str::from_utf8(&self.s[start..end]).ok()?);
                    self.i = end;
                }
            }
        }
    }
}
