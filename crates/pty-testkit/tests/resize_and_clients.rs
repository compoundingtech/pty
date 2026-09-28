//! What a session owes its child and its clients when sizes change or clients
//! come and go.
//!
//! - The child learns every new size: the window size, SIGWINCH, and the
//!   in-band size report (mode 2048) when it asked for one.
//! - History reflows with the screen: no duplicated rows, no rows left at the
//!   old width, no rows lost across narrow/widen cycles.
//! - Zero, one-cell and very large sizes neither crash the session nor stop
//!   its output.
//! - Several clients share one size by the stated rule (the smallest writable
//!   client wins per axis), a read-only viewer never resizes the child, a
//!   leaving client releases its constraint, and the reported size is the
//!   size the child sees.
//! - A session with no client has a usable size.
//! - Resize storms and clients whose terminal disappears neither lose history
//!   nor stall the session.
//! - A stalled client never blocks the child or the other clients, and costs
//!   bounded memory; a new client changes nothing the others receive.
//! - Bells, notifications, titles and clipboard writes reach every client.
//! - A session never attaches to itself.
//!
//! Every test uses its own `PTY_ROOT` in a temporary directory and the `pty`
//! binary built from this workspace.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pty_core::protocol::{
    MessageType, Packet, PacketReader, decode_geometry, encode_attach, encode_data, encode_detach,
    encode_peek, encode_resize,
};
use pty_core::stats::StatsResult;
use pty_testkit::{Session, SpawnOptions};

const WAIT: Duration = Duration::from_secs(10);

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

fn pty_bin() -> String {
    use_local_pty();
    pty_testkit::server::pty_bin()
}

fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
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

fn kill_pid(signal: &str, pid: i32) {
    let _ = Command::new("kill")
        .args([signal, &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn pid_alive(pid: i32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Resident memory of a process, in KiB, from `/proc` (0 where there is
/// none).
fn rss_kib(pid: i32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|n| n.parse().ok())
        })
        .unwrap_or(0)
}

/// A private registry with the sessions it started. Dropping it stops them
/// and removes the directory.
struct Registry {
    root: PathBuf,
    bin: String,
    names: Vec<String>,
}

impl Registry {
    fn new() -> Registry {
        let bin = pty_bin();
        // Short: a session socket path has to fit 104 bytes.
        let root = std::env::temp_dir().join(format!("prc-{}", pty_testkit::server::random_id()));
        std::fs::create_dir_all(&root).expect("temp root");
        Registry {
            root,
            bin,
            names: Vec::new(),
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.env("PTY_ROOT", &self.root);
        for key in [
            "PTY_SESSION",
            "PTY_SESSION_GENERATION",
            "PTY_SESSION_DIR",
            "PTY_REAP_ON_EXIT",
        ] {
            cmd.env_remove(key);
        }
        cmd
    }

    fn pty(&self, args: &[&str]) -> Output {
        self.command()
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("run pty")
    }

    /// `pty run -d` a `sh -c` script at the given size.
    fn run(&mut self, name: &str, rows: u16, cols: u16, script: &str) {
        self.run_with(name, Some((rows, cols)), &[], script, None);
    }

    /// `pty run -d` with optional size, extra `--env`s and an address-space
    /// cap (KiB) inherited by the daemon.
    fn run_with(
        &mut self,
        name: &str,
        size: Option<(u16, u16)>,
        env: &[(&str, &str)],
        script: &str,
        address_space_kib: Option<u64>,
    ) {
        let mut args: Vec<String> = ["run", "-d", "-e", "--no-display-name", "--id", name]
            .iter()
            .map(|s| s.to_string())
            .collect();
        if let Some((rows, cols)) = size {
            args.extend(["--rows".into(), rows.to_string(), "--cols".into(), cols.to_string()]);
        }
        for (k, v) in env {
            args.push("--env".into());
            args.push(format!("{k}={v}"));
        }
        args.extend(["--".into(), "sh".into(), "-c".into(), script.into()]);
        let out = match address_space_kib {
            None => self.command().args(&args).stdin(Stdio::null()).output(),
            // `ulimit -v` in a wrapper shell: the daemon inherits it, so no
            // allocation can take the machine down with it.
            Some(kib) => Command::new("sh")
                .arg("-c")
                .arg(format!("ulimit -v {kib}; exec \"$0\" \"$@\""))
                .arg(&self.bin)
                .args(&args)
                .env("PTY_ROOT", &self.root)
                .env_remove("PTY_SESSION")
                .env_remove("PTY_SESSION_GENERATION")
                .env_remove("PTY_SESSION_DIR")
                .env_remove("PTY_REAP_ON_EXIT")
                .stdin(Stdio::null())
                .output(),
        }
        .expect("pty run");
        assert!(
            out.status.success(),
            "pty run failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        self.names.push(name.to_string());
    }

    fn socket(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}.sock"))
    }

    fn daemon_pid(&self, name: &str) -> i32 {
        std::fs::read_to_string(self.root.join(format!("{name}.pid")))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    fn stats(&self, name: &str) -> Option<StatsResult> {
        pty_client::query_stats_in_with_timeout(
            &self.root,
            name,
            Duration::from_secs(2),
        )
        .ok()
    }

    fn size(&self, name: &str) -> Option<(u16, u16)> {
        self.stats(name).map(|s| (s.terminal.rows, s.terminal.cols))
    }

    fn wait_size(&self, name: &str, rows: u16, cols: u16) -> bool {
        wait_until(WAIT, || self.size(name) == Some((rows, cols)))
    }

    fn peek_full(&self, name: &str) -> String {
        String::from_utf8_lossy(&self.pty(&["peek", "--plain", "--full", name]).stdout).into_owned()
    }

    fn wait_peek(&self, name: &str, needle: &str) -> bool {
        wait_until(WAIT, || self.peek_full(name).contains(needle))
    }

    /// The environment a `pty` client needs to find this registry.
    fn client_env(&self) -> Vec<(String, String)> {
        vec![(
            "PTY_ROOT".to_string(),
            self.root.to_string_lossy().into_owned(),
        )]
    }

    /// A real `pty attach` in a terminal of its own: the testkit's spawned
    /// terminal is the "outer" terminal a person would be looking at.
    fn outer(&self, args: &[&str], rows: u16, cols: u16) -> Session {
        Session::spawn(
            &self.bin,
            args,
            SpawnOptions {
                rows: Some(rows),
                cols: Some(cols),
                env: self.client_env(),
                ..Default::default()
            },
        )
        .expect("spawn a client terminal")
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        for name in &self.names {
            let pid = self.daemon_pid(name);
            if pid > 0 && pid_alive(pid) {
                // A daemon a test left busy may not get to its own shutdown,
                // so do not wait on it: the child hangs up with the master.
                kill_pid("-KILL", pid);
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A raw protocol client: every packet it receives is kept for inspection.
struct Wire {
    stream: UnixStream,
    packets: Arc<Mutex<Vec<Packet>>>,
    closed: Arc<AtomicBool>,
}

impl Wire {
    fn connect(reg: &Registry, name: &str, hello: &[u8]) -> Wire {
        let mut stream = UnixStream::connect(reg.socket(name)).expect("connect to the session");
        stream.write_all(hello).expect("hello");
        let packets = Arc::new(Mutex::new(Vec::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let mut reader = stream.try_clone().expect("clone");
        let (sink, done) = (packets.clone(), closed.clone());
        std::thread::spawn(move || {
            let mut parser = PacketReader::new();
            let mut buf = vec![0u8; 1 << 16];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => match parser.feed(&buf[..n]) {
                        Ok(ps) => sink.lock().unwrap().extend(ps),
                        Err(_) => break,
                    },
                }
            }
            done.store(true, Ordering::Release);
        });
        Wire {
            stream,
            packets,
            closed,
        }
    }

    fn attach(reg: &Registry, name: &str, rows: u16, cols: u16) -> Wire {
        let w = Wire::connect(reg, name, &encode_attach(rows, cols));
        assert!(w.wait_type(MessageType::Screen), "no SCREEN for the attach");
        w
    }

    fn peek(reg: &Registry, name: &str) -> Wire {
        let w = Wire::connect(reg, name, &encode_peek(false, false));
        assert!(w.wait_type(MessageType::Screen), "no SCREEN for the peek");
        w
    }

    fn send(&mut self, bytes: &[u8]) {
        let _ = self.stream.write_all(bytes);
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.send(&encode_resize(rows, cols));
    }

    fn input(&mut self, text: &str) {
        self.send(&encode_data(text.as_bytes()));
    }

    fn wait_type(&self, t: MessageType) -> bool {
        wait_until(WAIT, || self.packets.lock().unwrap().iter().any(|p| p.type_ == t))
    }

    /// SCREEN and DATA payloads, in order.
    fn text(&self) -> String {
        let ps = self.packets.lock().unwrap();
        let mut out = Vec::new();
        for p in ps.iter() {
            if matches!(p.type_, MessageType::Screen | MessageType::Data) {
                out.extend_from_slice(&p.payload);
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// DATA payloads only: what arrived live, not the replay.
    fn data(&self) -> Vec<u8> {
        let ps = self.packets.lock().unwrap();
        let mut out = Vec::new();
        for p in ps.iter().filter(|p| p.type_ == MessageType::Data) {
            out.extend_from_slice(&p.payload);
        }
        out
    }

    /// Whether the DATA seen so far ends with (or recently contained)
    /// `needle`: only the last 64 KiB are searched, so a large stream is
    /// not copied on every poll.
    fn data_tail_contains(&self, needle: &[u8]) -> bool {
        let ps = self.packets.lock().unwrap();
        let mut tail: Vec<u8> = Vec::new();
        for p in ps.iter().rev().filter(|p| p.type_ == MessageType::Data) {
            let mut chunk = p.payload.clone();
            chunk.extend_from_slice(&tail);
            tail = chunk;
            if tail.len() >= 64 * 1024 {
                break;
            }
        }
        contains(&tail, needle)
    }

    fn data_len(&self) -> usize {
        self.packets
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.type_ == MessageType::Data)
            .map(|p| p.payload.len())
            .sum()
    }

    fn wait_text(&self, needle: &str) -> bool {
        wait_until(WAIT, || self.text().contains(needle))
    }

    fn geometries(&self) -> Vec<(u16, u16)> {
        self.packets
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.type_ == MessageType::Geometry)
            .map(|p| decode_geometry(&p.payload))
            .collect()
    }

    fn count(&self) -> usize {
        self.packets.lock().unwrap().len()
    }

    fn clear(&self) {
        self.packets.lock().unwrap().clear();
    }
}

impl Drop for Wire {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The last `WINCH r c` report a child printed.
fn last_winch(text: &str) -> Option<String> {
    text.lines()
        .filter_map(|l| l.find("WINCH ").map(|i| l[i..].trim().to_string()))
        .next_back()
}

/// A child that reports its size at start and on every SIGWINCH, and prints
/// a tick every 100 ms so a stall shows as missing ticks.
const WINCH_REPORTER: &str = "trap 'echo \"WINCH $(stty size)\"' WINCH; \
     echo \"START $(stty size)\"; i=0; \
     while :; do i=$((i+1)); [ $((i % 2)) -eq 0 ] && echo \"TICK-$i\"; sleep 0.05; done";

// ── the child learns every new size ─────────────────────────────────────────

/// A person resizes the terminal `pty attach` runs in: the attach client
/// turns the SIGWINCH into a RESIZE, and the child gets the new window size
/// and its own SIGWINCH. Back-to-back resizes end with the child at the last
/// size, and the size the session reports is the one the child sees.
#[test]
fn an_attach_client_resize_reaches_the_child_as_window_size_and_sigwinch() {
    let mut reg = Registry::new();
    reg.run("winch", 37, 111, WINCH_REPORTER);
    // No client yet: the child starts at the size the session was made with.
    assert!(reg.wait_peek("winch", "START 37 111"), "{}", reg.peek_full("winch"));

    let mut outer = reg.outer(&["attach", "winch"], 37, 111);
    outer
        .wait_for_text("START 37 111", 10_000)
        .expect("the attach client shows the session");

    outer.resize(30, 100);
    outer
        .wait_for_text("WINCH 30 100", 10_000)
        .expect("the child saw the first resize");

    for (rows, cols) in [(31, 101), (32, 102), (33, 103)] {
        outer.resize(rows, cols);
    }
    assert!(reg.wait_size("winch", 33, 103), "stats: {:?}", reg.size("winch"));
    assert!(
        wait_until(WAIT, || last_winch(&reg.peek_full("winch")).as_deref()
            == Some("WINCH 33 103")),
        "the child's last report is not the final size:\n{}",
        reg.peek_full("winch")
    );
    outer.close();
}

/// A child that turned on in-band size reports (`CSI ? 2048 h`) and then
/// waits for input must receive `CSI 48 ; rows ; cols ; … t` when the session
/// is resized, without having to print anything first.
#[test]
#[ignore = "fails on main: the mode-2048 report a resize generates stays queued in the daemon until the child next prints"]
fn a_resize_delivers_the_in_band_size_report_the_child_asked_for() {
    let mut reg = Registry::new();
    reg.run(
        "inband",
        24,
        80,
        "stty raw -echo; printf '\\033[?2048h'; printf 'MODE-ON\\r\\n'; exec cat -v",
    );
    let mut client = Wire::attach(&reg, "inband", 24, 80);
    assert!(client.wait_text("MODE-ON"), "{}", client.text());

    client.resize(30, 100);
    assert!(reg.wait_size("inband", 30, 100));
    // `cat -v` prints the report it reads as `^[[48;30;100;…t`.
    let arrived = wait_until(Duration::from_secs(3), || client.text().contains("[48;30;100;"));
    assert!(
        arrived,
        "no in-band size report reached the child after the resize; the child printed: {:?}",
        client.data()
    );
}

// ── history reflows with the screen ─────────────────────────────────────────

fn long_line(i: usize) -> String {
    format!("{i:04} {}", "X".repeat(70))
}

/// Lines of 75 cells written at 40 columns wrap onto two rows. Widening to
/// 120 columns must join every one of them back into a single row, once, in
/// order; narrowing again must split them again without losing or repeating
/// any; and a client that attaches afterwards gets the same history.
#[test]
fn widening_and_narrowing_reflow_history_without_duplicates_or_stale_rows() {
    let mut reg = Registry::new();
    let x70 = "X".repeat(70);
    reg.run(
        "reflow",
        20,
        40,
        &format!(
            "i=0; while [ $i -lt 300 ]; do printf '%04d {x70}\\n' $i; i=$((i+1)); done; \
             echo FILLED; exec cat"
        ),
    );
    let mut client = Wire::attach(&reg, "reflow", 20, 40);
    assert!(reg.wait_peek("reflow", "FILLED"));

    let check_wide = |full: &str| {
        for i in 0..300 {
            let head = format!("{i:04} ");
            let rows: Vec<&str> = full.lines().filter(|l| l.starts_with(&head)).collect();
            assert_eq!(rows.len(), 1, "line {i} appears {} times", rows.len());
            assert_eq!(rows[0].trim_end(), long_line(i), "line {i} was not rejoined");
        }
    };

    client.resize(20, 120);
    assert!(reg.wait_size("reflow", 20, 120));
    check_wide(&reg.peek_full("reflow"));

    client.resize(20, 40);
    assert!(reg.wait_size("reflow", 20, 40));
    let narrow = reg.peek_full("reflow");
    for i in 0..300 {
        let head = format!("{i:04} ");
        assert_eq!(
            narrow.lines().filter(|l| l.starts_with(&head)).count(),
            1,
            "line {i} at 40 columns"
        );
    }
    let xs: usize = narrow.chars().filter(|c| *c == 'X').count();
    assert_eq!(xs, 300 * 70, "cells were lost or repeated at 40 columns");

    client.resize(20, 120);
    assert!(reg.wait_size("reflow", 20, 120));
    check_wide(&reg.peek_full("reflow"));

    // A client arriving now replays the same history, each line once.
    let late = Wire::attach(&reg, "reflow", 20, 120);
    let replay = late.text();
    for i in (0..300).step_by(7) {
        assert_eq!(
            replay.matches(&long_line(i)).count(),
            1,
            "the replay carries line {i} other than once"
        );
    }
}

/// A block graphic drawn in half-block cells, the way a terminal QR code is,
/// 23 cells wide and 12 rows tall.
fn block_graphic() -> Vec<String> {
    let glyphs = ['█', '▀', '▄', ' '];
    (0..12)
        .map(|r| {
            let body: String = (0..21)
                .map(|c| glyphs[(r * 7 + c * 3 + (r * c) % 5) % 4])
                .collect();
            format!("█{body}█")
        })
        .collect()
}

fn graphic_rows(full: &str) -> Vec<String> {
    let lines: Vec<&str> = full.lines().collect();
    let start = lines.iter().position(|l| l.starts_with("QR-BEGIN")).expect("begin marker");
    let end = lines.iter().position(|l| l.starts_with("QR-END")).expect("end marker");
    lines[start + 1..end]
        .iter()
        .map(|l| l.trim_end().to_string())
        .collect()
}

/// Narrowing below the graphic's width wraps every row of it; widening again
/// must restore exactly the rows it had. Repeated cycles — including the
/// one-column redraw nudge a client attaching at another size causes — must
/// not lose a row each time.
#[test]
fn a_block_graphic_keeps_every_row_through_narrow_and_widen_cycles() {
    let mut reg = Registry::new();
    let expected = block_graphic();
    let mut script = String::from("printf 'QR-BEGIN\\n'; ");
    for row in &expected {
        script.push_str(&format!("printf '\\033[47;30m%s\\033[0m\\n' '{row}'; "));
    }
    script.push_str("printf 'QR-END\\n'; exec cat");
    reg.run("qr", 24, 80, &script);
    let mut client = Wire::attach(&reg, "qr", 24, 80);
    assert!(reg.wait_peek("qr", "QR-END"));
    assert_eq!(graphic_rows(&reg.peek_full("qr")), expected, "as drawn");

    for (cycle, narrow) in [12u16, 40, 9, 17].into_iter().enumerate() {
        client.resize(24, narrow);
        assert!(reg.wait_size("qr", 24, narrow));
        client.resize(24, 80);
        assert!(reg.wait_size("qr", 24, 80));
        assert_eq!(
            graphic_rows(&reg.peek_full("qr")),
            expected,
            "after cycle {cycle} through {narrow} columns"
        );
    }

    // A second client at another size makes the daemon nudge the child with
    // a one-column-narrower resize and back.
    for _ in 0..3 {
        let other = Wire::attach(&reg, "qr", 30, 100);
        drop(other);
        std::thread::sleep(Duration::from_millis(150));
    }
    assert!(reg.wait_size("qr", 24, 80));
    assert_eq!(graphic_rows(&reg.peek_full("qr")), expected, "after redraw nudges");
}

// ── zero, tiny and very large sizes ─────────────────────────────────────────

fn tick_count(text: &str) -> usize {
    text.matches("TICK-").count()
}

/// A client the size of a wall of monitors (710x96, then 2000x300) gets the
/// size, keeps receiving output, and a client attaching at that size gets a
/// complete replay rather than a frame too large to deliver.
#[test]
fn a_very_large_client_keeps_receiving_output_and_a_full_replay() {
    let mut reg = Registry::new();
    reg.run(
        "wide",
        24,
        80,
        "i=0; while [ $i -lt 400 ]; do echo \"HISTORY-$i\"; i=$((i+1)); done; \
         j=0; while :; do echo \"TICK-$j\"; j=$((j+1)); sleep 0.1; done",
    );
    for (rows, cols) in [(96u16, 710u16), (300, 2000)] {
        let big = Wire::attach(&reg, "wide", rows, cols);
        assert!(reg.wait_size("wide", rows, cols), "{rows}x{cols}");
        let before = tick_count(&big.text());
        assert!(
            wait_until(WAIT, || tick_count(&big.text()) >= before + 3),
            "output stopped at {rows}x{cols}"
        );
        let late = Wire::attach(&reg, "wide", rows, cols);
        let replay = late.text();
        assert!(replay.contains("HISTORY-0") && replay.contains("HISTORY-399"));
        assert!(
            wait_until(WAIT, || tick_count(&String::from_utf8_lossy(&late.data())) >= 3),
            "a late client at {rows}x{cols} gets no live output"
        );
        assert!(!late.closed.load(Ordering::Acquire));
    }
}

/// A zero-row, zero-column or zero-by-zero request is ignored rather than
/// applied; a 1x1 size is applied and survived; and `pty attach` from a
/// terminal that reports 0x0 stays attached. Output never stops.
#[test]
fn zero_and_one_cell_sizes_are_survived_and_output_continues() {
    let mut reg = Registry::new();
    reg.run("tiny", 24, 80, WINCH_REPORTER);
    let viewer = Wire::peek(&reg, "tiny");
    let ticking = |w: &Wire| {
        let before = tick_count(&String::from_utf8_lossy(&w.data()));
        wait_until(WAIT, || tick_count(&String::from_utf8_lossy(&w.data())) >= before + 2)
    };

    let mut zero = Wire::attach(&reg, "tiny", 0, 0);
    assert_eq!(reg.size("tiny"), Some((24, 80)), "a 0x0 attach was applied");
    zero.resize(0, 100);
    zero.resize(30, 0);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(reg.size("tiny"), Some((24, 80)), "a zero axis was applied");
    assert!(ticking(&viewer), "output stopped after zero-size requests");

    zero.resize(1, 1);
    assert!(reg.wait_size("tiny", 1, 1));
    assert!(ticking(&viewer), "output stopped at 1x1");
    zero.resize(24, 80);
    assert!(reg.wait_size("tiny", 24, 80));
    assert!(ticking(&viewer));
    drop(zero);

    // A real attach client whose terminal says 0x0.
    let outer = reg.outer(&["attach", "tiny"], 0, 0);
    assert!(
        wait_until(WAIT, || reg.stats("tiny").is_some_and(|s| s.clients.attached == 1)),
        "the 0x0 attach client did not stay attached"
    );
    assert_eq!(reg.size("tiny"), Some((24, 80)));
    assert!(ticking(&viewer), "output stopped with a 0x0 client attached");
    drop(outer);
}

/// No real terminal is 65535 rows tall, but a client can ask for it. The
/// session must not stop serving output to everyone else while it applies
/// (or refuses) that size.
#[test]
#[ignore = "fails on main: an unclamped 65535-row request blocks the daemon's only thread for ~50 s (debug build); no output or stats meanwhile"]
fn an_absurd_client_size_does_not_stall_the_session_for_everyone() {
    let mut reg = Registry::new();
    // 4 GiB of address space: whatever the daemon tries, it cannot take the
    // machine with it.
    reg.run_with(
        "absurd",
        Some((24, 80)),
        &[],
        "j=0; while :; do echo \"TICK-$j\"; j=$((j+1)); sleep 0.1; done",
        Some(4 * 1024 * 1024),
    );
    let viewer = Wire::peek(&reg, "absurd");
    let mut writer = Wire::attach(&reg, "absurd", 24, 80);
    assert!(wait_until(WAIT, || tick_count(&String::from_utf8_lossy(&viewer.data())) >= 2));

    writer.resize(65535, 80);
    std::thread::sleep(Duration::from_millis(300));
    let before = tick_count(&String::from_utf8_lossy(&viewer.data()));
    let started = Instant::now();
    let answered = reg.stats("absurd").is_some();
    let ticking = wait_until(Duration::from_secs(3), || {
        tick_count(&String::from_utf8_lossy(&viewer.data())) >= before + 3
    });
    assert!(
        answered && ticking,
        "after a 65535x80 request: stats answered={answered}, output resumed={ticking} \
         within {:?}",
        started.elapsed()
    );
}

// ── a session with no client ────────────────────────────────────────────────

/// A detached session runs at the size it was created with, not a fixed
/// floor; after its last client leaves it keeps that client's size rather
/// than snapping back; and a session created by a non-terminal caller with
/// no size gets 24x80.
#[test]
fn a_detached_session_has_the_size_it_was_given_and_keeps_the_last_client_size() {
    let mut reg = Registry::new();
    reg.run("headless", 50, 160, WINCH_REPORTER);
    assert!(reg.wait_peek("headless", "START 50 160"), "{}", reg.peek_full("headless"));
    assert_eq!(reg.size("headless"), Some((50, 160)));

    let mut client = Wire::attach(&reg, "headless", 40, 130);
    assert!(reg.wait_peek("headless", "WINCH 40 130"));
    client.send(&encode_detach());
    drop(client);
    assert!(wait_until(WAIT, || reg
        .stats("headless")
        .is_some_and(|s| s.clients.attached == 0)));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(reg.size("headless"), Some((40, 130)), "the size snapped back");
    assert_eq!(
        last_winch(&reg.peek_full("headless")).as_deref(),
        Some("WINCH 40 130"),
        "the child was resized after the last client left"
    );

    reg.run_with("unsized", None, &[], WINCH_REPORTER, None);
    assert!(reg.wait_peek("unsized", "START 24 80"), "{}", reg.peek_full("unsized"));
}

/// `pty run -d` from a terminal that momentarily reports 0x0 must still give
/// the session a usable size, and the size the session reports must be the
/// size its child sees.
#[test]
#[ignore = "fails on main: a 0x0 launcher terminal creates a 0x0 pty (the child sees 0 0) behind a 1x1 screen that stats reports"]
fn a_session_started_from_a_zero_size_terminal_gets_a_usable_size() {
    let mut reg = Registry::new();
    // The child writes its size to a file: a 1-column screen would break
    // any line it printed into single characters.
    let report = reg.root.join("child-size");
    let script = format!("stty size > '{}'; exec cat", report.display());
    let launcher = reg.outer(
        &[
            "run",
            "-d",
            "-e",
            "--no-display-name",
            "--id",
            "zerolaunch",
            "--",
            "sh",
            "-c",
            &script,
        ],
        0,
        0,
    );
    reg.names.push("zerolaunch".into());
    let read_report = || {
        std::fs::read_to_string(&report)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    assert!(wait_until(WAIT, || read_report().is_some()), "the child never reported");
    drop(launcher);

    let child_size = read_report().unwrap_or_default();
    let (rows, cols) = reg.size("zerolaunch").expect("stats");
    assert!(
        rows > 1 && cols > 1 && child_size == format!("{rows} {cols}"),
        "the child sees `{child_size}` and the session reports {rows}x{cols}; its screen \
         reads {:?}",
        reg.peek_full("zerolaunch")
    );
}

// ── several clients ─────────────────────────────────────────────────────────

/// Two `pty attach` clients in terminals of different sizes: the session
/// takes the smaller size on each axis whichever client is typing, a
/// read-only `pty peek -f` viewer never resizes the child whatever its size,
/// the smaller client leaving (its terminal closed, no DETACH) gives the
/// larger one its size back, and at every step the size `pty stats` reports
/// is the size the child reads from its terminal.
#[test]
fn attach_clients_share_the_smaller_size_viewers_never_resize_and_leavers_release() {
    let mut reg = Registry::new();
    reg.run("multi", 24, 80, WINCH_REPORTER);

    let check = |rows: u16, cols: u16| {
        assert!(reg.wait_size("multi", rows, cols), "stats {:?}", reg.size("multi"));
        let want = format!("WINCH {rows} {cols}");
        assert!(
            wait_until(WAIT, || last_winch(&reg.peek_full("multi")).as_deref()
                == Some(want.as_str())),
            "the child's last size report is {:?}, stats say {rows}x{cols}",
            last_winch(&reg.peek_full("multi"))
        );
    };

    let mut wide = reg.outer(&["attach", "multi"], 40, 130);
    check(40, 130);
    let narrow = reg.outer(&["attach", "multi"], 20, 60);
    check(20, 60);

    let mut viewer = reg.outer(&["peek", "-f", "multi"], 10, 30);
    viewer.wait_for_text("TICK-", 10_000).expect("the viewer follows");
    viewer.resize(8, 20);
    std::thread::sleep(Duration::from_millis(300));
    check(20, 60);

    // Activity in the wide client does not hand it the size.
    wide.type_str("typing in the wide client");
    wide.resize(41, 131);
    std::thread::sleep(Duration::from_millis(300));
    check(20, 60);

    drop(narrow);
    check(41, 131);
    let stats = reg.stats("multi").expect("stats");
    assert_eq!(stats.clients.attached, 1);
    assert_eq!(stats.clients.read_only, 1);
    drop(viewer);
    drop(wide);
}

// ── storms and vanished clients ─────────────────────────────────────────────

/// Forty resizes oscillating between two sizes with no pause between them,
/// over history whose lines wrap at one size and not the other: the history
/// is intact afterwards, the child is alive and saw the final size, and the
/// session answers promptly.
#[test]
fn an_oscillating_resize_storm_keeps_history_and_leaves_the_child_at_the_final_size() {
    let mut reg = Registry::new();
    let pad = "0".repeat(150);
    reg.run(
        "storm",
        52,
        115,
        &format!(
            "i=0; while [ $i -lt 300 ]; do printf '%05d {pad}\\n' $i; i=$((i+1)); done; \
             echo FILLED; {WINCH_REPORTER}"
        ),
    );
    let mut client = Wire::attach(&reg, "storm", 52, 115);
    assert!(reg.wait_peek("storm", "FILLED"));

    let started = Instant::now();
    for _ in 0..20 {
        client.resize(85, 154);
        client.resize(52, 115);
    }
    client.resize(60, 130);
    assert!(
        wait_until(Duration::from_secs(5), || reg.size("storm") == Some((60, 130))),
        "the storm was not absorbed within 5 s: {:?}",
        reg.size("storm")
    );
    let absorbed = started.elapsed();

    let full = reg.peek_full("storm");
    for i in 0..300 {
        let head = format!("{i:05} ");
        assert_eq!(
            full.lines().filter(|l| l.starts_with(&head)).count(),
            1,
            "history line {i} after the storm"
        );
    }
    assert!(
        wait_until(WAIT, || last_winch(&reg.peek_full("storm")).as_deref() == Some("WINCH 60 130")),
        "the child did not end at the final size"
    );
    let ticks = tick_count(&reg.peek_full("storm"));
    assert!(
        wait_until(WAIT, || tick_count(&reg.peek_full("storm")) > ticks),
        "the child stopped ticking after the storm"
    );
    eprintln!("storm of 41 resizes absorbed in {absorbed:?}");
}

/// `pty attach` running in a 50x200 terminal whose terminal goes away (the
/// master side closes, as when an SSH connection dies): the session is never
/// resized to a fallback 24x80, stays responsive, and the client's
/// constraint goes with it.
#[test]
fn a_client_whose_terminal_disappears_releases_its_size_without_a_fallback_resize() {
    let mut reg = Registry::new();
    reg.run("orphan", 30, 100, WINCH_REPORTER);
    let viewer = Wire::peek(&reg, "orphan");

    let pair = pty_spawn::open(50, 200).expect("pty");
    let mut cmd = portable_pty::CommandBuilder::new(&reg.bin);
    cmd.args(["attach", "orphan"]);
    cmd.env("PTY_ROOT", &reg.root);
    cmd.env("TERM", "xterm-256color");
    cmd.env_remove("PTY_SESSION");
    let mut child = pair.slave.spawn_command(cmd).expect("spawn pty attach");
    drop(pair.slave);
    // Keep the terminal drained while it exists. The reader is a second
    // descriptor for the master, so it has to go too before the terminal is
    // really gone: it lets go at the next read after `gone` is set, which
    // the child's ticks guarantee within 100 ms.
    let mut reader = pair.master.try_clone_reader().expect("reader");
    let gone = Arc::new(AtomicBool::new(false));
    let reader_gone = gone.clone();
    let drained = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while matches!(reader.read(&mut buf), Ok(n) if n > 0) {
            if reader_gone.load(Ordering::Acquire) {
                break;
            }
        }
    });
    assert!(reg.wait_size("orphan", 50, 200), "{:?}", reg.size("orphan"));

    gone.store(true, Ordering::Release);
    drop(pair.master);
    drained.join().expect("reader thread");
    assert!(
        wait_until(WAIT, || reg.stats("orphan").is_some_and(|s| s.clients.attached == 0)),
        "the client of a vanished terminal is still attached"
    );
    let _ = child.kill();
    let _ = child.wait();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(reg.size("orphan"), Some((50, 200)), "the size moved after the client left");
    assert!(
        !viewer.geometries().contains(&(24, 80)),
        "a fallback 24x80 reached the session: {:?}",
        viewer.geometries()
    );
    let started = Instant::now();
    assert!(reg.stats("orphan").is_some());
    assert!(started.elapsed() < Duration::from_secs(1), "stats took {:?}", started.elapsed());
}

// ── clients never cost each other ───────────────────────────────────────────

/// How many bytes a unix stream socket holds for a peer that never reads.
fn kernel_socket_capacity() -> usize {
    let (a, _b) = UnixStream::pair().expect("pair");
    a.set_nonblocking(true).expect("nonblocking");
    let chunk = [0u8; 4096];
    let mut total = 0;
    while let Ok(n) = (&a).write(&chunk) {
        total += n;
    }
    total
}

/// Attach and then never read again, like a client that was stopped.
fn stalled_client(reg: &Registry, name: &str) -> UnixStream {
    let mut s = UnixStream::connect(reg.socket(name)).expect("connect");
    s.write_all(&encode_attach(24, 80)).expect("attach");
    s
}

/// Drain whatever a stalled client's socket still has to deliver.
fn drain(mut s: UnixStream) -> usize {
    s.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    let mut buf = vec![0u8; 1 << 16];
    let mut total = 0;
    while let Ok(n) = s.read(&mut buf) {
        if n == 0 {
            break;
        }
        total += n;
    }
    total
}

/// A client that stops reading must not slow the child's output or the
/// other clients: every byte of an 8 MB burst reaches the reading client
/// while the stalled one sits on a full socket.
#[test]
fn a_stalled_client_does_not_block_the_child_or_the_other_clients() {
    let mut reg = Registry::new();
    reg.run(
        "stall",
        24,
        80,
        "read go; head -c 8000000 /dev/zero; printf 'BURST-DONE'; exec cat",
    );
    let stalled = stalled_client(&reg, "stall");
    let mut reader = Wire::attach(&reg, "stall", 24, 80);
    reader.input("go\n");
    assert!(
        wait_until(Duration::from_secs(15), || reader.data_tail_contains(b"BURST-DONE")),
        "the reading client got {} bytes",
        reader.data_len()
    );
    assert!(reader.data_len() >= 8_000_000);
    // The child is not blocked either: it still answers.
    reader.input("still-here\n");
    assert!(wait_until(WAIT, || reader.data_tail_contains(b"still-here")));
    drop(stalled);
}

/// A stalled client may cost the session a bounded amount of memory, not a
/// copy of everything the child prints while it is stalled. The bound used
/// here is twice the 8 MiB the session substrate allows one attachment before
/// it detaches it; a 48 MB burst is well past it.
#[test]
#[ignore = "fails on main: the daemon queues every byte a stalled client cannot take in an unbounded channel; 48 MB of output costs ~48 MB"]
fn a_stalled_client_costs_bounded_daemon_memory() {
    let mut reg = Registry::new();
    const BURST: usize = 48_000_000;
    reg.run(
        "hoard",
        24,
        80,
        &format!("read go; head -c {BURST} /dev/zero; printf 'BURST-DONE'; exec cat"),
    );
    let daemon = reg.daemon_pid("hoard");
    let stalled = stalled_client(&reg, "hoard");
    let mut reader = Wire::attach(&reg, "hoard", 24, 80);
    let rss_before = rss_kib(daemon);
    reader.input("go\n");
    assert!(
        wait_until(Duration::from_secs(20), || reader.data_tail_contains(b"BURST-DONE")),
        "the reading client got {} bytes",
        reader.data_len()
    );
    // The reader has everything; the daemon now holds what the stalled
    // client has not taken.
    std::thread::sleep(Duration::from_millis(300));
    let rss_after = rss_kib(daemon);
    let kernel = kernel_socket_capacity();
    let delivered = drain(stalled);
    let held_by_daemon = delivered.saturating_sub(kernel);
    let bound = 16 * 1024 * 1024;
    assert!(
        held_by_daemon <= bound,
        "the daemon held {held_by_daemon} bytes for one stalled client ({delivered} delivered \
         after the stall, the kernel holds at most {kernel}); daemon RSS {rss_before} KiB -> \
         {rss_after} KiB"
    );
}

/// A client arriving — at the session's size, at a larger size, or as a
/// read-only viewer — sends nothing to the clients already there, and from
/// then on every client receives the same bytes.
#[test]
fn a_new_client_changes_nothing_the_existing_clients_receive() {
    let mut reg = Registry::new();
    reg.run("fair", 24, 80, "exec cat");
    let mut first = Wire::attach(&reg, "fair", 24, 80);
    std::thread::sleep(Duration::from_millis(300));
    first.clear();

    let second = Wire::attach(&reg, "fair", 24, 80);
    let larger = Wire::attach(&reg, "fair", 40, 120);
    let viewer = Wire::peek(&reg, "fair");
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        first.count(),
        0,
        "the first client received packets because others arrived: {:?}",
        first.packets.lock().unwrap().iter().map(|p| p.type_).collect::<Vec<_>>()
    );

    for w in [&second, &larger, &viewer] {
        w.clear();
    }
    first.input("same-bytes-for-everyone\n");
    // The terminal echoes the line and `cat` prints it again.
    let needle = b"same-bytes-for-everyone\r\n";
    for w in [&first, &second, &larger, &viewer] {
        assert!(wait_until(WAIT, || {
            w.data().windows(needle.len()).filter(|x| x == needle).count() == 2
        }));
    }
    std::thread::sleep(Duration::from_millis(200));
    let expected = first.data();
    for w in [&second, &larger, &viewer] {
        assert_eq!(w.data(), expected);
    }
}

// ── terminal events reach every client ──────────────────────────────────────

/// A bell, OSC 9 / 777 / 99 notifications, an OSC 0 title and an OSC 52
/// clipboard write reach two writable clients of different sizes and a
/// read-only viewer, byte for byte.
#[test]
fn bells_notifications_titles_and_clipboard_writes_reach_every_client() {
    let mut reg = Registry::new();
    reg.run(
        "events",
        24,
        80,
        "read go; printf '\\a'; printf '\\033]9;note-nine\\033\\\\'; \
         printf '\\033]777;notify;Title;Body\\a'; printf '\\033]99;;body-99\\033\\\\'; \
         printf '\\033]0;title-zero\\a'; printf '\\033]52;c;aGVsbG8=\\a'; \
         printf 'EVENTS-DONE'; exec cat",
    );
    let mut a = Wire::attach(&reg, "events", 24, 80);
    let b = Wire::attach(&reg, "events", 30, 100);
    let viewer = Wire::peek(&reg, "events");
    // Let every client's replay settle so what follows is live DATA.
    std::thread::sleep(Duration::from_millis(400));
    a.input("go\n");

    let expected: [&[u8]; 6] = [
        b"\x07",
        b"\x1b]9;note-nine\x1b\\",
        b"\x1b]777;notify;Title;Body\x07",
        b"\x1b]99;;body-99\x1b\\",
        b"\x1b]0;title-zero\x07",
        b"\x1b]52;c;aGVsbG8=\x07",
    ];
    for (who, w) in [("a", &a), ("b", &b), ("viewer", &viewer)] {
        assert!(
            wait_until(WAIT, || contains(&w.data(), b"EVENTS-DONE")),
            "{who} got no events"
        );
        let data = w.data();
        for seq in expected {
            assert!(contains(&data, seq), "{who} is missing {seq:?} in {data:?}");
        }
    }
}

// ── a session never attaches to itself ──────────────────────────────────────

fn shell_session(reg: &mut Registry, name: &str) -> Wire {
    let bin_dir = Path::new(&reg.bin).parent().unwrap().to_string_lossy().into_owned();
    let path = format!("{bin_dir}:/usr/bin:/bin");
    let root = reg.root.to_string_lossy().into_owned();
    reg.run_with(
        name,
        Some((24, 80)),
        &[("PTY_ROOT", &root), ("PATH", &path), ("PS1", "$ ")],
        "exec sh -i",
        None,
    );
    let mut user = Wire::attach(reg, name, 24, 80);
    user.input("echo SHELL-$((40+2))\n");
    assert!(user.wait_text("SHELL-42"), "{}", user.text());
    user
}

/// Bytes a client receives over one quiet second.
fn idle_bytes(w: &Wire) -> usize {
    let before = w.data_len();
    std::thread::sleep(Duration::from_secs(1));
    w.data_len() - before
}

/// Running `pty attach` for the session from inside that session is refused
/// with an error, nothing loops, and the shell stays usable.
#[test]
fn attaching_to_its_own_session_from_inside_is_refused_and_the_session_stays_usable() {
    let mut reg = Registry::new();
    let mut user = shell_session(&mut reg, "selfish");
    user.input("pty attach selfish; echo EXIT-$?\n");
    assert!(user.wait_text("already inside pty session"), "{}", user.text());
    assert!(user.wait_text("EXIT-1"), "{}", user.text());
    assert_eq!(reg.stats("selfish").map(|s| s.clients.attached), Some(1));
    assert!(idle_bytes(&user) < 1024, "the session kept printing");
    user.input("echo ALIVE-$((6*7))\n");
    assert!(user.wait_text("ALIVE-42"));
}

/// The refusal must not depend on the child's environment still naming the
/// session: a shell that dropped `PTY_SESSION` (as `env -u`, `sudo` or a
/// login shell do) is still inside the session's own terminal.
#[test]
#[ignore = "fails on main: with PTY_SESSION unset the daemon accepts its own pty as a client, which loops the session's output into itself"]
fn a_self_attach_is_refused_even_without_the_session_variable() {
    let mut reg = Registry::new();
    let mut user = shell_session(&mut reg, "selfenv");
    user.input("env -u PTY_SESSION pty attach selfenv; echo EXIT-$?\n");
    std::thread::sleep(Duration::from_secs(1));
    let attached = reg.stats("selfenv").map(|s| s.clients.attached);
    let looping = idle_bytes(&user);
    // Leave the loop before asserting, so a failure does not leave it spinning.
    user.input("\x1c");
    assert!(
        attached == Some(1) && looping < 64 * 1024,
        "the session accepted itself as a client (attached = {attached:?}) and sent {looping} \
         bytes in one idle second"
    );
    assert!(user.wait_text("EXIT-"));
}
