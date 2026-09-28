//! Throughput, latency, idle cost, memory and images: a session runtime has
//! to behave like the terminal it stands in for.
//!
//! - A child's writes never wait on a client, however slowly that client
//!   reads, and never wait on input traffic.
//! - Input and control requests are answered promptly whatever else the
//!   session, its neighbours or the host are doing.
//! - An idle session costs nothing, and keeps costing nothing.
//! - What a client receives tracks what the child wrote, whatever the screen
//!   size, the number of clients, or the modes the child turned on.
//! - Memory stays bounded however much the child prints.
//! - A slow or hung filesystem under a session's working directory stalls
//!   nothing.
//! - A child's images reach the client, survive a reattach without being sent
//!   twice, and never show up as text.
//!
//! Every test uses its own `PTY_ROOT` and the `pty` binary built from this
//! tree. Daemons are stopped with SIGKILL on the way out: a graceful `pty
//! kill` is a message to the daemon, and some of these tests leave it with a
//! long queue of output to get through first.

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use pty_core::protocol::{self, MessageType, Packet, PacketReader};
use pty_terminal::{GraphicsOptions, TerminalActor};
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

// ── A registry of this test's own ──────────────────────────────────────────

struct Registry {
    root: PathBuf,
    daemons: Vec<(String, u32)>,
    helpers: Vec<Child>,
}

impl Registry {
    fn new() -> Registry {
        use_local_pty();
        // Short: a session socket path has to fit 104 bytes.
        let root = std::env::temp_dir().join(format!("pp-{}", pty_testkit::server::random_id()));
        std::fs::create_dir_all(&root).expect("registry dir");
        Registry {
            root,
            daemons: Vec::new(),
            helpers: Vec::new(),
        }
    }

    fn pty(&self) -> Command {
        let mut cmd = Command::new(pty_testkit::server::pty_bin());
        cmd.env("PTY_ROOT", &self.root);
        for key in [
            "PTY_SESSION",
            "PTY_SESSION_GENERATION",
            "PTY_SESSION_DIR",
            "PTY_REAP_ON_EXIT",
        ] {
            cmd.env_remove(key);
        }
        cmd.stdin(Stdio::null());
        cmd
    }

    /// `pty run -d` a shell script, at a size, optionally in a directory and
    /// with extra `run` flags.
    fn start_with(
        &mut self,
        name: &str,
        rows: u16,
        cols: u16,
        cwd: Option<&Path>,
        flags: &[&str],
        script: &str,
    ) {
        let (rows, cols) = (rows.to_string(), cols.to_string());
        let mut cmd = self.pty();
        cmd.args([
            "run",
            "-d",
            "-e",
            "--no-display-name",
            "--id",
            name,
            "--rows",
            &rows,
            "--cols",
            &cols,
        ]);
        if let Some(cwd) = cwd {
            cmd.arg("--cwd").arg(cwd);
        }
        cmd.args(flags).args(["--", "sh", "-c", script]);
        let out = cmd.output().expect("pty run");
        assert!(
            out.status.success(),
            "pty run {name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let pid = std::fs::read_to_string(self.root.join(format!("{name}.pid")))
            .expect("the daemon's pid file")
            .trim()
            .parse()
            .expect("a pid");
        self.daemons.push((name.to_string(), pid));
    }

    fn start(&mut self, name: &str, script: &str) {
        self.start_with(name, 24, 80, None, &[], script);
    }

    fn daemon(&self, name: &str) -> u32 {
        self.daemons
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, pid)| *pid)
            .expect("a session this registry started")
    }

    fn path(&self, file: &str) -> PathBuf {
        self.root.join(file)
    }

    /// Three hundred extra processes on the host for the rest of the test.
    fn crowd_the_host(&mut self) {
        use std::os::unix::process::CommandExt;
        let helper = Command::new("sh")
            .args([
                "-c",
                "i=0; while [ $i -lt 300 ]; do sleep 60 & i=$((i+1)); done; wait",
            ])
            .process_group(0)
            .spawn()
            .expect("host processes");
        self.helpers.push(helper);
    }

    /// Write `bytes` into the registry directory and return its path.
    fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.path(name);
        std::fs::write(&path, bytes).expect("fixture file");
        path
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        for helper in &mut self.helpers {
            let group = format!("-{}", helper.id());
            let _ = Command::new("kill")
                .args(["-9", "--", &group])
                .stderr(Stdio::null())
                .status();
            let _ = helper.wait();
        }
        for (_, pid) in &self.daemons {
            // The child's terminal hangs up with its daemon.
            let _ = Command::new("kill")
                .args(["-9", &pid.to_string()])
                .stderr(Stdio::null())
                .status();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Run a command to completion, or give up on it at `limit` and kill it.
fn run_within(mut cmd: Command, limit: Duration) -> Option<(Output, Duration)> {
    let start = Instant::now();
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            let took = start.elapsed();
            return child.wait_with_output().ok().map(|out| (out, took));
        }
        if start.elapsed() >= limit {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) -> Duration {
    let start = Instant::now();
    while !done() {
        assert!(
            start.elapsed() < limit,
            "timed out after {limit:?} waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    start.elapsed()
}

// ── A client that speaks the socket protocol directly ──────────────────────

fn dial(root: &Path, name: &str) -> UnixStream {
    let path = root.join(format!("{name}.sock"));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match UnixStream::connect(&path) {
            Ok(sock) => return sock,
            Err(e) => {
                assert!(Instant::now() < deadline, "connect {}: {e}", path.display());
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Every packet is stamped the moment its bytes come off the socket.
struct Client {
    sock: UnixStream,
    rx: Receiver<(Instant, Packet)>,
}

impl Client {
    fn attach(reg: &Registry, name: &str, rows: u16, cols: u16) -> Client {
        let mut sock = dial(&reg.root, name);
        sock.write_all(&protocol::encode_attach(rows, cols))
            .expect("ATTACH");
        Client::reading(sock)
    }

    /// A connection that never attaches: what `pty send` and `pty stats` use.
    fn command(reg: &Registry, name: &str) -> Client {
        Client::reading(dial(&reg.root, name))
    }

    fn reading(sock: UnixStream) -> Client {
        let (tx, rx) = mpsc::channel();
        let mut reader = sock.try_clone().expect("clone socket");
        std::thread::spawn(move || {
            let mut parser = PacketReader::new();
            let mut buf = vec![0u8; 1 << 16];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                let at = Instant::now();
                let Ok(packets) = parser.feed(&buf[..n]) else {
                    return;
                };
                for packet in packets {
                    if tx.send((at, packet)).is_err() {
                        return;
                    }
                }
            }
        });
        Client { sock, rx }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.sock
            .write_all(&protocol::encode_data(bytes))
            .expect("DATA");
    }

    fn frame(&mut self, frame: &[u8]) {
        self.sock.write_all(frame).expect("frame");
    }

    fn next(&self, until: Instant) -> Option<(Instant, Packet)> {
        let left = until.saturating_duration_since(Instant::now());
        match self.rx.recv_timeout(left) {
            Ok(item) => Some(item),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => None,
        }
    }

    /// Throw away whatever has arrived so far.
    fn drain(&self) {
        while self.rx.try_recv().is_ok() {}
    }

    /// The first SCREEN.
    fn screen(&self) -> Vec<u8> {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let (_, p) = self.next(until).expect("a SCREEN within 10 s");
            if p.type_ == MessageType::Screen {
                return p.payload;
            }
        }
    }

    /// DATA bytes until `done` holds for everything received so far.
    fn data_until(
        &self,
        what: &str,
        limit: Duration,
        done: impl Fn(&[u8]) -> bool,
    ) -> (Vec<u8>, Instant) {
        let until = Instant::now() + limit;
        let mut bytes = Vec::new();
        loop {
            let Some((at, p)) = self.next(until) else {
                panic!(
                    "timed out waiting for {what}; got {} bytes: {:?}",
                    bytes.len(),
                    String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(300)..])
                );
            };
            if p.type_ == MessageType::Data {
                bytes.extend_from_slice(&p.payload);
                if done(&bytes) {
                    return (bytes, at);
                }
            }
        }
    }

    /// One STATUS round trip on this connection; the reply's arrival time.
    fn status(&mut self, limit: Duration) -> Option<Duration> {
        let sent = Instant::now();
        self.frame(&protocol::encode_status());
        let until = sent + limit;
        loop {
            let (at, p) = self.next(until)?;
            if p.type_ == MessageType::Status {
                return Some(at - sent);
            }
        }
    }
}

/// Send one byte to a raw-mode `cat` and time its echo.
fn echo_round_trip(c: &mut Client, byte: u8) -> Duration {
    c.drain();
    let sent = Instant::now();
    c.send(&[byte]);
    let until = sent + Duration::from_secs(20);
    loop {
        let (at, p) = c.next(until).expect("the echo within 20 s");
        if p.type_ == MessageType::Data && p.payload.contains(&byte) {
            return at - sent;
        }
    }
}

fn echo_samples(c: &mut Client, n: usize) -> Vec<Duration> {
    (0..n)
        .map(|i| {
            let d = echo_round_trip(c, b'a' + (i % 26) as u8);
            std::thread::sleep(Duration::from_millis(5));
            d
        })
        .collect()
}

fn percentile(samples: &[Duration], p: f64) -> Duration {
    let mut sorted = samples.to_vec();
    sorted.sort();
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i]
}

// ── What a process costs ────────────────────────────────────────────────────

/// Time on CPU, all threads, nanosecond precision.
fn cpu_time(pid: u32) -> Duration {
    let mut ns = 0u64;
    for task in std::fs::read_dir(format!("/proc/{pid}/task")).expect("/proc/<pid>/task") {
        let path = task.expect("task").path().join("schedstat");
        if let Ok(text) = std::fs::read_to_string(path) {
            ns += text
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        }
    }
    Duration::from_nanos(ns)
}

/// Read and write system calls made so far, all threads.
fn syscalls(pid: u32) -> u64 {
    let text = std::fs::read_to_string(format!("/proc/{pid}/io")).expect("/proc/<pid>/io");
    text.lines()
        .filter(|l| l.starts_with("syscr:") || l.starts_with("syscw:"))
        .map(|l| {
            l.split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0)
        })
        .sum()
}

fn rss_mib(pid: u32) -> f64 {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).expect("/proc/<pid>/status");
    let kb: f64 = text
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .expect("VmRSS");
    kb / 1024.0
}

// ── Output fixtures ─────────────────────────────────────────────────────────

/// Full-screen frames with a truecolor foreground and background on every
/// cell: what an animated TUI draws. About 70 KiB a frame.
fn truecolor_frames(frames: usize) -> Vec<u8> {
    let mut seed: u32 = 7;
    let mut next = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 24) as u8
    };
    let mut out = String::new();
    for _ in 0..frames {
        out.push_str("\x1b[H");
        for row in 0..24 {
            for _ in 0..80 {
                let glyph = char::from_u32(0x2580 + (next() % 16) as u32).expect("block glyph");
                let _ = write!(
                    out,
                    "\x1b[38;2;{};{};{}m\x1b[48;2;{};{};{}m{glyph}",
                    next(),
                    next(),
                    next(),
                    next(),
                    next(),
                    next()
                );
            }
            if row < 23 {
                out.push_str("\r\n");
            }
        }
    }
    out.push_str("\x1b[0m");
    out.into_bytes()
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A kitty transmission of raw RGBA pixels, chunked the way clients chunk
/// (4096 base64 bytes per APC), with the given leading keys.
fn kitty_rgba(keys: &str, width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let payload = base64(rgba);
    let chunks: Vec<&str> = payload
        .as_bytes()
        .chunks(4096)
        .map(|c| std::str::from_utf8(c).expect("ascii"))
        .collect();
    let mut out = String::new();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        if i == 0 {
            let _ = write!(
                out,
                "\x1b_G{keys},f=32,s={width},v={height},m={more};{chunk}\x1b\\"
            );
        } else {
            let _ = write!(out, "\x1b_Gm={more};{chunk}\x1b\\");
        }
    }
    out.into_bytes()
}

/// A 1x1 red PNG, the protocol documentation's own example.
const RED_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

/// The Unicode placeholder cell for image row 0, column 0 of image `id`
/// (id in the truecolor foreground).
fn placeholder_cell(id: u32) -> String {
    format!(
        "\x1b[38;2;{};{};{}m\u{10eeee}\u{305}\u{305}\x1b[39m",
        (id >> 16) & 255,
        (id >> 8) & 255,
        id & 255
    )
}

/// Serve `bytes` to the child from a file, so the script needs no quoting.
fn cat_script(reg: &Registry, name: &str, bytes: &[u8]) -> String {
    format!("cat '{}'", reg.file(name, bytes).display())
}

fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    count(haystack, needle) > 0
}

/// A client-side terminal with graphics on, filled from one replay.
fn replay_into_terminal(screen: &[u8]) -> TerminalActor {
    let mut actor = TerminalActor::new(24, 80, 1000);
    assert!(
        actor.enable_graphics(GraphicsOptions::DEFAULT),
        "graphics on"
    );
    actor.write(screen);
    actor
}

// ═══ The child's writes never wait on a client or on input ═════════════════

/// A client that attaches and then never reads again is the slowest renderer
/// there is. The child's writes must not wait for it: a terminal multiplexer
/// that applied its clients' back-pressure to the child would freeze every
/// program in the session behind one stuck client.
#[test]
fn a_client_that_stops_reading_never_slows_the_childs_writes() {
    let mut reg = Registry::new();
    let done = reg.path("done");
    reg.start(
        "w",
        &format!(
            "stty raw -echo; head -c1 >/dev/null; head -c 4000000 /dev/zero | tr '\\0' x; : > '{}'; exec cat",
            done.display()
        ),
    );
    let mut trigger = Client::attach(&reg, "w", 24, 80);
    trigger.screen();
    // Attached, and never read from again.
    let mut stalled = dial(&reg.root, "w");
    stalled
        .write_all(&protocol::encode_attach(24, 80))
        .expect("ATTACH");

    trigger.send(b"g");
    let took = wait_until(
        "the child to finish writing 4 MB",
        Duration::from_secs(20),
        || done.exists(),
    );
    eprintln!("4 MB written past a client that does not read in {took:?}");
    assert!(
        took < Duration::from_secs(3),
        "the child needed {took:?} to write 4 MB with a client that does not read"
    );
    drop(stalled);
}

/// Input traffic must not hold up the child's output either: a mouse drag
/// reported at full rate, and DATA frames with nothing in them (what an
/// encoder produces for an event it cannot encode). An empty frame must cost
/// nothing and leave the session working.
#[test]
fn input_floods_and_empty_input_frames_never_slow_the_childs_writes() {
    let mut reg = Registry::new();
    let done = reg.path("done");
    reg.start(
        "m",
        &format!(
            "stty raw -echo; printf '\\033[?1003h\\033[?1006hREADY'; head -c1; head -c1 >/dev/null; \
             (head -c 2000000 /dev/zero | tr '\\0' y; : > '{}') & exec cat",
            done.display()
        ),
    );
    let daemon = reg.daemon("m");
    let mut c = Client::attach(&reg, "m", 24, 80);
    let screen = c.screen();
    if !contains(&screen, b"READY") {
        c.data_until("READY after attach", Duration::from_secs(10), |b| {
            contains(b, b"READY")
        });
    }

    // Ten thousand empty frames to an idle session: nothing to write, so
    // nothing to do, and nothing left spinning afterwards.
    let mut empties = Vec::new();
    for _ in 0..10_000 {
        empties.extend_from_slice(&protocol::encode_data(b""));
    }
    c.frame(&empties);
    let echo = echo_round_trip(&mut c, b'e');
    std::thread::sleep(Duration::from_millis(200));
    let before = cpu_time(daemon);
    std::thread::sleep(Duration::from_millis(1000));
    let spent = cpu_time(daemon) - before;
    assert!(
        spent < Duration::from_millis(20),
        "the daemon kept working after empty input frames: {spent:?} of CPU in the next second"
    );
    assert!(
        echo < Duration::from_secs(1),
        "input after empty frames took {echo:?}"
    );

    // The child floods its output while the client floods motion reports.
    let mut motion = Vec::new();
    for i in 0..20_000u32 {
        let report = format!("\x1b[<35;{};{}M", 1 + i % 80, 1 + (i / 80) % 24);
        motion.extend_from_slice(&protocol::encode_data(report.as_bytes()));
        motion.extend_from_slice(&protocol::encode_data(b""));
    }
    let mut writer = c.sock.try_clone().expect("clone");
    c.send(b"g");
    let start = Instant::now();
    let flood = std::thread::spawn(move || {
        let _ = writer.write_all(&motion);
    });
    let took = wait_until(
        "the child to finish writing 2 MB",
        Duration::from_secs(20),
        || done.exists(),
    );
    eprintln!("2 MB written during an input flood in {took:?}");
    let _ = flood.join();
    assert!(
        took < Duration::from_secs(3),
        "the child needed {took:?} (from {start:?}) to write 2 MB during an input flood"
    );
}

// ═══ Input latency does not grow with other work ═══════════════════════════

/// Echo through the whole runtime (client → daemon → child → daemon →
/// client) for a quiet session, then again while two other sessions flood
/// their clients and the host carries three hundred extra processes. The
/// quiet session's keystrokes must not queue behind anybody else's output or
/// any per-process scan.
#[test]
fn keystroke_echo_stays_fast_while_other_sessions_flood_and_the_host_is_busy() {
    let mut reg = Registry::new();
    reg.start("q", "stty raw -echo; exec cat");
    let mut q = Client::attach(&reg, "q", 24, 80);
    q.screen();
    let quiet = echo_samples(&mut q, 40);

    // Bounded, so a daemon that parses slower than its child prints (a debug
    // build does) holds a bounded backlog; each still keeps its daemon busy
    // for the whole measurement.
    let frames = cat_script(&reg, "frames", &truecolor_frames(8));
    reg.start(
        "f1",
        &format!("i=0; while [ $i -lt 12 ]; do {frames}; i=$((i+1)); done; exec cat"),
    );
    reg.start(
        "f2",
        "yes 'the quick brown fox jumps over the lazy dog' | head -c 3000000; exec cat",
    );
    let f1 = Client::attach(&reg, "f1", 24, 80);
    let f2 = Client::attach(&reg, "f2", 24, 80);
    let drainers: Vec<_> = [f1, f2]
        .into_iter()
        .map(|c| {
            std::thread::spawn(
                move || while c.next(Instant::now() + Duration::from_secs(30)).is_some() {},
            )
        })
        .collect();
    reg.crowd_the_host();
    std::thread::sleep(Duration::from_millis(500));

    let busy = echo_samples(&mut q, 40);
    let (q50, q95) = (percentile(&quiet, 0.5), percentile(&quiet, 0.95));
    let (b50, b95) = (percentile(&busy, 0.5), percentile(&busy, 0.95));
    eprintln!("echo quiet p50 {q50:?} p95 {q95:?}; busy p50 {b50:?} p95 {b95:?}");
    assert!(
        b95 < Duration::from_millis(100) && b50 < (q50 * 5).max(Duration::from_millis(20)),
        "echo latency grew with other sessions' output: quiet p50 {q50:?} p95 {q95:?}, busy p50 {b50:?} p95 {b95:?}"
    );
    drop(drainers);
}

/// A keystroke (think Ctrl-C) has to reach the child while the child is
/// still printing. The daemon may be behind on drawing that output; it must
/// not make the keystroke wait until it has drawn all of it.
///
/// The child floods truecolor full-screen frames in the background and, in
/// the foreground, waits for one byte of input; the test times how long the
/// byte takes to reach it.
#[test]
#[ignore = "fails on main: a keystroke waits behind all of the session's unparsed output"]
fn a_keystroke_reaches_the_child_promptly_while_its_own_output_floods() {
    let mut reg = Registry::new();
    let got = reg.path("got");
    let frames = cat_script(&reg, "frames", &truecolor_frames(8));
    reg.start(
        "s",
        &format!(
            "stty raw -echo; head -c1 >/dev/null; (i=0; while [ $i -lt 6 ]; do {frames}; i=$((i+1)); done) & \
             head -c1 >/dev/null; : > '{}'; wait; exec cat",
            got.display()
        ),
    );
    let mut c = Client::attach(&reg, "s", 24, 80);
    c.screen();
    c.send(b"g");
    std::thread::sleep(Duration::from_millis(400));
    c.send(b"k");
    let took = wait_until(
        "the keystroke to reach the child",
        Duration::from_secs(120),
        || got.exists(),
    );
    eprintln!("the keystroke reached the child after {took:?}");
    assert!(
        took < Duration::from_secs(1),
        "the keystroke reached the child {took:?} after it was sent"
    );
}

/// The detach key ends the client after the double-tap window and nothing
/// else: no tail that depends on what the session or the host is doing.
#[test]
fn detaching_takes_the_double_tap_window_and_no_more() {
    let mut reg = Registry::new();
    reg.start("d", "printf 'SESSION-UP\\n'; exec cat");
    let mut times = Vec::new();
    for _ in 0..6 {
        let mut outer = Session::spawn(
            &pty_testkit::server::pty_bin(),
            &["attach", "d"],
            SpawnOptions {
                rows: Some(24),
                cols: Some(80),
                env: vec![("PTY_ROOT".into(), reg.root.display().to_string())],
                ..Default::default()
            },
        )
        .expect("pty attach");
        outer.wait_for_text("SESSION-UP", 10_000).expect("attached");
        let pressed = Instant::now();
        outer.send_keys("\x1c");
        wait_until("the attach client to exit", Duration::from_secs(10), || {
            outer.has_exited()
        });
        times.push(pressed.elapsed());
        outer.close();
    }
    let (fast, slow) = (percentile(&times, 0.0), percentile(&times, 1.0));
    eprintln!("detach times: {times:?}");
    assert!(
        slow < Duration::from_millis(600) && slow - fast < Duration::from_millis(200),
        "detach took {fast:?} to {slow:?}: {times:?}"
    );
}

// ═══ An idle session costs nothing ═════════════════════════════════════════

/// Sixteen idle sessions, some with a client attached, on a host carrying
/// three hundred extra processes: their daemons spend no CPU and make no
/// read or write system calls over three seconds — and again three seconds
/// later, so nothing accumulates with uptime. A burst of mouse motion leaves
/// no work behind once it stops.
#[test]
fn idle_sessions_cost_no_cpu_and_make_no_system_calls() {
    let mut reg = Registry::new();
    let names: Vec<String> = (0..16).map(|i| format!("i{i}")).collect();
    for (i, name) in names.iter().enumerate() {
        let script = if i % 2 == 0 {
            "exec bash --norc --noprofile -i"
        } else {
            "stty raw -echo; exec cat"
        };
        reg.start(name, script);
    }
    let mut attached: Vec<Client> = names[..4]
        .iter()
        .map(|n| Client::attach(&reg, n, 24, 80))
        .collect();
    for c in &attached {
        c.screen();
    }
    reg.crowd_the_host();
    // Past the one-second activity write that follows a session's first output.
    std::thread::sleep(Duration::from_millis(2000));

    let pids: Vec<u32> = names.iter().map(|n| reg.daemon(n)).collect();
    let window = |label: &str| {
        let before: Vec<(Duration, u64)> =
            pids.iter().map(|&p| (cpu_time(p), syscalls(p))).collect();
        std::thread::sleep(Duration::from_secs(3));
        let mut total = Duration::ZERO;
        for (i, &pid) in pids.iter().enumerate() {
            let cpu = cpu_time(pid) - before[i].0;
            let calls = syscalls(pid) - before[i].1;
            total += cpu;
            assert!(
                cpu < Duration::from_millis(5) && calls == 0,
                "{label}: idle daemon {} spent {cpu:?} of CPU and made {calls} read/write calls in 3 s",
                names[i]
            );
        }
        eprintln!("{label}: 16 idle daemons spent {total:?} of CPU in 3 s");
        total
    };
    window("first window");
    window("second window");

    // Motion over an idle session: the reports go to the child and that is
    // all. Once they stop, so does the work.
    let cat = &mut attached[1];
    for i in 0..500u32 {
        cat.send(format!("\x1b[<35;{};{}M", 1 + i % 80, 1 + (i / 80) % 24).as_bytes());
    }
    let _ = cat.data_until("the motion reports echoed", Duration::from_secs(20), |b| {
        count(b, b"M") >= 500
    });
    // Past the one debounced activity write the echoed reports schedule.
    std::thread::sleep(Duration::from_millis(1500));
    window("after mouse motion");
}

// ═══ Client traffic and CPU follow the child's bytes ═══════════════════════

/// One DATA stream as a client sees it between two markers.
fn spinner_stream(c: &Client) -> Vec<u8> {
    let (bytes, _) = c.data_until("the spinner's END", Duration::from_secs(60), |b| {
        contains(b, b"END")
    });
    bytes
}

/// A spinner that redraws one cell: `\r` and a glyph, 1500 times. Every
/// client must receive exactly the child's bytes — no full-screen redraw,
/// no per-client or per-size amplification, no cost from the keyboard modes
/// the child enabled — and the daemon's CPU for the same bytes must not
/// depend on the screen size or those modes.
#[test]
fn a_one_cell_spinner_costs_every_client_exactly_the_childs_bytes() {
    let mut reg = Registry::new();
    let spin = "i=0; while [ $i -lt 1500 ]; do case $((i % 4)) in 0) c='|';; 1) c='/';; 2) c='-';; *) c='+';; esac; \
                printf '\\r%s' \"$c\"; i=$((i+1)); done; printf '\\r\\nEND\\r\\n'";
    let mut expected = Vec::new();
    for i in 0..1500 {
        expected.push(b'\r');
        expected.push(b"|/-+"[i % 4]);
    }
    expected.extend_from_slice(b"\r\nEND\r\n");

    struct Case {
        name: &'static str,
        rows: u16,
        cols: u16,
        clients: usize,
        modes: &'static str,
    }
    let cases = [
        Case {
            name: "small",
            rows: 24,
            cols: 80,
            clients: 1,
            modes: "",
        },
        Case {
            name: "large",
            rows: 60,
            cols: 200,
            clients: 1,
            modes: "",
        },
        Case {
            name: "three",
            rows: 24,
            cols: 80,
            clients: 3,
            modes: "",
        },
        Case {
            name: "modes",
            rows: 24,
            cols: 80,
            clients: 1,
            modes: "\\033[>2u\\033[>4;2m",
        },
    ];
    let mut cpu = Vec::new();
    for case in &cases {
        reg.start_with(
            case.name,
            case.rows,
            case.cols,
            None,
            &[],
            &format!(
                "stty raw -echo; printf '{}READY'; head -c1 >/dev/null; {spin}; exec cat",
                case.modes
            ),
        );
        let mut clients: Vec<Client> = (0..case.clients)
            .map(|_| Client::attach(&reg, case.name, case.rows, case.cols))
            .collect();
        for c in &clients {
            let screen = c.screen();
            if !contains(&screen, b"READY") {
                c.data_until("READY after attach", Duration::from_secs(10), |b| {
                    contains(b, b"READY")
                });
            }
        }
        std::thread::sleep(Duration::from_millis(200));
        for c in &clients {
            c.drain();
        }
        let daemon = reg.daemon(case.name);
        let before = cpu_time(daemon);
        clients[0].send(b"g");
        for (i, c) in clients.iter().enumerate() {
            let got = spinner_stream(c);
            assert_eq!(
                got.len(),
                expected.len(),
                "{} client {i}: received {} bytes for the child's {}",
                case.name,
                got.len(),
                expected.len()
            );
            assert!(
                got == expected,
                "{} client {i}: the bytes differ from the child's",
                case.name
            );
        }
        cpu.push((case.name, cpu_time(daemon) - before));
    }
    eprintln!("daemon CPU per case: {cpu:?}");
    let base = cpu[0].1;
    for (name, spent) in &cpu[1..] {
        let bound = if *name == "three" { base * 3 } else { base * 2 };
        assert!(
            *spent < bound + Duration::from_millis(30),
            "{name}: the daemon spent {spent:?} on the same 3000 bytes the small case spent {base:?} on"
        );
    }
}

// ═══ Control queries scale and answer promptly ═════════════════════════════

/// `pty list --json` over two dozen sessions stays well under a second and
/// grows roughly linearly; a STATUS written a millisecond after connecting is
/// answered in milliseconds; and answering a stream of STATUS requests does
/// not slow the session's own input and output.
#[test]
#[ignore = "requires an isolated performance host: tail latency varies under shared load"]
fn listing_stays_fast_and_status_answers_within_milliseconds() {
    let mut reg = Registry::new();
    let list_time = |reg: &Registry, expect: usize| {
        (0..3)
            .map(|_| {
                let mut cmd = reg.pty();
                cmd.args(["list", "--json"]);
                let (out, took) =
                    run_within(cmd, Duration::from_secs(20)).expect("pty list finished");
                let json = String::from_utf8_lossy(&out.stdout);
                assert_eq!(json.matches("\"running\"").count(), expect, "{json}");
                took
            })
            .min()
            .expect("three runs")
    };
    for i in 0..4 {
        reg.start(&format!("l{i}"), "stty raw -echo; exec cat");
    }
    let four = list_time(&reg, 4);
    for i in 4..24 {
        reg.start(&format!("l{i}"), "stty raw -echo; exec cat");
    }
    let twenty_four = list_time(&reg, 24);
    eprintln!("pty list --json: 4 sessions {four:?}, 24 sessions {twenty_four:?}");
    assert!(
        twenty_four < Duration::from_secs(1)
            && twenty_four < four * 12 + Duration::from_millis(100),
        "listing 24 sessions took {twenty_four:?} (4 took {four:?})"
    );

    // A request that arrives a millisecond after the connection does.
    let mut replies = Vec::new();
    for _ in 0..50 {
        let mut sock = dial(&reg.root, "l0");
        std::thread::sleep(Duration::from_millis(1));
        let sent = Instant::now();
        sock.write_all(&protocol::encode_status()).expect("STATUS");
        sock.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let packet = protocol::read_packet(&mut sock)
            .expect("read")
            .expect("a reply");
        assert_eq!(packet.type_, MessageType::Status);
        replies.push(sent.elapsed());
    }
    let (p50, p95) = (percentile(&replies, 0.5), percentile(&replies, 0.95));
    eprintln!("STATUS 1 ms after connect: p50 {p50:?} p95 {p95:?}");
    assert!(
        p95 < Duration::from_millis(50),
        "STATUS replies: p50 {p50:?} p95 {p95:?}"
    );

    // STATUS requests hammering the session while it echoes.
    let mut session = Client::attach(&reg, "l1", 24, 80);
    session.screen();
    let quiet = echo_samples(&mut session, 20);
    let mut hammer = Client::command(&reg, "l1");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hammering = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut n = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                hammer.status(Duration::from_secs(5)).expect("STATUS reply");
                n += 1;
            }
            n
        })
    };
    let loaded = echo_samples(&mut session, 20);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let answered = hammering.join().expect("hammer");
    let (q95, l95) = (percentile(&quiet, 0.95), percentile(&loaded, 0.95));
    eprintln!("echo p95 quiet {q95:?}, during {answered} STATUS requests {l95:?}");
    assert!(
        l95 < Duration::from_millis(50),
        "echo p95 went from {q95:?} to {l95:?} while {answered} STATUS requests were answered"
    );
}

/// A STATUS request to a session whose child is printing must be answered
/// promptly. The daemon may be behind on drawing the output; the question
/// "what is this session" must not wait for it to catch up.
#[test]
#[ignore = "fails on main: STATUS waits behind all of the session's unparsed output"]
fn status_answers_promptly_while_the_child_floods_output() {
    let mut reg = Registry::new();
    let frames = cat_script(&reg, "frames", &truecolor_frames(8));
    reg.start(
        "s",
        &format!("stty raw -echo; head -c1 >/dev/null; i=0; while [ $i -lt 6 ]; do {frames}; i=$((i+1)); done; exec cat"),
    );
    let mut c = Client::attach(&reg, "s", 24, 80);
    c.screen();
    c.send(b"g");
    std::thread::sleep(Duration::from_millis(400));
    let mut command = Client::command(&reg, "s");
    let took = command
        .status(Duration::from_secs(120))
        .expect("a STATUS reply at all");
    eprintln!("STATUS answered after {took:?}");
    assert!(
        took < Duration::from_secs(1),
        "STATUS was answered after {took:?}"
    );
}

// ═══ Memory stays bounded ══════════════════════════════════════════════════

/// Three bursts of 8 MB of sixel data (a DCS string the daemon's terminal
/// does not draw, so it parses quickly even in a debug build), each after
/// the daemon has caught up with the last, with one client reading and,
/// when `stalled`, a second one attached that never reads. Returns how many
/// MiB the daemon's resident memory grew between the end of the first burst
/// and the end of the third: a daemon whose memory is bounded reuses what
/// the first burst needed.
fn memory_growth_over_bursts(stalled: bool) -> f64 {
    let mut reg = Registry::new();
    reg.start(
        "b",
        "stty raw -echo; while :; do head -c1 >/dev/null; printf '\\033Pq'; \
         head -c 8000000 /dev/zero | tr '\\0' '?'; printf '\\033\\\\#'; done",
    );
    let daemon = reg.daemon("b");
    let mut driver = Client::attach(&reg, "b", 24, 80);
    driver.screen();
    let _stalled = stalled.then(|| {
        let mut sock = dial(&reg.root, "b");
        sock.write_all(&protocol::encode_attach(24, 80))
            .expect("ATTACH");
        sock
    });
    std::thread::sleep(Duration::from_millis(300));
    let mut command = Client::command(&reg, "b");
    let mut after = Vec::new();
    for burst in 0..3 {
        driver.send(b"g");
        // Wait for the burst's last byte on the reading client, then for a
        // STATUS round trip: the daemon has handled everything before it.
        driver.data_until("the end of a burst", Duration::from_secs(120), |b| {
            b.ends_with(b"#")
        });
        command.status(Duration::from_secs(120)).expect("STATUS");
        std::thread::sleep(Duration::from_millis(300));
        after.push(rss_mib(daemon));
        eprintln!(
            "stalled={stalled} burst {burst}: daemon RSS {:.1} MiB",
            after[burst]
        );
        driver.drain();
    }
    after[2] - after[0]
}

/// With every client reading, the daemon's memory reaches a plateau: a burst
/// of output passes through and the next one reuses the same memory.
#[test]
fn repeated_bursts_to_reading_clients_leave_memory_flat() {
    let grew = memory_growth_over_bursts(false);
    assert!(
        grew < 6.0,
        "the daemon's RSS grew {grew:.1} MiB over two more 8 MB bursts"
    );
}

/// A client that stops reading (a suspended terminal, a stalled network
/// link) must not make the daemon hold every byte addressed to it. The
/// daemon's memory has to stay bounded — by dropping or resynchronising the
/// client — rather than grow with everything the child prints.
#[test]
#[ignore = "fails on main: the daemon queues all output for a client that stopped reading, without bound"]
fn a_client_that_stops_reading_does_not_grow_daemon_memory_without_bound() {
    let grew = memory_growth_over_bursts(true);
    assert!(
        grew < 6.0,
        "the daemon's RSS grew {grew:.1} MiB over two more 8 MB bursts"
    );
}

// ═══ Kitty graphics reach the client and survive a reattach ════════════════

/// The child's graphics bytes reach an attached client exactly as written —
/// a direct-medium PNG with a virtual placement, a file-medium transmission,
/// and a same-id retransmission — so a client whose terminal draws images
/// draws them.
#[test]
fn kitty_graphics_reach_a_live_client_byte_for_byte() {
    let mut reg = Registry::new();
    let pixels = reg.file("pixels.rgba", &[0, 0, 255, 255]);
    let direct = format!(
        "\x1b_Ga=T,f=100,i=21,q=2,U=1,c=1,r=1;{RED_PNG}\x1b\\{}",
        placeholder_cell(21)
    );
    let by_file = format!(
        "\x1b_Ga=T,f=32,t=f,i=22,s=1,v=1,U=1,c=1,r=1,q=2;{}\x1b\\{}",
        base64(pixels.display().to_string().as_bytes()),
        placeholder_cell(22)
    );
    let again = kitty_rgba("a=t,i=21,q=2", 1, 1, &[0, 255, 0, 255]);
    let mut bytes = Vec::new();
    bytes.extend_from_slice(direct.as_bytes());
    bytes.extend_from_slice(by_file.as_bytes());
    bytes.extend_from_slice(&again);
    bytes.extend_from_slice(b"DRAWN");
    let script = cat_script(&reg, "draw", &bytes);
    reg.start(
        "k",
        &format!("stty raw -echo; head -c1 >/dev/null; {script}; exec cat"),
    );
    let mut c = Client::attach(&reg, "k", 24, 80);
    c.screen();
    c.send(b"g");
    let (got, _) = c.data_until("the drawing", Duration::from_secs(30), |b| {
        contains(b, b"DRAWN")
    });
    for (what, seq) in [
        ("direct PNG", direct.as_bytes()),
        ("file medium", by_file.as_bytes()),
        ("retransmission", &again[..]),
    ] {
        assert!(
            contains(&got, seq),
            "the {what} did not reach the client intact"
        );
    }
}

/// A reattach carries each image once, however many times the client
/// reattaches; and nothing after the image — text, focus reports, a resize —
/// makes the runtime send it again.
#[test]
fn a_reattach_replays_each_image_once_and_nothing_resends_it() {
    let mut reg = Registry::new();
    let draw = format!(
        "\x1b_Ga=T,f=100,i=31,q=2,U=1,c=1,r=1;{RED_PNG}\x1b\\{}\r\nIMAGE-UP",
        placeholder_cell(31)
    );
    let script = cat_script(&reg, "draw", draw.as_bytes());
    reg.start(
        "r",
        &format!(
            "stty raw -echo; printf '\\033[?1004h'; head -c1 >/dev/null; {script}; \
             head -c1 >/dev/null; i=0; while [ $i -lt 50 ]; do printf '\\r\\ntick %s' $i; i=$((i+1)); done; \
             printf '\\r\\nTICKS-DONE'; exec cat >/dev/null"
        ),
    );
    let mut live = Client::attach(&reg, "r", 24, 80);
    live.screen();
    live.send(b"g");
    let (drawn, _) = live.data_until("the image", Duration::from_secs(30), |b| {
        contains(b, b"IMAGE-UP")
    });
    assert_eq!(
        count(&drawn, b"i=31"),
        1,
        "the live client got the image once"
    );

    // Focus out and in, a resize and back, then text only.
    live.send(b"\x1b[O\x1b[I");
    live.frame(&protocol::encode_resize(20, 70));
    live.frame(&protocol::encode_resize(24, 80));
    live.send(b"t");
    let (later, _) = live.data_until("the ticks", Duration::from_secs(30), |b| {
        contains(b, b"TICKS-DONE")
    });
    assert!(
        !contains(&later, b"\x1b_G"),
        "graphics were sent again after the image: {:?}",
        String::from_utf8_lossy(&later[..later.len().min(200)])
    );

    for attempt in 0..3 {
        let late = Client::attach(&reg, "r", 24, 80);
        let screen = late.screen();
        let transmissions = count(&screen, b"a=t");
        assert_eq!(
            transmissions, 1,
            "reattach {attempt}: {transmissions} image transmissions in the replay"
        );
        let terminal = replay_into_terminal(&screen);
        assert_eq!(
            terminal.image_bytes(31).map(|b| b.data),
            Some(vec![255, 0, 0, 255]),
            "reattach {attempt}: the replay's image"
        );
    }
}

/// An image retransmitted under the same id replaces the old one: a client
/// that attaches afterwards gets the newest pixels, not the first frame.
#[test]
fn an_image_retransmitted_under_one_id_replays_its_newest_pixels() {
    let mut reg = Registry::new();
    let mut bytes = Vec::new();
    for rgba in [[255u8, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255]] {
        let mut pixels = Vec::new();
        for _ in 0..(40 * 40) {
            pixels.extend_from_slice(&rgba);
        }
        bytes.extend_from_slice(&kitty_rgba("a=t,i=7,q=2", 40, 40, &pixels));
    }
    bytes.extend_from_slice(b"\x1b_Ga=p,i=7,p=1,U=1,c=4,r=2,q=2\x1b\\");
    bytes.extend_from_slice(placeholder_cell(7).as_bytes());
    bytes.extend_from_slice(b"\r\nSTREAMED");
    let script = cat_script(&reg, "stream", &bytes);
    reg.start("n", &format!("{script}; exec cat"));
    let c = Client::attach(&reg, "n", 24, 80);
    let _ = c.screen();
    wait_until("the stream to be drawn", Duration::from_secs(30), || {
        let mut cmd = reg.pty();
        cmd.args(["peek", "--plain", "n"]);
        cmd.output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("STREAMED"))
            .unwrap_or(false)
    });
    let late = Client::attach(&reg, "n", 24, 80);
    let terminal = replay_into_terminal(&late.screen());
    let image = terminal.image_bytes(7).expect("image 7 in the replay");
    assert_eq!(&image.data[..4], &[0, 0, 255, 255], "the newest pixels");
}

/// An image the terminal refuses (its pixel data does not match its declared
/// size) must not poison the session: a small image sent afterwards is held
/// and replayed.
#[test]
fn a_refused_image_does_not_stop_later_images() {
    let mut reg = Registry::new();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"\x1b_Ga=T,f=32,i=11,s=3456,v=2234,q=2;AAAAAA==\x1b\\");
    bytes.extend_from_slice(
        format!("\x1b_Ga=T,f=100,i=12,q=2,U=1,c=1,r=1;{RED_PNG}\x1b\\").as_bytes(),
    );
    bytes.extend_from_slice(placeholder_cell(12).as_bytes());
    bytes.extend_from_slice(b"\r\nSMALL-UP");
    let script = cat_script(&reg, "draw", &bytes);
    reg.start("p", &format!("{script}; exec cat"));
    wait_until(
        "the small image to be drawn",
        Duration::from_secs(30),
        || {
            let mut cmd = reg.pty();
            cmd.args(["peek", "--plain", "p"]);
            cmd.output()
                .map(|o| String::from_utf8_lossy(&o.stdout).contains("SMALL-UP"))
                .unwrap_or(false)
        },
    );
    let late = Client::attach(&reg, "p", 24, 80);
    let terminal = replay_into_terminal(&late.screen());
    assert_eq!(
        terminal.image_bytes(12).map(|b| b.data),
        Some(vec![255, 0, 0, 255])
    );
    assert!(
        terminal.image_bytes(11).is_none(),
        "the refused image is not held"
    );
}

/// An image the child sends by file path (`t=f`), shown through Unicode
/// placeholders, must still be there for a client that attaches afterwards,
/// the same as a direct-medium image is.
#[test]
#[ignore = "fails on main: the session's terminal refuses file-medium images, so a reattach replays placeholders with no image"]
fn an_image_sent_by_file_path_survives_a_reattach() {
    let mut reg = Registry::new();
    let pixels = reg.file("pixels.rgba", &[0, 0, 255, 255]);
    let bytes = format!(
        "\x1b_Ga=T,f=32,t=f,i=9,s=1,v=1,U=1,c=1,r=1,q=2;{}\x1b\\{}\r\nFILE-UP",
        base64(pixels.display().to_string().as_bytes()),
        placeholder_cell(9)
    );
    let script = cat_script(&reg, "draw", bytes.as_bytes());
    reg.start("f", &format!("{script}; exec cat"));
    wait_until("the image to be drawn", Duration::from_secs(30), || {
        let mut cmd = reg.pty();
        cmd.args(["peek", "--plain", "f"]);
        cmd.output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("FILE-UP"))
            .unwrap_or(false)
    });
    let late = Client::attach(&reg, "f", 24, 80);
    let terminal = replay_into_terminal(&late.screen());
    assert_eq!(
        terminal.image_bytes(9).map(|b| b.data),
        Some(vec![0, 0, 255, 255]),
        "the file-medium image after a reattach"
    );
}

// ═══ Graphics never become text ════════════════════════════════════════════

/// A client whose terminal draws no images — here `pty attach` inside a
/// terminal with graphics off — must never see graphics escapes as text:
/// not a chunked kitty transmission, not a placement, not a sixel or an
/// inline-image OSC, neither live nor in the replay after a reattach. Nor
/// may `pty peek --plain` print any of it.
#[test]
fn graphics_escapes_never_show_as_text_on_a_client_without_graphics() {
    let mut reg = Registry::new();
    let mut pixels = Vec::new();
    for i in 0..(64 * 64u32) {
        pixels.extend_from_slice(&[(i % 256) as u8, 40, 200, 255]);
    }
    let mut bytes = kitty_rgba("a=T,i=41,q=2,U=1,c=8,r=4", 64, 64, &pixels);
    bytes.extend_from_slice(placeholder_cell(41).as_bytes());
    // A cursor placement with its size given: the outer terminal here has no
    // cell metrics to derive one from.
    bytes.extend_from_slice(b"\x1b_Ga=p,i=41,p=2,c=8,r=4,q=2\x1b\\");
    bytes.extend_from_slice(b"\x1bPq#0;2;100;0;0#0~~~~~~-~~~~~~\x1b\\");
    bytes.extend_from_slice(
        format!("\x1b]1337;File=inline=1:{}\x07", base64(&pixels[..6000])).as_bytes(),
    );
    bytes.extend_from_slice(b"\r\nAFTER-IMAGES");
    let script = cat_script(&reg, "draw", &bytes);
    reg.start(
        "g",
        &format!("stty raw -echo; head -c1 >/dev/null; {script}; exec cat"),
    );

    let payload = base64(&pixels);
    let needles = [
        "Ga=",
        "a=T",
        "i=41",
        "1337",
        "File=",
        "~~~",
        &payload[..16],
        &payload[4096..4112],
    ];
    let leaked = |text: &str| -> Option<String> {
        needles
            .iter()
            .find(|needle| text.contains(**needle))
            .map(|needle| format!("{needle:?} in:\n{text}"))
    };
    let attach = || {
        let mut outer = Session::spawn(
            &pty_testkit::server::pty_bin(),
            &["attach", "g"],
            SpawnOptions {
                rows: Some(24),
                cols: Some(80),
                env: vec![("PTY_ROOT".into(), reg.root.display().to_string())],
                ..Default::default()
            },
        )
        .expect("pty attach");
        std::thread::sleep(Duration::from_millis(300));
        outer.screenshot();
        outer
    };
    let mut outer = attach();
    outer.send_keys("g");
    let live = outer
        .wait_for_text("AFTER-IMAGES", 30_000)
        .expect("live output");
    assert!(
        leaked(&live.text).is_none(),
        "live: {}",
        leaked(&live.text).unwrap_or_default()
    );
    outer.send_keys("\x1c");
    wait_until("detach", Duration::from_secs(10), || outer.has_exited());
    outer.close();

    let mut again = attach();
    let replayed = again.wait_for_text("AFTER-IMAGES", 30_000).expect("replay");
    assert!(
        leaked(&replayed.text).is_none(),
        "replay: {}",
        leaked(&replayed.text).unwrap_or_default()
    );
    again.close();

    let mut cmd = reg.pty();
    cmd.args(["peek", "--plain", "--full", "g"]);
    let out = cmd.output().expect("peek");
    let plain = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(plain.contains("AFTER-IMAGES"), "{plain}");
    assert!(
        leaked(&plain).is_none(),
        "peek --plain: {}",
        leaked(&plain).unwrap_or_default()
    );
}

/// A terminal whose owner never turned graphics on — a client that cannot
/// display images — must not hold any: nothing to draw them with, so nothing
/// to spend memory on, and no placement geometry to compute without cell
/// metrics.
#[test]
#[ignore = "fails on main: a terminal that never enabled graphics still stores a child's images under libghostty's non-zero default limit"]
fn a_terminal_that_never_enabled_graphics_holds_no_images() {
    let mut actor = TerminalActor::new(24, 80, 100);
    actor.write(&kitty_rgba("a=T,i=5,q=2,c=1,r=1", 1, 1, &[1, 2, 3, 255]));
    let limit = actor.terminal().kitty_image_storage_limit().ok();
    let held = pty_terminal::graphics::image_bytes(actor.terminal(), 5).map(|b| b.data);
    assert!(
        held.is_none(),
        "graphics were never enabled, yet the terminal holds image 5 ({held:?}) under a storage limit of {limit:?}"
    );
}

// ═══ Sixel and inline images pass through ══════════════════════════════════

/// Sixel (a DCS string) and OSC 1337 `File=` inline images (both
/// terminators, one far longer than any OSC the daemon inspects) reach the
/// client exactly as the child wrote them, so a client whose terminal draws
/// them draws them.
#[test]
fn sixel_and_inline_images_reach_the_client_unchanged() {
    let mut reg = Registry::new();
    let big: Vec<u8> = (0..30_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let sixel = b"\x1bPq\"1;1;6;6#0;2;100;0;0#0~~~~~~-~~~~~~\x1b\\".to_vec();
    let small_bel = format!(
        "\x1b]1337;File=inline=1;size=4:{}\x07",
        base64(&[1, 2, 3, 4])
    )
    .into_bytes();
    let small_st = format!(
        "\x1b]1337;File=inline=1;size=4:{}\x1b\\",
        base64(&[5, 6, 7, 8])
    )
    .into_bytes();
    let large = format!(
        "\x1b]1337;File=inline=1;size={}:{}\x07",
        big.len(),
        base64(&big)
    )
    .into_bytes();
    let mut bytes = Vec::new();
    for part in [&sixel, &small_bel, &small_st, &large] {
        bytes.extend_from_slice(part);
    }
    bytes.extend_from_slice(b"INLINE-DONE");
    let script = cat_script(&reg, "draw", &bytes);
    reg.start(
        "x",
        &format!("stty raw -echo; head -c1 >/dev/null; {script}; exec cat"),
    );
    let mut c = Client::attach(&reg, "x", 24, 80);
    c.screen();
    c.send(b"g");
    let (got, _) = c.data_until("the images", Duration::from_secs(60), |b| {
        contains(b, b"INLINE-DONE")
    });
    for (what, seq) in [
        ("sixel", &sixel),
        ("OSC 1337 (BEL)", &small_bel),
        ("OSC 1337 (ST)", &small_st),
        ("large OSC 1337", &large),
    ] {
        assert!(
            contains(&got, seq),
            "the {what} did not reach the client intact"
        );
    }
    assert_eq!(got.len(), bytes.len(), "nothing added, nothing removed");
}

// ═══ A hung filesystem under a session's cwd stalls nothing ════════════════

/// A two-directory FUSE filesystem that answers until a control file
/// appears, then stops reading requests: every later lookup or stat under it
/// waits (killably) for an answer that never comes.
const HANGFS: &str = r#"
import os, select, socket, struct, subprocess, sys, time
mnt, ctl = sys.argv[1], sys.argv[2]
a, b = socket.socketpair()
env = dict(os.environ, _FUSE_COMMFD=str(b.fileno()))
p = subprocess.Popen(["fusermount3", "-o", "fsname=hangfs,default_permissions", "--", mnt],
                     env=env, pass_fds=[b.fileno()])
_, fds, _, _ = socket.recv_fds(a, 16, 1)
p.wait()
fd = fds[0]
uid, gid, now = os.getuid(), os.getgid(), int(time.time())
def attr(ino):
    return struct.pack("<QQQQQQIIIIIIIIII", ino, 4096, 8, now, now, now, 0, 0, 0,
                       0o40755, 2, uid, gid, 0, 4096, 0)
def reply(unique, payload=b"", error=0):
    os.write(fd, struct.pack("<IiQ", 16 + len(payload), error, unique) + payload)
print("ready", flush=True)
while True:
    if os.path.exists(ctl):
        time.sleep(0.05)
        continue
    if not select.select([fd], [], [], 0.05)[0]:
        continue
    try:
        buf = os.read(fd, 1 << 21)
    except OSError:
        break
    length, op, unique, node = struct.unpack("<IIQQ", buf[:24])
    body = buf[40:length]
    if op == 26:
        reply(unique, struct.pack("<IIIIHHIIHHI28x", 7, 31, 131072, 0, 16, 12, 131072, 1, 32, 0, 0))
    elif op == 1:
        if node == 1 and body.split(b"\0", 1)[0] == b"work":
            reply(unique, struct.pack("<QQQQII", 2, 0, 0, 0, 0, 0) + attr(2))
        else:
            reply(unique, error=-2)
    elif op == 3:
        reply(unique, struct.pack("<QII", 0, 0, 0) + attr(node))
    elif op in (14, 27):
        reply(unique, struct.pack("<QII", 0, 0, 0))
    elif op in (18, 25, 28, 29, 34, 44):
        reply(unique)
    elif op == 17:
        reply(unique, bytes(80))
    elif op in (2, 36, 42):
        pass
    elif op == 38:
        reply(unique)
        break
    else:
        reply(unique, error=-38)
"#;

/// A mounted filesystem that can be told to hang.
struct HangingMount {
    mount: PathBuf,
    control: PathBuf,
    server: Child,
}

impl HangingMount {
    fn new(dir: &Path) -> Option<HangingMount> {
        let have = |bin: &str| {
            Command::new("sh")
                .args(["-c", &format!("command -v {bin}")])
                .stdout(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        if !have("python3") || !have("fusermount3") || !Path::new("/dev/fuse").exists() {
            return None;
        }
        let mount = dir.join("m");
        let control = dir.join("hang");
        std::fs::create_dir_all(&mount).ok()?;
        let script = dir.join("hangfs.py");
        std::fs::write(&script, HANGFS).ok()?;
        let mut server = Command::new("python3")
            .arg(&script)
            .arg(&mount)
            .arg(&control)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut line = String::new();
        let _ = BufReader::new(server.stdout.take()?).read_line(&mut line);
        if line.trim() != "ready" || !mount.join("work").is_dir() {
            let _ = server.kill();
            let _ = server.wait();
            return None;
        }
        Some(HangingMount {
            mount,
            control,
            server,
        })
    }

    fn work(&self) -> PathBuf {
        self.mount.join("work")
    }

    /// Stop answering, and prove it: a fresh stat of the directory does not
    /// return.
    fn hang(&self) {
        std::fs::write(&self.control, b"").expect("control file");
        std::thread::sleep(Duration::from_millis(200));
        let mut probe = Command::new("stat");
        probe.arg(self.work());
        assert!(
            run_within(probe, Duration::from_millis(1500)).is_none(),
            "the filesystem still answers a stat"
        );
    }
}

impl Drop for HangingMount {
    fn drop(&mut self) {
        // Closing the device aborts the connection: every waiter gets an
        // error instead of an answer.
        let _ = self.server.kill();
        let _ = self.server.wait();
        let _ = Command::new("fusermount3")
            .arg("-u")
            .arg("-z")
            .arg(&self.mount)
            .stderr(Stdio::null())
            .status();
    }
}

/// With the filesystem under one session's cwd hung, that session still
/// takes input and echoes, attaches, peeks and reports stats; listing
/// finishes; and another session is untouched.
#[test]
fn a_hung_filesystem_under_a_sessions_cwd_stalls_nothing() {
    let mut reg = Registry::new();
    let Some(mount) = HangingMount::new(&reg.root) else {
        eprintln!("skipping: no FUSE (python3, fusermount3, /dev/fuse) on this host");
        return;
    };
    reg.start_with(
        "h",
        24,
        80,
        Some(&mount.work()),
        &[],
        "stty raw -echo; printf 'IN-MOUNT'; exec cat",
    );
    reg.start("o", "stty raw -echo; printf 'ELSEWHERE'; exec cat");
    let first = Client::attach(&reg, "h", 24, 80);
    assert!(contains(&first.screen(), b"IN-MOUNT"));
    drop(first);
    mount.hang();

    let limit = Duration::from_secs(5);
    let pty = |args: &[&str]| {
        let mut cmd = reg.pty();
        cmd.args(args);
        let (out, took) = run_within(cmd, limit).unwrap_or_else(|| panic!("pty {args:?} hung"));
        assert!(
            out.status.success(),
            "pty {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        (String::from_utf8_lossy(&out.stdout).into_owned(), took)
    };
    let (list, took) = pty(&["list", "--json"]);
    assert!(list.contains("\"h\"") && list.contains("running"), "{list}");
    eprintln!("list with a hung cwd: {took:?}");
    pty(&["list", "--json", "--clients"]);
    let (peek, _) = pty(&["peek", "--plain", "h"]);
    assert!(peek.contains("IN-MOUNT"), "{peek}");
    pty(&["stats", "--json", "h"]);
    pty(&["send", "h", "via-send"]);

    for name in ["h", "o"] {
        let mut c = Client::attach(&reg, name, 24, 80);
        c.screen();
        let echo = echo_round_trip(&mut c, b'z');
        assert!(echo < Duration::from_secs(1), "{name}: echo took {echo:?}");
    }
    let (peek, _) = pty(&["peek", "--plain", "h"]);
    assert!(peek.contains("via-send"), "{peek}");
    // A new session elsewhere still starts.
    reg.start("n", "printf 'NEW'; exec cat");
}

/// The reconciliation pass (`pty gc`, run on a timer) checks whether a
/// permanent session's cwd still exists. With that cwd on a hung
/// filesystem the check must give up, not hold the whole pass — and every
/// other session's respawn and sweep — hostage.
#[test]
#[ignore = "fails on main: pty gc stats a permanent session's cwd on its own thread with no bound and hangs with the filesystem"]
fn gc_finishes_while_a_permanent_sessions_cwd_hangs() {
    let mut reg = Registry::new();
    let Some(mount) = HangingMount::new(&reg.root) else {
        eprintln!("skipping: no FUSE (python3, fusermount3, /dev/fuse) on this host");
        return;
    };
    reg.start_with(
        "perm",
        24,
        80,
        Some(&mount.work()),
        &["--tag", "strategy=permanent"],
        "exec cat",
    );
    mount.hang();
    let mut cmd = reg.pty();
    cmd.args(["gc", "--dry-run"]);
    let finished = run_within(cmd, Duration::from_secs(5));
    assert!(
        finished.is_some(),
        "pty gc --dry-run did not finish within 5 s"
    );
}
