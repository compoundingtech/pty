//! Terminal queries and colours a child sends, held to what a session runtime
//! owes it.
//!
//! Every query the child asks gets exactly one well-formed reply on its pty,
//! in request order, whether or not a client is attached, and is never
//! answered a second time by an attached client's own terminal. Colour and
//! geometry replies describe the terminal the session is really shown on.
//! The child's colour sets, clipboard writes, and the difference between
//! palette, truecolor, reverse-video and default colours reach a client
//! intact, both live and in the replay a reattaching client gets.
//!
//! The probing child puts its tty in raw mode, sends a query, and records
//! every byte that comes back on its stdin until a second passes with
//! nothing more (`stty min 0 time 10` + `cat`), so a missing reply and a
//! duplicated one are both visible.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use libghostty_vt::style::RgbColor;
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use pty_core::protocol::encode_resize_with_cell;
use pty_terminal::{CellSnap, ColorSnap};
use pty_testkit::server::{connect, pty_bin, random_id, spawn_daemon};
use pty_testkit::{Session, SpawnOptions};

// ── harness ──

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

const WAIT: Duration = Duration::from_secs(10);

/// A private registry holding one session, killed and removed on drop.
struct Sandbox {
    bin: String,
    root: PathBuf,
    name: String,
}

impl Sandbox {
    fn new() -> Sandbox {
        use_local_pty();
        // Short: a session socket path has to fit 104 bytes.
        let root = std::env::temp_dir().join(format!("pq-{}", random_id()));
        std::fs::create_dir_all(&root).expect("temp root");
        Sandbox {
            bin: pty_bin(),
            root,
            name: random_id(),
        }
    }

    fn dir(&self) -> String {
        self.root.display().to_string()
    }

    /// `pty run -d` a 24x80 session running `sh -c script`.
    fn run(&self, script: &str) {
        spawn_daemon(
            &self.bin,
            &self.root,
            &self.name,
            "sh",
            &["-c", script],
            24,
            80,
            None,
            &[],
        )
        .expect("pty run -d");
    }

    /// Release a child gated on `name` (see [`gate`]).
    fn release(&self, name: &str) {
        std::fs::write(self.root.join(name), b"").expect("gate file");
    }

    fn peek(&self) -> String {
        let out = std::process::Command::new(&self.bin)
            .args(["peek", "--plain", &self.name])
            .env("PTY_ROOT", &self.root)
            .output();
        match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
            Err(e) => format!("<peek failed: {e}>"),
        }
    }

    /// Wait for the child to publish `name` in the sandbox and return it.
    fn wait_file(&self, name: &str) -> Vec<u8> {
        self.wait_file_while(name, || {})
    }

    /// Like [`Sandbox::wait_file`], running `tick` between polls (a client
    /// that has to keep reading and answering while the child waits).
    fn wait_file_while(&self, name: &str, mut tick: impl FnMut()) -> Vec<u8> {
        let path = self.root.join(name);
        let deadline = Instant::now() + WAIT;
        loop {
            tick();
            if let Ok(bytes) = std::fs::read(&path) {
                return bytes;
            }
            assert!(
                Instant::now() < deadline,
                "the child never wrote {name}; its screen:\n{}",
                self.peek()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// `pty attach` running inside a spawn-mode session: that session's
    /// libghostty terminal is the client's real terminal, and it answers any
    /// query that reaches it, the way the user's terminal would.
    fn attach_in_terminal(&self) -> Session {
        Session::spawn(
            &self.bin,
            &["attach", &self.name],
            SpawnOptions {
                rows: Some(24),
                cols: Some(80),
                env: vec![("PTY_ROOT".into(), self.dir())],
                ..Default::default()
            },
        )
        .expect("pty attach in a terminal")
    }

    /// A protocol-level client that answers nothing: bytes it is sent arrive
    /// on the receiver.
    fn raw_client(&self) -> (std::os::unix::net::UnixStream, mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = mpsc::channel();
        let (socket, _state) = connect(&self.root, &self.name, 24, 80, tx).expect("connect");
        (socket, rx)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        for verb in ["kill", "rm"] {
            let _ = std::process::Command::new(&self.bin)
                .args([verb, &self.name])
                .env("PTY_ROOT", &self.root)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Shell that prints `READY-<name>` and waits for the test to release it.
fn gate(sb: &Sandbox, name: &str) -> String {
    let d = sb.dir();
    format!("printf 'READY-{name}\\n'; while [ ! -e '{d}/{name}' ]; do sleep 0.02; done; ")
}

/// A child that sends `query` (printf syntax) from a raw tty and writes every
/// byte it gets back to `<root>/reply`. With `gated`, it first waits for
/// [`Sandbox::release`]`("go")`.
fn probe(sb: &Sandbox, query: &str, gated: bool) -> String {
    let d = sb.dir();
    let wait = if gated { gate(sb, "go") } else { String::new() };
    format!(
        "{wait}stty raw -echo min 0 time 10; printf '{query}'; cat > '{d}/reply.tmp'; \
         mv '{d}/reply.tmp' '{d}/reply'; stty sane; printf 'PROBE-DONE\\n'; exec sleep 60"
    )
}

/// A terminal the test plays itself, byte for byte, running the real
/// `pty attach`: it records everything the client writes to it and answers
/// the queries it is told to, the way a particular host terminal would.
struct FakeTerminal {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    seen: Arc<Mutex<Vec<u8>>>,
}

impl FakeTerminal {
    fn attach(sb: &Sandbox, size: PtySize, answers: &[(&[u8], &[u8])]) -> FakeTerminal {
        let pair = portable_pty::native_pty_system()
            .openpty(size)
            .expect("open a pty");
        let mut cmd = CommandBuilder::new(&sb.bin);
        cmd.args(["attach", &sb.name]);
        cmd.env("PTY_ROOT", sb.dir());
        cmd.env("TERM", "xterm-256color");
        for key in ["PTY_SESSION", "PTY_SESSION_GENERATION", "PTY_SESSION_DIR"] {
            cmd.env_remove(key);
        }
        let child = pair.slave.spawn_command(cmd).expect("spawn pty attach");
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().expect("reader");
        let writer = Arc::new(Mutex::new(pair.master.take_writer().expect("writer")));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let answers: Vec<(Vec<u8>, Vec<u8>)> =
            answers.iter().map(|(q, a)| (q.to_vec(), a.to_vec())).collect();
        {
            let (seen, writer) = (seen.clone(), writer.clone());
            std::thread::spawn(move || {
                let mut answered = vec![0usize; answers.len()];
                let mut buf = vec![0u8; 65536];
                loop {
                    let n = match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let mut s = seen.lock().unwrap();
                    s.extend_from_slice(&buf[..n]);
                    for (i, (query, answer)) in answers.iter().enumerate() {
                        let asked = count(&s, query);
                        while answered[i] < asked {
                            let mut w = writer.lock().unwrap();
                            let _ = w.write_all(answer);
                            let _ = w.flush();
                            answered[i] += 1;
                        }
                    }
                }
            });
        }
        FakeTerminal {
            child,
            _master: pair.master,
            writer,
            seen,
        }
    }

    fn seen(&self) -> Vec<u8> {
        self.seen.lock().unwrap().clone()
    }

    fn wait_seen(&self, needle: &[u8]) {
        let deadline = Instant::now() + WAIT;
        while count(&self.seen(), needle) == 0 {
            assert!(
                Instant::now() < deadline,
                "the terminal never showed {:?}; it got {} bytes",
                String::from_utf8_lossy(needle),
                self.seen().len()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn type_bytes(&self, bytes: &[u8]) {
        let mut w = self.writer.lock().unwrap();
        let _ = w.write_all(bytes);
        let _ = w.flush();
    }
}

impl Drop for FakeTerminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn size(rows: u16, cols: u16, pixel_width: u16, pixel_height: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width,
        pixel_height,
    }
}

// ── reading replies ──

fn count(hay: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || hay.len() < needle.len() {
        return 0;
    }
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Split terminal bytes into escape sequences (CSI, OSC, DCS, or a
/// two-byte escape) and runs of anything else.
fn tokens(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b || i + 1 >= bytes.len() {
            let start = i;
            while i < bytes.len() && bytes[i] != 0x1b {
                i += 1;
            }
            if i == start {
                i += 1;
            }
            out.push(bytes[start..i].to_vec());
            continue;
        }
        let start = i;
        match bytes[i + 1] {
            b'[' => {
                i += 2;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                i = (i + 1).min(bytes.len());
            }
            b']' | b'P' => {
                i += 2;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            _ => i += 2,
        }
        out.push(bytes[start..i].to_vec());
    }
    out
}

/// The payloads of every OSC reply: `ESC ] payload (BEL | ST)`.
fn osc_payloads(bytes: &[u8]) -> Vec<String> {
    tokens(bytes)
        .into_iter()
        .filter(|t| t.starts_with(b"\x1b]"))
        .map(|t| {
            let end = if t.ends_with(b"\x07") {
                t.len() - 1
            } else if t.ends_with(b"\x1b\\") {
                t.len() - 2
            } else {
                t.len()
            };
            String::from_utf8_lossy(&t[2..end]).into_owned()
        })
        .collect()
}

/// `rgb:h/h/h` with one to four hex digits per channel (xterm's form).
fn is_rgb_spec(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("rgb:") else {
        return false;
    };
    let parts: Vec<&str> = rest.split('/').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| (1..=4).contains(&p.len()) && p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn shown(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace('\x1b', "ESC").replace('\x07', "BEL")
}

/// Assert that `reply` holds exactly one well-formed colour report for each
/// `prefix` (`"11;"`, `"4;1;"`, …) and nothing else.
fn assert_one_colour_reply_each(reply: &[u8], prefixes: &[&str]) {
    let payloads = osc_payloads(reply);
    for prefix in prefixes {
        let matching: Vec<&String> = payloads.iter().filter(|p| p.starts_with(prefix)).collect();
        assert_eq!(
            matching.len(),
            1,
            "OSC {prefix}? should get exactly one reply; the child got {}",
            shown(reply)
        );
        assert!(
            is_rgb_spec(&matching[0][prefix.len()..]),
            "OSC {prefix}? reply is not an rgb: spec: {}",
            shown(reply)
        );
    }
    assert_eq!(
        payloads.len(),
        prefixes.len(),
        "unexpected extra replies: {}",
        shown(reply)
    );
    let stray: Vec<Vec<u8>> = tokens(reply)
        .into_iter()
        .filter(|t| !t.starts_with(b"\x1b]"))
        .collect();
    assert!(stray.is_empty(), "stray bytes around the replies: {}", shown(reply));
}

/// Send `query` from a child and collect what comes back while a client
/// whose terminal answers what it sees is attached.
fn probe_with_client_terminal(sb: &Sandbox, query: &str) -> Vec<u8> {
    sb.run(&probe(sb, query, true));
    let mut term = sb.attach_in_terminal();
    term.wait_for_text("READY-go", WAIT.as_millis() as u64)
        .expect("the client is attached");
    sb.release("go");
    sb.wait_file_while("reply", || {
        // Draining the client's output is what lets its terminal answer.
        let _ = term.screenshot();
    })
}

// ── colour queries get one reply, attached or not ──

const COLOUR_QUERIES: &str = "\\033]10;?\\033\\\\\\033]11;?\\007\\033]12;?\\033\\\\\\033]4;0;?\\007\\033]4;1;?\\033\\\\";

#[test]
fn colour_queries_get_exactly_one_well_formed_reply_with_no_client_attached() {
    let sb = Sandbox::new();
    sb.run(&probe(&sb, COLOUR_QUERIES, false));
    let reply = sb.wait_file("reply");
    assert_one_colour_reply_each(&reply, &["10;", "11;", "12;", "4;0;", "4;1;"]);
}

#[test]
fn foreground_background_and_palette_queries_get_one_reply_with_a_client_attached() {
    let sb = Sandbox::new();
    let reply = probe_with_client_terminal(
        &sb,
        "\\033]10;?\\033\\\\\\033]11;?\\007\\033]4;0;?\\007\\033]4;1;?\\033\\\\",
    );
    assert_one_colour_reply_each(&reply, &["10;", "11;", "4;0;", "4;1;"]);
}

#[test]
#[ignore = "fails on main: OSC 12 is answered by the daemon and also forwarded to the client, whose terminal answers it again"]
fn cursor_colour_query_gets_one_reply_with_a_client_attached() {
    let sb = Sandbox::new();
    let reply = probe_with_client_terminal(&sb, "\\033]12;?\\033\\\\");
    assert_one_colour_reply_each(&reply, &["12;"]);
}

#[test]
#[ignore = "fails on main: a multi-slot OSC 4 query is answered for its first slot only"]
fn every_slot_of_a_multi_slot_palette_query_gets_a_reply() {
    let sb = Sandbox::new();
    sb.run(&probe(&sb, "\\033]4;1;?;2;?\\007", false));
    let reply = sb.wait_file("reply");
    // xterm answers each slot with its own OSC 4 report.
    let slots: Vec<String> = osc_payloads(&reply)
        .into_iter()
        .filter_map(|p| p.strip_prefix("4;").map(|r| r.split(';').next().unwrap_or("").to_string()))
        .collect();
    assert_eq!(slots, vec!["1".to_string(), "2".to_string()], "{}", shown(&reply));
}

// ── colour replies describe the terminal the session is shown on ──

/// A light terminal: white background, dark text, light colour scheme.
const LIGHT_TERMINAL: &[(&[u8], &[u8])] = &[
    (b"\x1b]11;?", b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\"),
    (b"\x1b]10;?", b"\x1b]10;rgb:1111/1111/1111\x1b\\"),
    (b"\x1b[?996n", b"\x1b[?997;2n"),
];

#[test]
#[ignore = "fails on main: OSC 11 is always answered rgb:0000/0000/0000 (a fixed constant), even from a light terminal, while CSI ?996n says light"]
fn background_and_scheme_replies_report_the_light_attached_terminal() {
    let sb = Sandbox::new();
    sb.run(&probe(&sb, "\\033]11;?\\033\\\\\\033[?996n", true));
    let term = FakeTerminal::attach(&sb, size(24, 80, 0, 0), LIGHT_TERMINAL);
    term.wait_seen(b"READY-go");
    sb.release("go");
    let reply = sb.wait_file("reply");
    let background: Vec<String> = osc_payloads(&reply)
        .into_iter()
        .filter(|p| p.starts_with("11;"))
        .collect();
    let scheme = (count(&reply, b"\x1b[?997;1n"), count(&reply, b"\x1b[?997;2n"));
    assert_eq!(
        (background.as_slice(), scheme),
        (&["11;rgb:ffff/ffff/ffff".to_string()][..], (0, 1)),
        "(OSC 11 replies, (dark, light) scheme reports) from a light terminal: {}",
        shown(&reply)
    );
}

#[test]
#[ignore = "fails on main: OSC 11 is answered rgb:0000/0000/0000 even after the child set the background"]
fn background_query_reports_the_background_the_child_set() {
    let sb = Sandbox::new();
    sb.run(&probe(&sb, "\\033]11;rgb:ff/00/00\\007\\033]11;?\\033\\\\", false));
    let reply = sb.wait_file("reply");
    let payloads = osc_payloads(&reply);
    assert_eq!(
        payloads,
        vec!["11;rgb:ffff/0000/0000".to_string()],
        "reply after OSC 11 set red: {}",
        shown(&reply)
    );
}

#[test]
#[ignore = "fails on main: CSI ? 996 n is not answered by the daemon; with no client attached the child gets nothing"]
fn colour_scheme_query_gets_a_reply_with_no_client_attached() {
    let sb = Sandbox::new();
    sb.run(&probe(&sb, "\\033[?996n", false));
    let reply = sb.wait_file("reply");
    let answers = count(&reply, b"\x1b[?997;1n") + count(&reply, b"\x1b[?997;2n");
    assert_eq!(answers, 1, "one colour-scheme report expected: {}", shown(&reply));
}

/// Learning the attached terminal's colours may cost a bounded number of
/// queries to it, but focus changes alone must not set off new sweeps.
#[test]
fn focus_changes_do_not_send_colour_queries_to_the_client_terminal() {
    let sb = Sandbox::new();
    let d = sb.dir();
    sb.run(&format!(
        "stty raw -echo; printf 'READY-FOCUS\\n'; cat > '{d}/input'"
    ));
    let term = FakeTerminal::attach(&sb, size(24, 80, 0, 0), LIGHT_TERMINAL);
    term.wait_seen(b"READY-FOCUS");
    std::thread::sleep(Duration::from_millis(300));
    let colour_queries = |b: &[u8]| {
        count(b, b"\x1b]4;") + count(b, b"\x1b]10;?") + count(b, b"\x1b]11;?") + count(b, b"\x1b[?996n")
    };
    let before = colour_queries(&term.seen());
    for _ in 0..5 {
        term.type_bytes(b"\x1b[O");
        term.type_bytes(b"\x1b[I");
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(500));
    let after = colour_queries(&term.seen());
    assert_eq!(
        after, before,
        "focus changes made the runtime query the client's colours again"
    );
    assert!(before <= 260, "an attach sent {before} colour queries");
}

// ── replies reach only the asker, once, in order ──

#[test]
fn replies_arrive_once_each_and_in_request_order_with_a_client_attached() {
    let sb = Sandbox::new();
    let reply = probe_with_client_terminal(
        &sb,
        "\\033]11;?\\033\\\\\\033[6n\\033[c\\033]10;?\\007",
    );
    let osc11 = find(&reply, b"\x1b]11;").expect("OSC 11 reply");
    let cpr = tokens(&reply)
        .iter()
        .position(|t| t.starts_with(b"\x1b[") && t.ends_with(b"R"))
        .expect("cursor position report");
    let cpr = find(&reply, &tokens(&reply)[cpr]).unwrap();
    let da1 = find(&reply, b"\x1b[?62;22c").expect("DA1 reply");
    let osc10 = find(&reply, b"\x1b]10;").expect("OSC 10 reply");
    assert!(
        osc11 < cpr && cpr < da1 && da1 < osc10,
        "replies out of request order: {}",
        shown(&reply)
    );
    assert_eq!(count(&reply, b"\x1b]11;"), 1, "{}", shown(&reply));
    assert_eq!(count(&reply, b"\x1b]10;"), 1, "{}", shown(&reply));
    assert_eq!(count(&reply, b"\x1b[?62;22c"), 1, "{}", shown(&reply));
    let cprs = tokens(&reply)
        .iter()
        .filter(|t| t.starts_with(b"\x1b[") && t.ends_with(b"R"))
        .count();
    assert_eq!(cprs, 1, "{}", shown(&reply));
}

#[test]
#[ignore = "fails on main: DECRQM, DSR 5n and the kitty keyboard query are answered by the daemon and forwarded to the client, whose terminal answers them again"]
fn state_queries_are_not_answered_a_second_time_by_the_client_terminal() {
    let sb = Sandbox::new();
    let reply = probe_with_client_terminal(&sb, "\\033[?2004$p\\033[5n\\033[?u");
    let counts = [
        ("DECRQM ?2004", count(&reply, b"$y")),
        ("DSR 5n", count(&reply, b"\x1b[0n")),
        ("kitty keyboard query", count(&reply, b"\x1b[?0u")),
    ];
    let wrong: Vec<_> = counts.iter().filter(|(_, n)| *n != 1).collect();
    assert!(
        wrong.is_empty(),
        "each query needs exactly one reply, got {wrong:?}: {}",
        shown(&reply)
    );
}

// ── device attributes ──

#[test]
fn device_queries_get_one_reply_each_with_no_client_attached() {
    let sb = Sandbox::new();
    sb.run(&probe(
        &sb,
        "\\033[c\\033[>c\\033[6n\\033[>0q\\033[?2004$p",
        false,
    ));
    let reply = sb.wait_file("reply");
    assert_eq!(count(&reply, b"\x1b[?62;22c"), 1, "DA1: {}", shown(&reply));
    assert_eq!(count(&reply, b"\x1b[>0;382;0c"), 1, "DA2: {}", shown(&reply));
    assert_eq!(count(&reply, b"\x1b[1;1R"), 1, "DSR: {}", shown(&reply));
    assert_eq!(count(&reply, b"\x1bP>|pty(0.8)\x1b\\"), 1, "XTVERSION: {}", shown(&reply));
    assert_eq!(count(&reply, b"\x1b[?2004;2$y"), 1, "DECRQM: {}", shown(&reply));
}

#[test]
#[ignore = "fails on main: XTGETTCAP and DECRQSS are passed through to clients unanswered; with no client attached the child gets nothing"]
fn capability_and_setting_queries_get_a_reply_with_no_client_attached() {
    let sb = Sandbox::new();
    // XTGETTCAP for TN and Smulx, then DECRQSS for SGR.
    sb.run(&probe(
        &sb,
        "\\033P+q544e\\033\\\\\\033P+q536d756c78\\033\\\\\\033P$qm\\033\\\\",
        false,
    ));
    let reply = sb.wait_file("reply");
    let xtgettcap = count(&reply, b"\x1bP1+r") + count(&reply, b"\x1bP0+r");
    let decrqss = count(&reply, b"\x1bP1$r") + count(&reply, b"\x1bP0$r");
    assert_eq!(
        (xtgettcap, decrqss),
        (2, 1),
        "(XTGETTCAP replies, DECRQSS replies): {}",
        shown(&reply)
    );
}

// ── pixel and cell geometry ──

#[test]
#[ignore = "fails on main: CSI 14/16/18 t are not answered by the daemon; with no client attached the child gets nothing"]
fn size_queries_get_a_reply_with_no_client_attached() {
    let sb = Sandbox::new();
    sb.run(&probe(&sb, "\\033[14t\\033[16t\\033[18t", false));
    let reply = sb.wait_file("reply");
    let toks: Vec<String> = tokens(&reply).iter().map(|t| shown(t)).collect();
    let starts = |p: &str| toks.iter().filter(|t| t.starts_with(p) && t.ends_with('t')).count();
    assert_eq!(
        (starts("ESC[4;"), starts("ESC[6;"), count(&reply, b"\x1b[8;24;80t")),
        (1, 1, 1),
        "(CSI 14t, CSI 16t, CSI 18t) replies: {}",
        shown(&reply)
    );
}

#[test]
#[ignore = "fails on main: the daemon adopts a client's declared cell size but never answers CSI 14t/16t with it"]
fn pixel_size_queries_report_the_cell_size_a_client_declared() {
    let sb = Sandbox::new();
    sb.run(&probe(&sb, "\\033[16t\\033[14t", true));
    let (mut socket, rx) = sb.raw_client();
    // A client drawing 10x21-pixel cells says so.
    socket
        .write_all(&encode_resize_with_cell(24, 80, 10, 21))
        .expect("declare the cell size");
    let deadline = Instant::now() + WAIT;
    let mut got = Vec::new();
    while count(&got, b"READY-go") == 0 {
        assert!(Instant::now() < deadline, "client never saw the child");
        if let Ok(b) = rx.recv_timeout(Duration::from_millis(50)) {
            got.extend(b);
        }
    }
    sb.release("go");
    let reply = sb.wait_file("reply");
    assert_eq!(
        (count(&reply, b"\x1b[6;21;10t"), count(&reply, b"\x1b[4;504;800t")),
        (1, 1),
        "(CSI 16t, CSI 14t) replies for 10x21 cells on 24x80: {}",
        shown(&reply)
    );
}

#[test]
#[ignore = "fails on main: the pty's winsize always carries 0x0 pixels, whatever the attached terminal reports"]
fn the_childs_winsize_carries_the_attached_terminals_pixel_size() {
    let sb = Sandbox::new();
    let d = sb.dir();
    // Wait for the attached terminal's 30x100 grid to arrive, then record
    // the winsize the child sees.
    let script = format!(
        "{gate}python3 -c 'import fcntl,os,termios,struct,sys,time\n\
         end=time.time()+5\n\
         while True:\n\
         \x20   r,c,x,y=struct.unpack(\"HHHH\",fcntl.ioctl(0,termios.TIOCGWINSZ,bytes(8)))\n\
         \x20   if r==30 or time.time()>end: break\n\
         \x20   time.sleep(0.02)\n\
         p=sys.argv[1]\n\
         open(p+\".tmp\",\"w\").write(\"%d %d %d %d\"%(r,c,x,y))\n\
         os.rename(p+\".tmp\",p)' '{d}/winsize'; exec sleep 60",
        gate = gate(&sb, "go"),
    );
    sb.run(&script);
    // A 30x100 terminal whose cells are 10x21 pixels.
    let term = FakeTerminal::attach(&sb, size(30, 100, 1000, 630), &[]);
    term.wait_seen(b"READY-go");
    sb.release("go");
    let winsize = String::from_utf8(sb.wait_file("winsize")).unwrap();
    assert_eq!(winsize, "30 100 1000 630", "rows cols xpixel ypixel");
}

// ── colours the child sets ──

fn rgb(r: u8, g: u8, b: u8) -> Option<RgbColor> {
    Some(RgbColor { r, g, b })
}

const COLOUR_SETS: &str = "\\033]11;rgb:ff/00/00\\007\\033]10;rgb:00/ff/00\\007\\033]12;rgb:00/00/ff\\007\\033]4;1;rgb:12/34/56\\007";

#[test]
fn colour_sets_and_resets_reach_an_attached_client() {
    let sb = Sandbox::new();
    let script = format!(
        "{go}printf '{COLOUR_SETS}SET-DONE\\n'; {reset}printf '\\033]111\\007\\033]110\\007\\033]112\\007\\033]104;1\\007RESET-DONE\\n'; exec sleep 60",
        go = gate(&sb, "go"),
        reset = gate(&sb, "reset"),
    );
    sb.run(&script);
    let mut term = sb.attach_in_terminal();
    term.wait_for_text("READY-go", WAIT.as_millis() as u64).expect("attached");
    let default_fg = term.actor().terminal().default_fg_color().unwrap();
    let default_bg = term.actor().terminal().default_bg_color().unwrap();
    let default_one = term.actor().terminal().default_color_palette().unwrap().0[1];
    sb.release("go");
    term.wait_for_text("SET-DONE", WAIT.as_millis() as u64).expect("sets");
    let t = term.actor().terminal();
    assert_eq!(t.bg_color().unwrap(), rgb(0xff, 0, 0), "OSC 11 set");
    assert_eq!(t.fg_color().unwrap(), rgb(0, 0xff, 0), "OSC 10 set");
    assert_eq!(t.cursor_color().unwrap(), rgb(0, 0, 0xff), "OSC 12 set");
    assert_eq!(t.color_palette().unwrap().0[1], RgbColor { r: 0x12, g: 0x34, b: 0x56 }, "OSC 4 set");

    sb.release("reset");
    term.wait_for_text("RESET-DONE", WAIT.as_millis() as u64).expect("resets");
    let t = term.actor().terminal();
    assert_eq!(t.bg_color().unwrap(), default_bg, "OSC 111 reset");
    assert_eq!(t.fg_color().unwrap(), default_fg, "OSC 110 reset");
    assert_eq!(t.color_palette().unwrap().0[1], default_one, "OSC 104 reset");
}

#[test]
#[ignore = "fails on main: the attach replay carries no OSC 10/11/12/4 state, so a reattaching client loses the child's colours"]
fn colour_sets_survive_a_reattach() {
    let sb = Sandbox::new();
    sb.run(&format!("printf '{COLOUR_SETS}SET-DONE\\n'; exec sleep 60"));
    // Attach only after the sets: everything the client has comes from the
    // replay.
    let deadline = Instant::now() + WAIT;
    while !sb.peek().contains("SET-DONE") {
        assert!(Instant::now() < deadline, "the session never printed");
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut term = sb.attach_in_terminal();
    term.wait_for_text("SET-DONE", WAIT.as_millis() as u64).expect("replay");
    let t = term.actor().terminal();
    let got = (
        t.bg_color().unwrap(),
        t.fg_color().unwrap(),
        t.cursor_color().unwrap(),
        t.color_palette().unwrap().0[1],
    );
    assert_eq!(
        got,
        (rgb(0xff, 0, 0), rgb(0, 0xff, 0), rgb(0, 0, 0xff), RgbColor { r: 0x12, g: 0x34, b: 0x56 }),
        "(bg, fg, cursor, palette[1]) after reattach"
    );
}

// ── colour fidelity ──

const FIXTURE: &str = "\\033[31mRED\\033[0m \\033[91mBRIGHT\\033[0m \\033[7mINVERSE\\033[0m \\033[38;2;10;20;30mTRUE\\033[0m \\033[48;5;124mBGIDX\\033[0m PLAIN\\n";

fn cell_of(term: &Session, word: &str) -> CellSnap {
    let grid = term.actor().snapshot(0);
    for row in &grid.rows {
        let text: String = row.iter().map(|c| c.text.as_str()).collect();
        if let Some(byte) = text.find(word) {
            let col = text[..byte].chars().count();
            return row[col].clone();
        }
    }
    panic!("{word} not on the client's screen:\n{}", grid.text());
}

fn assert_fixture_colours(term: &Session, how: &str) {
    let c = cell_of(term, "RED");
    assert_eq!((c.fg, c.bg), (ColorSnap::Indexed(1), ColorSnap::Default), "{how}: SGR 31");
    let c = cell_of(term, "BRIGHT");
    assert_eq!((c.fg, c.bg), (ColorSnap::Indexed(9), ColorSnap::Default), "{how}: SGR 91");
    let c = cell_of(term, "INVERSE");
    assert!(c.inverse, "{how}: SGR 7 lost");
    assert_eq!((c.fg, c.bg), (ColorSnap::Default, ColorSnap::Default), "{how}: reverse over defaults");
    let c = cell_of(term, "TRUE");
    assert_eq!((c.fg, c.bg), (ColorSnap::Rgb(10, 20, 30), ColorSnap::Default), "{how}: truecolor");
    let c = cell_of(term, "BGIDX");
    assert_eq!((c.fg, c.bg), (ColorSnap::Default, ColorSnap::Indexed(124)), "{how}: SGR 48;5");
    let c = cell_of(term, "PLAIN");
    assert_eq!((c.fg, c.bg, c.inverse), (ColorSnap::Default, ColorSnap::Default, false), "{how}: defaults");
    // Cells never written stay default too: no background painted in.
    let grid = term.actor().snapshot(0);
    let blank = &grid.rows[grid.rows.len() - 1][grid.cols as usize - 1];
    assert_eq!((blank.fg, blank.bg), (ColorSnap::Default, ColorSnap::Default), "{how}: blank cell");
}

#[test]
fn palette_truecolor_reverse_and_default_colours_reach_clients_live_and_after_reattach() {
    let sb = Sandbox::new();
    sb.run(&format!(
        "{go}printf '{FIXTURE}'; printf 'FIXTURE-DONE\\n'; exec sleep 60",
        go = gate(&sb, "go")
    ));
    let mut live = sb.attach_in_terminal();
    live.wait_for_text("READY-go", WAIT.as_millis() as u64).expect("attached");
    sb.release("go");
    live.wait_for_text("FIXTURE-DONE", WAIT.as_millis() as u64).expect("live");
    assert_fixture_colours(&live, "live");

    // A second client arriving later gets the same colours from the replay.
    let mut late = sb.attach_in_terminal();
    late.wait_for_text("FIXTURE-DONE", WAIT.as_millis() as u64).expect("replay");
    assert_fixture_colours(&late, "replay");
}

// ── clipboard ──

#[test]
fn clipboard_writes_reach_every_attached_client_intact_at_any_reasonable_size() {
    let sb = Sandbox::new();
    let d = sb.dir();
    // 197 000 bytes of text: about 263 KB of base64 in one OSC 52.
    sb.run(&format!(
        "head -c 197000 /dev/zero | tr '\\000' x | base64 -w0 > '{d}/big.b64'; \
         {go}printf '\\033]52;c;%s\\007' \"$(printf PROBE-OK | base64)\"; \
         printf '\\033]52;c;'; cat '{d}/big.b64'; printf '\\007'; printf 'CLIP-DONE\\n'; exec sleep 60",
        go = gate(&sb, "go")
    ));
    let term = FakeTerminal::attach(&sb, size(24, 80, 0, 0), &[]);
    let (_socket, rx) = sb.raw_client();
    term.wait_seen(b"READY-go");
    sb.release("go");
    term.wait_seen(b"CLIP-DONE");

    let big = std::fs::read(sb.root.join("big.b64")).expect("payload");
    assert_eq!(big.len(), 262_668, "base64 of 197000 bytes");
    let mut small_write = b"\x1b]52;c;".to_vec();
    small_write.extend_from_slice(b"UFJPQkUtT0s=\x07");
    let mut big_write = b"\x1b]52;c;".to_vec();
    big_write.extend_from_slice(&big);
    big_write.push(0x07);

    let seen = term.seen();
    assert_eq!(count(&seen, &small_write), 1, "pty attach: small OSC 52 write");
    assert_eq!(count(&seen, &big_write), 1, "pty attach: large OSC 52 write");

    let deadline = Instant::now() + WAIT;
    let mut other = Vec::new();
    while count(&other, b"CLIP-DONE") == 0 {
        assert!(Instant::now() < deadline, "second client never saw the writes");
        if let Ok(b) = rx.recv_timeout(Duration::from_millis(50)) {
            other.extend(b);
        }
    }
    assert_eq!(count(&other, &small_write), 1, "second client: small OSC 52 write");
    assert_eq!(count(&other, &big_write), 1, "second client: large OSC 52 write");
}

#[test]
#[ignore = "fails on main: an OSC 52 read is passed through to clients unanswered; with no client attached the child waits forever"]
fn clipboard_read_query_gets_a_reply_with_no_client_attached() {
    let sb = Sandbox::new();
    sb.run(&probe(&sb, "\\033]52;c;?\\007", false));
    let reply = sb.wait_file("reply");
    let replies: Vec<String> = osc_payloads(&reply)
        .into_iter()
        .filter(|p| p.starts_with("52;"))
        .collect();
    assert_eq!(replies.len(), 1, "one OSC 52 reply (the clipboard, or empty): {}", shown(&reply));
}
