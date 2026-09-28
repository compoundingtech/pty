//! Shared harness for the `daemon_*` tests: spawn the built `pty` binary as
//! a daemon (`pty __daemon` with `PTY_SERVER_CONFIG`, the way Node's own
//! daemon tests spawn `server.js`), and a small socket client that records
//! every packet the daemon sends.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use pty_core::protocol::{
    MessageType, Packet, PacketReader, decode_exit, decode_geometry, encode_attach, encode_data,
    encode_detach, encode_peek, encode_resize, encode_status,
};
use serde_json::{Value, json};

/// Skip a test that drives a real daemon over its socket.
///
/// These pass on a machine, in debug and in release, and some of them fail
/// inside a Nix build sandbox: the geometry cases time out waiting for a
/// GEOMETRY frame and a write to the daemon gets a broken pipe. Measured on
/// 2026-08-31 — 6 of 6 outside the sandbox across six runs in both profiles,
/// 3 of 6 inside it.
///
/// `flake.nix` no longer runs this crate's tests at all, so nothing in this
/// repository sets `PTY_SKIP_SOCKET_TESTS`. It stays as the escape hatch for
/// anyone running the suite somewhere that cannot start a daemon reliably —
/// a container, a constrained CI box. A plain `cargo test` runs everything.
#[macro_export]
macro_rules! skip_without_a_real_machine {
    () => {
        if std::env::var_os("PTY_SKIP_SOCKET_TESTS").is_some() {
            eprintln!("skipped: PTY_SKIP_SOCKET_TESTS is set (see daemon_support)");
            return;
        }
    };
}

pub fn pty_bin() -> &'static str {
    env!("CARGO_BIN_EXE_pty")
}

/// One real daemon at a time per test binary: they share CPU and PTY
/// timing, and the ordering cases have real timers.
pub fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// A short registry root (unix socket paths are capped at 104 bytes).
pub fn short_root() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(format!("/tmp/ptyd-{}-{}", std::process::id(), n));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn unique_name(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
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

pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if pred() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub fn wait_dead(pid: i32, timeout: Duration) -> bool {
    wait_until(timeout, || !pid_alive(pid))
}

pub fn read_meta(root: &Path, name: &str) -> Option<Value> {
    let bytes = std::fs::read(root.join(format!("{name}.json"))).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn read_events(root: &Path, name: &str) -> Vec<Value> {
    let Ok(content) = std::fs::read_to_string(root.join(format!("{name}.events.jsonl"))) else {
        return Vec::new();
    };
    content
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("event line"))
        .collect()
}

pub fn events_of_type(root: &Path, name: &str, r#type: &str) -> Vec<Value> {
    read_events(root, name)
        .into_iter()
        .filter(|e| e["type"] == r#type)
        .collect()
}

/// The daemon config Node's spawner would write for `command args`.
pub fn config(name: &str, command: &str, args: &[&str]) -> Value {
    json!({
        "name": name,
        "command": command,
        "args": args,
        "displayCommand": std::iter::once(command).chain(args.iter().copied()).collect::<Vec<_>>().join(" "),
        "cwd": std::env::temp_dir().to_string_lossy(),
        "rows": 24,
        "cols": 80,
    })
}

/// A daemon spawned as a direct child of the test, with its config in
/// `PTY_SERVER_CONFIG` and an isolated `PTY_ROOT`.
pub struct Daemon {
    pub root: PathBuf,
    pub name: String,
    pub pid: i32,
    child: Child,
    exit: Option<i32>,
}

impl Daemon {
    /// Spawn and wait for the socket (5 s).
    pub fn start(root: &Path, config: Value) -> Daemon {
        let d = Daemon::spawn(root, config, &[]);
        assert!(
            wait_until(Duration::from_secs(5), || d.socket_path().exists()),
            "daemon socket never appeared for {}",
            d.name
        );
        d
    }

    /// Spawn with extra environment and wait for the socket.
    pub fn start_env(root: &Path, config: Value, env: &[(&str, &str)]) -> Daemon {
        let d = Daemon::spawn(root, config, env);
        assert!(
            wait_until(Duration::from_secs(5), || d.socket_path().exists()),
            "daemon socket never appeared for {}",
            d.name
        );
        d
    }

    /// Spawn without waiting for anything.
    pub fn spawn(root: &Path, config: Value, env: &[(&str, &str)]) -> Daemon {
        let name = config["name"].as_str().expect("config name").to_string();
        let mut cmd = Command::new(pty_bin());
        cmd.arg("__daemon")
            .env("PTY_ROOT", root)
            .env("PTY_SERVER_CONFIG", config.to_string())
            .env_remove("PTY_SESSION")
            .env_remove("PTY_REAP_ON_EXIT")
            .env_remove("PTY_SPAWNER_PID")
            .env_remove("PTY_SHUTDOWN_DEADLINE_MS")
            .env_remove("PTY_REDRAW_SETTLE_MS")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn daemon");
        // Drain stderr in the background so a chatty daemon never blocks.
        if let Some(mut stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                let mut sink = Vec::new();
                let _ = stderr.read_to_end(&mut sink);
                if !sink.is_empty() {
                    eprintln!("[daemon stderr] {}", String::from_utf8_lossy(&sink));
                }
            });
        }
        let pid = child.id() as i32;
        Daemon {
            root: root.to_path_buf(),
            name,
            pid,
            child,
            exit: None,
        }
    }

    pub fn socket_path(&self) -> PathBuf {
        self.root.join(format!("{}.sock", self.name))
    }

    pub fn connect(&self) -> Conn {
        Conn::connect(&self.root, &self.name)
    }

    pub fn meta(&self) -> Option<Value> {
        read_meta(&self.root, &self.name)
    }

    pub fn events(&self, r#type: &str) -> Vec<Value> {
        events_of_type(&self.root, &self.name, r#type)
    }

    pub fn alive(&mut self) -> bool {
        self.try_exit().is_none()
    }

    pub fn try_exit(&mut self) -> Option<i32> {
        if self.exit.is_none()
            && let Ok(Some(status)) = self.child.try_wait()
        {
            self.exit = Some(status.code().unwrap_or(-1));
        }
        self.exit
    }

    /// Wait for the daemon process to exit; its status.
    pub fn wait_exit(&mut self, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(code) = self.try_exit() {
                return Some(code);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn signal(&self, sig: i32) {
        unsafe {
            libc::kill(self.pid, sig);
        }
    }

    /// The session child's pid, from STATUS.
    pub fn child_pid(&self) -> i32 {
        let mut c = self.connect();
        c.status();
        let st = c.wait_status(Duration::from_secs(3)).expect("status");
        st["process"]["pid"].as_i64().expect("child pid") as i32
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.try_exit().is_none() {
            self.signal(libc::SIGKILL);
            let _ = self.child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A raw protocol client that records every packet it receives.
pub struct Conn {
    stream: UnixStream,
    reader: PacketReader,
    pub packets: Vec<Packet>,
    closed: bool,
}

impl Conn {
    pub fn connect(root: &Path, name: &str) -> Conn {
        let stream = UnixStream::connect(root.join(format!("{name}.sock"))).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_millis(30)))
            .unwrap();
        Conn {
            stream,
            reader: PacketReader::new(),
            packets: Vec::new(),
            closed: false,
        }
    }

    pub fn send(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).expect("write");
    }

    pub fn attach(&mut self, rows: u16, cols: u16) {
        self.send(&encode_attach(rows, cols));
    }

    pub fn peek(&mut self) {
        self.send(&encode_peek(false, false));
    }

    pub fn peek_flags(&mut self, plain: bool, full: bool) {
        self.send(&encode_peek(plain, full));
    }

    pub fn data(&mut self, text: &str) {
        self.send(&encode_data(text.as_bytes()));
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.send(&encode_resize(rows, cols));
    }

    pub fn status(&mut self) {
        self.send(&encode_status());
    }

    pub fn detach(&mut self) {
        self.send(&encode_detach());
    }

    /// Read whatever is available (one short timeout).
    pub fn pump(&mut self) {
        if self.closed {
            return;
        }
        let mut buf = [0u8; 65536];
        match self.stream.read(&mut buf) {
            // A Node socket (allowHalfOpen: false) ends its own side when
            // the peer's FIN arrives; do the same so the daemon sees close.
            Ok(0) => {
                let _ = self.stream.shutdown(std::net::Shutdown::Both);
                self.closed = true;
            }
            Ok(n) => {
                let packets = self.reader.feed(&buf[..n]).expect("feed");
                self.packets.extend(packets);
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => self.closed = true,
        }
    }

    pub fn is_closed(&mut self) -> bool {
        self.pump();
        self.closed
    }

    pub fn wait_closed(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !self.closed && Instant::now() < deadline {
            self.pump();
        }
        self.closed
    }

    /// Pump until `pred` holds or `timeout` passes.
    pub fn wait_for(&mut self, timeout: Duration, mut pred: impl FnMut(&[Packet]) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if pred(&self.packets) {
                return true;
            }
            if Instant::now() >= deadline || self.closed {
                return pred(&self.packets);
            }
            self.pump();
        }
    }

    /// Pump until no packet has arrived for `idle` (the child has finished
    /// redrawing), at most `max`.
    pub fn quiesce(&mut self, idle: Duration, max: Duration) {
        let hard = Instant::now() + max;
        let mut last = Instant::now();
        let mut seen = self.packets.len();
        while Instant::now() < hard {
            self.pump();
            if self.packets.len() != seen {
                seen = self.packets.len();
                last = Instant::now();
            } else if last.elapsed() >= idle {
                return;
            }
        }
    }

    /// Keep pumping for `d` (to prove nothing more arrives).
    pub fn settle(&mut self, d: Duration) {
        let deadline = Instant::now() + d;
        while Instant::now() < deadline {
            self.pump();
        }
    }

    pub fn wait_type(&mut self, t: MessageType, timeout: Duration) -> bool {
        self.wait_for(timeout, |p| p.iter().any(|x| x.type_ == t))
    }

    pub fn wait_count(&mut self, t: MessageType, n: usize, timeout: Duration) -> bool {
        self.wait_for(timeout, |p| p.iter().filter(|x| x.type_ == t).count() >= n)
    }

    /// Wait for a DATA or SCREEN payload containing `text`.
    pub fn wait_text(&mut self, text: &str, timeout: Duration) -> bool {
        self.wait_for(timeout, |p| {
            p.iter().any(|x| {
                matches!(x.type_, MessageType::Data | MessageType::Screen)
                    && String::from_utf8_lossy(&x.payload).contains(text)
            })
        })
    }

    pub fn wait_status(&mut self, timeout: Duration) -> Option<Value> {
        let before = self.count(MessageType::Status);
        if !self.wait_count(MessageType::Status, before + 1, timeout) {
            return None;
        }
        self.last_status()
    }

    /// The most recent STATUS body.
    pub fn last_status(&self) -> Option<Value> {
        self.packets
            .iter()
            .rev()
            .find(|p| p.type_ == MessageType::Status)
            .map(|p| serde_json::from_slice(&p.payload).expect("status json"))
    }

    /// STATUS round trip.
    pub fn query_status(&mut self) -> Value {
        self.status();
        self.wait_status(Duration::from_secs(3)).expect("STATUS reply")
    }

    pub fn types(&self) -> Vec<MessageType> {
        self.packets.iter().map(|p| p.type_).collect()
    }

    pub fn count(&self, t: MessageType) -> usize {
        self.packets.iter().filter(|p| p.type_ == t).count()
    }

    pub fn clear(&mut self) {
        self.packets.clear();
    }

    /// Every SCREEN and DATA payload, concatenated.
    pub fn output(&self) -> String {
        self.packets
            .iter()
            .filter(|p| matches!(p.type_, MessageType::Screen | MessageType::Data))
            .map(|p| String::from_utf8_lossy(&p.payload).into_owned())
            .collect()
    }

    pub fn screen(&self) -> Option<String> {
        self.packets
            .iter()
            .find(|p| p.type_ == MessageType::Screen)
            .map(|p| String::from_utf8_lossy(&p.payload).into_owned())
    }

    pub fn screens(&self) -> Vec<String> {
        self.packets
            .iter()
            .filter(|p| p.type_ == MessageType::Screen)
            .map(|p| String::from_utf8_lossy(&p.payload).into_owned())
            .collect()
    }

    pub fn exit_codes(&self) -> Vec<i32> {
        self.packets
            .iter()
            .filter(|p| p.type_ == MessageType::Exit)
            .map(|p| decode_exit(&p.payload))
            .collect()
    }

    pub fn geometries(&self) -> Vec<(u16, u16)> {
        self.packets
            .iter()
            .filter(|p| p.type_ == MessageType::Geometry)
            .map(|p| decode_geometry(&p.payload))
            .collect()
    }

    /// Index of the first GEOMETRY(rows, cols), if any.
    pub fn geometry_index(&self, rows: u16, cols: u16) -> Option<usize> {
        self.packets.iter().position(|p| {
            p.type_ == MessageType::Geometry && decode_geometry(&p.payload) == (rows, cols)
        })
    }

    /// Indices of every SCREEN and DATA packet.
    pub fn output_indices(&self) -> Vec<usize> {
        self.packets
            .iter()
            .enumerate()
            .filter(|(_, p)| matches!(p.type_, MessageType::Screen | MessageType::Data))
            .map(|(i, _)| i)
            .collect()
    }

    /// node: tests/effective-geometry.test.ts:129-139
    pub fn assert_geometry_before_all_output(&self, rows: u16, cols: u16) {
        let g = self
            .geometry_index(rows, cols)
            .unwrap_or_else(|| panic!("no GEOMETRY({rows},{cols}) in {:?}", self.types()));
        let out = self.output_indices();
        assert!(!out.is_empty(), "no output after GEOMETRY({rows},{cols})");
        assert!(
            out.iter().all(|&i| g < i),
            "output before GEOMETRY({rows},{cols}): {:?}",
            self.types()
        );
    }

    pub fn shutdown(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        self.closed = true;
    }
}

/// Run the built `pty` with `PTY_ROOT` = root; `(stdout, stderr, code)`.
pub fn run_pty(root: &Path, args: &[&str], env: &[(&str, &str)]) -> (String, String, i32) {
    let mut c = Command::new(pty_bin());
    c.args(args)
        .env("PTY_ROOT", root)
        .env_remove("PTY_SESSION")
        .env_remove("PTY_REAP_ON_EXIT")
        .env_remove("PTY_SPAWNER_PID")
        .stdin(Stdio::null());
    for (k, v) in env {
        c.env(k, v);
    }
    let out = c.output().expect("run pty");
    (
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
        out.status.code().unwrap_or(-1),
    )
}

/// The Node `pty` on PATH, when it is the Node one (`0.12.0+<sha>`).
pub fn node_pty() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("pty");
        if candidate == Path::new(pty_bin()) || !candidate.is_file() {
            continue;
        }
        let out = Command::new(&candidate)
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .ok()?;
        let v = String::from_utf8_lossy(&out.stdout);
        if v.trim().starts_with("0.12.0+") {
            return Some(candidate);
        }
    }
    None
}

/// Write an executable shell script into `root`.
pub fn script(root: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = root.join(name);
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}
