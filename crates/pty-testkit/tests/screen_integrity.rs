//! Screen integrity: one picture, however it is looked at.
//!
//! A session's terminal is the truth. `pty peek` reads it, `pty stats` reports
//! its cursor, and every client shows it: one that was attached while the
//! child wrote, one that attaches later and is built from the replay, and one
//! that detaches and comes back. These tests hold the runtime to that: wide
//! and clustered graphemes take the same cells everywhere, UTF-8 survives
//! being split across writes and frames, the picture does not depend on how
//! the child's output was chunked, the alternate screen gives the primary
//! screen back intact, plain reads join what the terminal soft-wrapped, a
//! read neither pokes the child nor disturbs a client, and a replay restores
//! the screen, the cursor and the modes exactly once.
//!
//! The "outer terminal" in these tests is a spawn-mode session running
//! `pty attach`: its libghostty terminal is what a person would be looking at.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Once;
use std::time::{Duration, Instant};

use libghostty_vt::terminal::Mode;
use pty_core::protocol::{MessageType, Packet, PacketReader, encode_attach, encode_data, encode_peek};
use pty_terminal::{CellGrid, Range, TerminalActor, Wide};
use pty_testkit::{Session, SpawnOptions};

// ── harness ──

/// Point the testkit at the `pty` built from this workspace.
fn use_local_pty() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // CARGO_BIN_EXE_ is only set for the crate that owns the binary, so
        // find it beside this test binary instead.
        let mut dir = std::env::current_exe().expect("test binary path");
        dir.pop(); // deps/
        dir.pop(); // debug/
        let bin = dir.join("pty");
        if bin.exists() {
            // SAFETY: set once, before any session is started, and never
            // changed afterwards.
            unsafe { std::env::set_var("PTY_BIN", &bin) };
        }
    });
}

fn pty_bin() -> String {
    pty_testkit::server::pty_bin()
}

const WAIT: Duration = Duration::from_secs(10);

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(start.elapsed() < WAIT, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A session in a registry of its own, created with `pty run -d` so that no
/// client is attached until a test attaches one. Killed and removed on drop.
struct Daemon {
    root: PathBuf,
    name: String,
}

impl Daemon {
    /// A registry and a session name; nothing runs until [`Daemon::run`].
    fn new() -> Daemon {
        use_local_pty();
        // Short: a session socket path has to fit 104 bytes.
        let root = std::env::temp_dir().join(format!("si-{}", pty_testkit::server::random_id()));
        std::fs::create_dir_all(&root).expect("create a registry");
        Daemon {
            root,
            name: pty_testkit::server::random_id(),
        }
    }

    fn run(&self, rows: u16, cols: u16, command: &str, args: &[&str]) {
        pty_testkit::server::spawn_daemon(&pty_bin(), &self.root, &self.name, command, args, rows, cols, None, &[])
            .expect("start a session");
        wait_until("the session socket", || self.socket().exists());
    }

    /// `sh -c <script>`.
    fn run_sh(&self, rows: u16, cols: u16, script: &str) {
        self.run(rows, cols, "sh", &["-c", script]);
    }

    fn socket(&self) -> PathBuf {
        self.root.join(format!("{}.sock", self.name))
    }

    /// A file in this session's registry directory (gates, fixtures, logs).
    fn file(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// A shell fragment that blocks until [`Daemon::open`] is called.
    fn gate(&self, name: &str) -> String {
        format!("until [ -e '{}' ]; do sleep 0.02; done; ", self.file(name).display())
    }

    fn open(&self, name: &str) {
        std::fs::write(self.file(name), b"go").expect("open a gate");
    }

    fn pty(&self, args: &[&str]) -> Output {
        Command::new(pty_bin())
            .args(args)
            .env("PTY_ROOT", &self.root)
            .env_remove("PTY_SESSION")
            .env_remove("PTY_SESSION_GENERATION")
            .env_remove("PTY_SESSION_DIR")
            .output()
            .expect("run pty")
    }

    fn peek(&self, flags: &[&str]) -> String {
        let mut args = vec!["peek"];
        args.extend_from_slice(flags);
        args.push(&self.name);
        let out = self.pty(&args);
        assert!(
            out.status.success(),
            "pty {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("peek output is UTF-8")
    }

    /// `pty peek --plain`: the viewport as text.
    fn plain(&self) -> String {
        self.peek(&["--plain"])
    }

    /// `pty peek --plain --full`: history and viewport as text.
    fn plain_full(&self) -> String {
        self.peek(&["--plain", "--full"])
    }

    fn stats(&self) -> String {
        let out = self.pty(&["stats", "--json", &self.name]);
        assert!(out.status.success(), "pty stats failed");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// `(x, y)` of the cursor, as `pty stats` reports it.
    fn cursor(&self) -> (u16, u16) {
        let s = self.stats();
        (
            json_number(&s, "cursorX").expect("cursorX") as u16,
            json_number(&s, "cursorY").expect("cursorY") as u16,
        )
    }

    fn child_pid(&self) -> i32 {
        let s = self.stats();
        let process = &s[s.find("\"process\"").expect("process")..];
        json_number(process, "pid").expect("child pid") as i32
    }

    fn attached(&self) -> i64 {
        json_number(&self.stats(), "attached").expect("attached count")
    }

    /// `pty attach` in a terminal of its own, at `rows` x `cols`.
    fn attach_outer(&self, rows: u16, cols: u16) -> Session {
        self.outer(rows, cols, &format!("exec '{}' attach {}", pty_bin(), self.name))
    }

    /// A spawn-mode terminal running `script`, with this registry in scope.
    fn outer(&self, rows: u16, cols: u16, script: &str) -> Session {
        Session::spawn(
            "sh",
            &["-c", script],
            SpawnOptions {
                rows: Some(rows),
                cols: Some(cols),
                env: vec![("PTY_ROOT".into(), self.root.display().to_string())],
                ..Default::default()
            },
        )
        .expect("spawn an outer terminal")
    }

    /// A testkit client on the socket: a fresh terminal built from the
    /// replay, then live DATA. Closing it leaves the session running.
    fn client(&self, rows: u16, cols: u16) -> Session {
        Session::connect(&self.name, rows, cols, self.root.clone()).expect("connect a client")
    }

    fn raw(&self) -> RawClient {
        RawClient::connect(&self.socket())
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        for verb in ["kill", "rm"] {
            let _ = Command::new(pty_bin())
                .args([verb, &self.name])
                .env("PTY_ROOT", &self.root)
                .env_remove("PTY_SESSION")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn json_number(json: &str, key: &str) -> Option<i64> {
    let at = json.find(&format!("\"{key}\":"))? + key.len() + 3;
    let digits: String = json[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    digits.parse().ok()
}

/// A socket client that keeps every frame it receives, in order.
struct RawClient {
    sock: UnixStream,
    reader: PacketReader,
    packets: Vec<Packet>,
}

impl RawClient {
    fn connect(path: &Path) -> RawClient {
        let sock = UnixStream::connect(path).expect("connect to the session socket");
        sock.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        RawClient {
            sock,
            reader: PacketReader::new(),
            packets: Vec::new(),
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.sock.write_all(bytes).expect("write to the session socket");
    }

    fn attach(&mut self, rows: u16, cols: u16) {
        self.send(&encode_attach(rows, cols));
    }

    fn data(&mut self, bytes: &[u8]) {
        self.send(&encode_data(bytes));
    }

    /// Read whatever has arrived.
    fn poll(&mut self) {
        let mut buf = [0u8; 65536];
        loop {
            match self.sock.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    let packets = self.reader.feed(&buf[..n]).expect("well-formed frames");
                    self.packets.extend(packets);
                }
                Err(_) => return,
            }
        }
    }

    fn wait_for(&mut self, what: &str, cond: impl Fn(&[Packet]) -> bool) {
        let start = Instant::now();
        loop {
            self.poll();
            if cond(&self.packets) {
                return;
            }
            assert!(start.elapsed() < WAIT, "timed out waiting for {what}");
        }
    }

    fn count(&self, t: MessageType) -> usize {
        self.packets.iter().filter(|p| p.type_ == t).count()
    }

    /// A fresh terminal fed this client's SCREEN and DATA frames in order.
    fn terminal(&self, rows: u16, cols: u16) -> TerminalActor {
        let mut t = TerminalActor::new(rows, cols, 10_000);
        for p in &self.packets {
            if matches!(p.type_, MessageType::Screen | MessageType::Data) {
                t.write(&p.payload);
            }
        }
        t
    }
}

/// Lines with trailing blanks removed and trailing empty lines dropped.
fn lines(text: &str) -> Vec<String> {
    let mut v: Vec<String> = text.lines().map(|l| l.trim_end().to_string()).collect();
    while v.last().is_some_and(|l| l.is_empty()) {
        v.pop();
    }
    v
}

/// What a client terminal shows in its viewport, as `lines`.
fn viewport(s: &mut Session) -> Vec<String> {
    s.screenshot();
    lines(&s.actor().plain(Range::Viewport))
}

/// The cells of a grid as `(grapheme, width class)`, row by row.
fn cells(g: &CellGrid) -> Vec<Vec<(String, Wide)>> {
    g.rows
        .iter()
        .map(|r| r.iter().map(|c| (c.text.clone(), c.wide)).collect())
        .collect()
}

/// The rows where two cell grids differ, drawn compactly (`_` for a spacer,
/// `[..]` around a wide cell), or `None` when they are the same.
fn cell_diff(a: &[Vec<(String, Wide)>], b: &[Vec<(String, Wide)>]) -> Option<String> {
    let draw = |row: &Vec<(String, Wide)>| -> String {
        row.iter()
            .map(|(t, w)| match w {
                Wide::Spacer => "_".to_string(),
                Wide::Wide => format!("[{t}]"),
                Wide::Narrow => t.clone(),
            })
            .collect::<String>()
            .trim_end()
            .to_string()
    };
    let mut out = String::new();
    for i in 0..a.len().max(b.len()) {
        let (ra, rb) = (a.get(i), b.get(i));
        if ra != rb {
            out.push_str(&format!(
                "row {i}:\n  left:  {:?}\n  right: {:?}\n",
                ra.map(draw).unwrap_or_default(),
                rb.map(draw).unwrap_or_default()
            ));
        }
    }
    (!out.is_empty()).then_some(out)
}

/// `(x, y)` of a client terminal's cursor.
fn cursor_of(s: &mut Session) -> (u16, u16) {
    s.screenshot();
    let (x, y, _) = s.actor().cursor();
    (x, y)
}

/// A shell `printf` format that prints `s` byte for byte.
fn octal(s: &str) -> String {
    s.bytes().map(|b| format!("\\{b:03o}")).collect()
}

// ── grapheme-width ──

/// Graphemes whose width depends on how a terminal clusters them.
const GRAPHEMES: &[(&str, &str)] = &[
    ("cjk", "漢字"),
    ("flag", "\u{1F1E7}\u{1F1F7}"),
    ("family", "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"),
    ("vs16-arrow", "\u{27A1}\u{FE0F}"),
    ("vs16-recycle", "\u{267B}\u{FE0F}"),
    ("skin-tone", "\u{1F44D}\u{1F3FD}"),
    ("combining", "e\u{301}"),
];

/// One `name:<grapheme>|` line per grapheme; the `|` shows where each one
/// ended.
fn grapheme_lines(tag: &str) -> String {
    GRAPHEMES
        .iter()
        .map(|(name, g)| format!("printf '{tag}{name}:{}|\\n'; ", octal(g)))
        .collect()
}

/// The same bytes into libghostty with nothing in between: the picture the
/// session's terminal should hold and every client should show.
fn bare_terminal(rows: u16, cols: u16, script: &str, until: &str) -> (Vec<Vec<(String, Wide)>>, (u16, u16), Vec<String>) {
    let mut bare = Session::spawn(
        "sh",
        &["-c", &format!("{script}exec cat")],
        SpawnOptions {
            rows: Some(rows),
            cols: Some(cols),
            ..Default::default()
        },
    )
    .expect("spawn the bare terminal");
    bare.wait_for_text(until, 10_000).expect("bare output");
    let grid = cells(&bare.actor().snapshot(0));
    let cursor = cursor_of(&mut bare);
    let text = lines(&bare.actor().plain(Range::Viewport));
    (grid, cursor, text)
}

/// Run `script` in a session with one client attached while it writes and
/// two that attach afterwards, and hold the session read and all three
/// clients to the bare terminal, cell by cell.
fn graphemes_match_everywhere(script: &str) {
    let (rows, cols) = (24, 40);
    let (bare_cells, bare_cursor, bare_text) = bare_terminal(rows, cols, script, "END");

    let d = Daemon::new();
    d.run_sh(rows, cols, &format!("{}{script}exec cat", d.gate("go")));
    // Attached before the child speaks: this client sees the output as DATA.
    let mut live = d.attach_outer(rows, cols);
    wait_until("the live client is attached", || d.attached() == 1);
    d.open("go");
    live.wait_for_text("END", 10_000).expect("live output");
    wait_until("the session read shows the output", || d.plain().contains("END"));
    // Attached afterwards: these are built from the replay.
    let mut late = d.client(rows, cols);
    late.wait_for_text("END", 10_000).expect("late client");
    let mut late_outer = d.attach_outer(rows, cols);
    late_outer.wait_for_text("END", 10_000).expect("late outer client");

    let session_text = lines(&d.plain());
    assert_eq!(session_text, bare_text, "the session read differs from the bare terminal");
    assert_eq!(d.cursor(), bare_cursor, "the session cursor");
    for (name, client) in [("live", &mut live), ("late", &mut late), ("late outer", &mut late_outer)] {
        client.screenshot();
        if let Some(diff) = cell_diff(&cells(&client.actor().snapshot(0)), &bare_cells) {
            panic!("{name} client (left) differs from the session's terminal (right):\n{diff}");
        }
        assert_eq!(cursor_of(client), bare_cursor, "{name} client cursor");
    }
}

/// Every client shows each wide or multi-codepoint grapheme in the cells the
/// session's own terminal gave it, whether the child asked for grapheme
/// clustering or not, and a plain read puts no padding after a wide
/// character and never blanks a cluster.
#[test]
fn wide_and_clustered_graphemes_take_the_same_cells_in_the_session_and_every_client() {
    graphemes_match_everywhere(&format!("{}printf 'END'; ", grapheme_lines("")));
    // Clustering on from the start: flags, ZWJ families, skin tones and VS16
    // emoji are each one wide cell.
    graphemes_match_everywhere(&format!("printf '\\033[?2027h'; {}printf 'END'; ", grapheme_lines("")));

    let d = Daemon::new();
    d.run_sh(24, 40, &format!("printf '\\033[?2027h'; {}printf 'END'; exec cat", grapheme_lines("")));
    wait_until("the output", || d.plain().contains("END"));
    let text = d.plain();
    assert!(text.lines().any(|l| l == "cjk:漢字|"), "a wide character is padded in the plain read: {text:?}");
    for (name, g) in GRAPHEMES {
        assert!(text.contains(&format!("{name}:{g}|")), "{name} is not read back whole: {text:?}");
    }
}

/// A row keeps the cells it was laid out in when the child later changes
/// grapheme clustering (`CSI ? 2027 h/l`): a client that attaches afterwards
/// must not re-cluster the history under the new mode.
#[test]
#[ignore = "fails on main: the replay writes the final ?2027 state ahead of every cell, so a late client re-clusters rows laid out under the other mode"]
fn rows_written_before_a_grapheme_clustering_change_keep_their_cells_in_a_late_client() {
    // Written unclustered, then the child turns clustering on.
    graphemes_match_everywhere(&format!(
        "{}printf '\\033[?2027h'; {}printf 'END'; ",
        grapheme_lines("before-"),
        grapheme_lines("after-")
    ));
    // Written clustered, then the child turns it off again (a program that
    // restores the mode when it exits).
    graphemes_match_everywhere(&format!(
        "printf '\\033[?2027h'; {}printf '\\033[?2027l'; {}printf 'END'; ",
        grapheme_lines("during-"),
        grapheme_lines("after-")
    ));
}

// ── utf8-integrity ──

/// Multi-byte UTF-8 split across the child's writes and across a client's
/// input frames comes out whole everywhere: in a client attached while it was
/// written, in plain and ANSI reads, in a late client's replay, and in the
/// child's own input. `🐛` is `F0 9F 90 9B`: its continuation bytes are the
/// 8-bit DCS and CSI controls, so a byte-level scanner that took them for
/// escapes would break it.
#[test]
fn utf8_split_across_writes_and_frames_is_never_corrupted() {
    let (rows, cols) = (12, 40);
    let d = Daemon::new();
    // `ö` is split after its lead byte, `🐛` after its second byte.
    let script = format!(
        "{}printf 'G\\303'; sleep 0.1; printf '\\266rn \\360\\237'; sleep 0.1; printf '\\220\\233\\n'; exec cat",
        d.gate("go")
    );
    d.run_sh(rows, cols, &script);
    let mut live = d.attach_outer(rows, cols);
    wait_until("the live client is attached", || d.attached() == 1);
    d.open("go");
    live.wait_for_text("Görn 🐛", 10_000).expect("the live client shows the text whole");
    assert!(d.plain().contains("Görn 🐛"), "plain read: {:?}", d.plain());
    assert!(d.peek(&[]).contains("Görn 🐛"), "ANSI read: {:?}", d.peek(&[]));
    let mut late = d.client(rows, cols);
    late.wait_for_text("Görn 🐛", 10_000).expect("the replay carries the text whole");

    // Input, one byte per DATA frame: `cat` (with the tty's echo) prints it
    // back, so a mangled scalar would show on the screen twice.
    let mut raw = d.raw();
    for b in "Ü🐛\r".bytes() {
        raw.data(&[b]);
        std::thread::sleep(Duration::from_millis(20));
    }
    // Input through `pty send` and typed into the attached client.
    let sent = d.pty(&["send", &d.name, "ß🦀\r"]);
    assert!(sent.status.success(), "pty send failed");
    wait_until("the sent line", || d.plain().matches("ß🦀").count() == 2);
    live.type_str("ñ🎉\r");
    wait_until("the typed line", || d.plain().matches("ñ🎉").count() == 2);

    let text = d.plain();
    assert_eq!(text.matches("Ü🐛").count(), 2, "split input was not echoed whole: {text:?}");
    assert!(!text.contains('\u{FFFD}'), "a replacement character appeared: {text:?}");
    for (name, client) in [("live", &mut live), ("late", &mut late)] {
        let shown = viewport(client);
        assert_eq!(shown, lines(&text), "{name} client differs from the session");
    }
}

// ── chunking-independent-screen ──

/// A full-screen editor's frame the way one draws it: the shell line it was
/// started from, then the alternate screen with a number column, a sign
/// column, a cursor line with a background, long lines that the terminal
/// autowraps with SGR runs and wide characters straddling the wrap column,
/// and a reverse-video status line.
fn editor_frame(rows: u16, cols: u16) -> Vec<u8> {
    let mut s: Vec<u8> = Vec::new();
    s.extend_from_slice(b"$ edit notes.md\r\n");
    s.extend_from_slice(b"\x1b]0;notes.md\x07\x1b[?1049h\x1b[?1h\x1b=\x1b[?25l\x1b[H\x1b[2J");
    let words = ["alpha", "βeta", "漢字", "gamma😀", "delta", "e\u{301}psilon", "zeta"];
    let mut row = 1u16;
    let mut n = 1;
    while row < rows - 1 {
        s.extend_from_slice(format!("\x1b[{row};1H").as_bytes());
        if n == 3 {
            // The cursor line: a background across the row.
            s.extend_from_slice(b"\x1b[48;5;236m\x1b[K");
        }
        s.extend_from_slice(format!("\x1b[38;5;244m{n:>3} \x1b[39m").as_bytes());
        s.extend_from_slice(if n % 2 == 0 { b"\x1b[31m>\x1b[39m " } else { b"  " });
        // Long enough to autowrap across three rows.
        let mut text = String::new();
        let mut visible = 6;
        let mut i = n;
        while visible < (cols as usize) * 2 + cols as usize / 2 {
            let w = words[i % words.len()];
            text.push_str(&format!("\x1b[{}m{w}\x1b[0m ", 32 + (i % 5)));
            if n == 3 {
                text.push_str("\x1b[48;5;236m");
            }
            visible += w.chars().count() + 1;
            i += 1;
        }
        s.extend_from_slice(text.as_bytes());
        s.extend_from_slice(b"\x1b[0m");
        // Where the terminal's autowrap left the cursor decides the next row.
        row += 4;
        n += 1;
    }
    s.extend_from_slice(format!("\x1b[{rows};1H\x1b[7m NORMAL  notes.md  {n}L \x1b[K\x1b[0m").as_bytes());
    s.extend_from_slice(b"\x1b[3;9H\x1b[?25h");
    s
}

/// Everything a reader or a late client can learn about a session's screen.
#[derive(Debug, PartialEq)]
struct Picture {
    ansi: String,
    plain_full: String,
    cursor: (u16, u16),
    replay_cells: Vec<Vec<(String, Wide)>>,
}

fn picture(d: &Daemon, rows: u16, cols: u16) -> Picture {
    let mut late = d.client(rows, cols);
    late.wait_for_text("NORMAL", 10_000).expect("a late client");
    Picture {
        ansi: d.peek(&[]),
        plain_full: d.plain_full(),
        cursor: d.cursor(),
        replay_cells: cells(&late.actor().snapshot(0)),
    }
}

/// The same bytes written in one piece, one byte per write, and five bytes per
/// write (splitting UTF-8 scalars, CSI and OSC sequences and wide characters
/// at the wrap column) leave the same screen, the same reads and the same
/// replay; and a client attached while the bytes arrived one at a time shows
/// what the session holds.
#[test]
fn the_screen_and_its_replay_do_not_depend_on_how_the_output_was_split() {
    let (rows, cols) = (20, 30);
    let frame = editor_frame(rows, cols);
    let mut pictures = Vec::new();
    for (label, writer) in [
        ("whole", "cat"),
        ("bytewise", "python3 -c 'import os,time; [(os.write(1,b),time.sleep(.001)) for b in iter(lambda:os.read(0,1),b\"\")]'"),
        ("five", "dd bs=5"),
    ] {
        let d = Daemon::new();
        let fixture = d.file("frame.bin");
        std::fs::write(&fixture, &frame).expect("write the fixture");
        d.run_sh(
            rows,
            cols,
            &format!("stty -echo; {}{writer} < '{}' 2>/dev/null; exec sleep 600", d.gate("go"), fixture.display()),
        );
        let mut raw = d.raw();
        raw.attach(rows, cols);
        raw.wait_for("the initial screen", |p| p.iter().any(|x| x.type_ == MessageType::Screen));
        d.open("go");
        wait_until("the frame", || d.plain().contains("NORMAL"));
        std::thread::sleep(Duration::from_millis(100));
        raw.poll();
        let live = raw.terminal(rows, cols);
        if label == "bytewise" {
            assert!(
                raw.count(MessageType::Data) > 10,
                "the output was not split: {} DATA frames",
                raw.count(MessageType::Data)
            );
        }
        let p = picture(&d, rows, cols);
        if let Some(diff) = cell_diff(&cells(&live.snapshot(0)), &p.replay_cells) {
            panic!("{label}: the live client (left) differs from the replay (right):\n{diff}");
        }
        let (lx, ly, _) = live.cursor();
        assert_eq!((lx, ly), p.cursor, "{label}: the live client's cursor");
        pictures.push((label, p));
    }
    let (_, whole) = &pictures[0];
    // The bare terminal agrees too.
    let mut bare = TerminalActor::new(rows, cols, 10_000);
    bare.write(&frame);
    if let Some(diff) = cell_diff(&whole.replay_cells, &cells(&bare.snapshot(0))) {
        panic!("the session (left) differs from a bare terminal (right):\n{diff}");
    }
    for (label, p) in &pictures[1..] {
        assert_eq!(p.ansi, whole.ansi, "{label}: the ANSI read differs from the unsplit one");
        assert_eq!(p.plain_full, whole.plain_full, "{label}: the plain read differs");
        assert_eq!(p.cursor, whole.cursor, "{label}: the cursor differs");
        if let Some(diff) = cell_diff(&p.replay_cells, &whole.replay_cells) {
            panic!("{label}: the replay (left) differs from the unsplit one (right):\n{diff}");
        }
    }
}

// ── altscreen-integrity ──

/// A client's viewport and cursor against the session's own read of them.
fn mismatch(name: &str, client: &mut Session, d: &Daemon) -> Option<String> {
    let (shown, cursor) = (viewport(client), cursor_of(client));
    let (truth, truth_cursor) = (lines(&d.plain()), d.cursor());
    (shown != truth || cursor != truth_cursor).then(|| {
        format!(
            "{name} client shows {shown:#?} with the cursor at {cursor:?}; the session holds {truth:#?} with the cursor at {truth_cursor:?}"
        )
    })
}

/// A full-screen program that fills the alternate screen with autowrap off,
/// is widened while it runs, and exits gives every client the primary screen
/// back as it was — text, cursor, nothing left over from the alternate screen
/// in the columns the resize exposed — including a client that attached while
/// it ran.
#[test]
fn leaving_the_alternate_screen_restores_the_primary_screen_and_cursor_for_every_client() {
    let d = Daemon::new();
    let fill = "#".repeat(60);
    let script = format!(
        "printf 'P1 primary\\nP2 primary\\nP3 primary\\n$ partial'; {}\
         printf '\\033[?1049h\\033[?7l\\033[H'; for r in 1 2 3 4 5 6 7 8 9 10; do printf '\\033[%d;1H{fill}' $r; done; {}\
         printf '\\033[?1049l'; printf ' back'; exec cat",
        d.gate("alt"),
        d.gate("exit")
    );
    d.run_sh(10, 30, &script);
    let mut live = d.attach_outer(10, 30);
    live.wait_for_text("$ partial", 10_000).expect("the primary screen");
    let primary = viewport(&mut live);
    let primary_cursor = cursor_of(&mut live);
    d.open("alt");
    live.wait_for_text("######", 10_000).expect("the alternate screen");
    // Attached while the program runs: built from a replay that carries both
    // screens.
    let mut during = d.client(10, 30);
    during.wait_for_text("######", 10_000).expect("a client attached during the program");
    // Every client grows the window; the session follows the smallest.
    live.resize(10, 50);
    during.resize(10, 50);
    wait_until("the session to grow", || json_number(&d.stats(), "cols") == Some(50));
    d.open("exit");
    wait_until("the program to exit", || d.plain().contains("back"));
    let mut after = d.client(10, 50);
    after.wait_for_text("back", 10_000).expect("a client attached afterwards");

    let expected: Vec<String> = primary
        .iter()
        .map(|l| if l == "$ partial" { "$ partial back".to_string() } else { l.clone() })
        .collect();
    assert_eq!(lines(&d.plain()), expected, "the session's primary screen");
    assert_eq!(d.cursor(), (primary_cursor.0 + 5, primary_cursor.1), "the session's cursor");
    for (name, client) in [("live", &mut live), ("during", &mut during), ("after", &mut after)] {
        wait_until(&format!("{name} client to show the exit"), || viewport(client).concat().contains("back"));
        if let Some(m) = mismatch(name, client, &d) {
            panic!("{m}");
        }
        let all = client.screenshot().text;
        assert!(!all.contains('#'), "{name} client kept cells of the alternate screen: {all:?}");
    }
}

/// A client that attaches while a full-screen program runs, after the window
/// was resized, must get the primary screen back the way the session holds
/// it when the program exits: reflowed to the new width, with the cursor
/// where the session's is.
#[test]
#[ignore = "fails on main: the replay's primary half is the copy taken on entering the alternate screen, laid out at the old width with the old cursor"]
fn a_client_attaching_during_a_full_screen_program_after_a_resize_gets_the_primary_screen_back_intact() {
    let mut failures = Vec::new();
    // Narrower: primary lines that fitted now wrap; wider: a soft-wrapped
    // line now fits on one row.
    for (label, from, to, primary) in [
        ("narrower", 30u16, 16u16, "printf 'line-1-abcdefghijk\\nline-2-abcdefghijk\\nline-3-abcdefghijk\\n$ '"),
        ("wider", 16, 30, "printf 'a-long-line-that-wraps-over-three-rows\\nnext\\n$ '"),
    ] {
        let d = Daemon::new();
        let script = format!(
            "{primary}; printf '\\033[?1049h\\033[HFULLSCREEN'; {}printf '\\033[?1049lX'; exec cat",
            d.gate("exit")
        );
        d.run_sh(10, from, &script);
        let mut first = d.client(10, from);
        first.wait_for_text("FULLSCREEN", 10_000).expect("the program");
        first.resize(10, to);
        wait_until("the resize", || json_number(&d.stats(), "cols") == Some(to as i64));
        std::thread::sleep(Duration::from_millis(200));
        let mut late = d.client(10, to);
        late.wait_for_text("FULLSCREEN", 10_000).expect("a late client");
        d.open("exit");
        wait_until("the program to exit", || d.plain().contains('X'));
        for (name, client) in [("live", &mut first), ("late", &mut late)] {
            wait_until("the client to leave the program", || {
                !client.screenshot().text.contains("FULLSCREEN")
            });
            std::thread::sleep(Duration::from_millis(100));
            if let Some(m) = mismatch(&format!("{label}: {name}"), client, &d) {
                failures.push(m);
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// `CSI ? 1049 l` with no alternate screen to leave is a no-op in xterm,
/// foot, kitty and alacritty: the cursor stays where it is and later output
/// lands below what is on screen.
#[test]
#[ignore = "fails in libghostty itself: an unpaired CSI ?1049l on the primary screen homes the cursor without clearing, so later output overwrites the top rows"]
fn an_unpaired_alternate_screen_exit_leaves_the_cursor_where_it_was() {
    let script = "printf '\\033[H\\033[2J'; seq 1 15; printf '\\033[?1049l'; echo hello; exec cat";
    // libghostty alone, no session in between.
    let mut bare = Session::spawn("sh", &["-c", script], SpawnOptions::default()).expect("spawn");
    bare.wait_for_text("hello", 10_000).expect("output");
    let bare_rows = viewport(&mut bare);
    // The session's terminal is the same emulator and inherits it.
    let d = Daemon::new();
    d.run_sh(24, 80, script);
    wait_until("output", || d.plain().contains("hello"));
    let session_rows = lines(&d.plain());
    let expected: Vec<String> = (1..=15).map(|i| i.to_string()).chain(["hello".to_string()]).collect();
    assert_eq!(
        (bare_rows, session_rows),
        (expected.clone(), expected),
        "(libghostty alone, the session)"
    );
}

// ── client-screen-matches-session ──

/// Two attached clients — `pty attach` in a terminal, and a socket client —
/// show exactly the session's screen and cursor through a shell session's
/// live output: echo and Backspace erasing, `EL`, scrolling, a soft-wrapped
/// long line, mid-line edits the line editor redraws, clear-screen, and
/// resizes both narrower and wider.
#[test]
fn attached_clients_show_the_sessions_screen_through_output_erases_and_resizes() {
    let d = Daemon::new();
    d.run_sh(24, 60, "exec env PS1='$ ' bash --norc --noprofile");
    let mut outer = d.attach_outer(24, 60);
    let mut socket = d.client(24, 60);
    wait_until("the prompt", || d.plain().contains("$ "));
    let settle = |d: &Daemon, want: &str| {
        wait_until(&format!("{want:?} in the session"), || d.plain().contains(want));
        std::thread::sleep(Duration::from_millis(150));
    };
    let check = |step: &str, outer: &mut Session, socket: &mut Session, d: &Daemon| {
        for (name, client) in [("outer", outer), ("socket", socket)] {
            if let Some(m) = mismatch(&format!("after {step}: {name}"), client, d) {
                panic!("{m}");
            }
        }
    };

    // Typing, then Backspace: the terminal erases what the line editor drew.
    outer.type_str("echo agy");
    settle(&d, "echo agy");
    outer.type_str("\x7f\x7f");
    wait_until("the erase", || !d.plain().contains("agy"));
    std::thread::sleep(Duration::from_millis(150));
    check("backspace", &mut outer, &mut socket, &d);
    outer.type_str("bc\r");
    settle(&d, "abc\n$ ");
    check("echo", &mut outer, &mut socket, &d);

    outer.type_str("printf 'xxxxxxxxxxxxxxxx\\r\\033[Kshort\\n'\r");
    settle(&d, "\nshort\n");
    check("erase in line", &mut outer, &mut socket, &d);

    outer.type_str("seq 1 40; printf '%0150d\\n' 7\r");
    settle(&d, "0007\n$ ");
    check("scrolling and a soft wrap", &mut outer, &mut socket, &d);

    // A mid-line edit: the line editor redraws the tail of the line.
    outer.type_str("echo abcdef");
    settle(&d, "echo abcdef");
    outer.type_str("\x1b[D\x1b[D\x1b[DXY");
    settle(&d, "echo abcXYdef");
    check("a mid-line edit", &mut outer, &mut socket, &d);
    outer.type_str("\r");
    settle(&d, "abcXYdef\n$ ");

    for (rows, cols) in [(16u16, 40u16), (30, 80), (20, 50)] {
        outer.resize(rows, cols);
        socket.resize(rows, cols);
        wait_until("the resize", || {
            let s = d.stats();
            json_number(&s, "rows") == Some(rows as i64) && json_number(&s, "cols") == Some(cols as i64)
        });
        let marker = format!("size-{rows}x{cols}");
        outer.type_str(&format!("echo {marker}\r"));
        settle(&d, &format!("{marker}\n$ "));
        check(&format!("a resize to {rows}x{cols}"), &mut outer, &mut socket, &d);
    }

    // Clear-screen from the child. Readline's Ctrl-L only redraws a prompt
    // in some non-login shells, so issue the terminal operation explicitly.
    outer.type_str("printf '\\033[2J\\033[H'; echo CLEARED\r");
    wait_until("the clear", || {
        let screen = d.plain();
        screen.contains("CLEARED") && !screen.contains("size-")
    });
    std::thread::sleep(Duration::from_millis(150));
    check("clear-screen", &mut outer, &mut socket, &d);
}

/// A client that attaches while the cursor waits to wrap (the child printed
/// into the last column) must put the child's next character where the
/// session puts it: at the start of the next row, not over the last column.
#[test]
#[ignore = "fails on main: the replay moves the cursor to the last column and drops the pending wrap, so the next character overwrites it"]
fn a_client_attaching_while_the_cursor_waits_to_wrap_shows_the_next_output_on_the_next_row() {
    let mut failures = Vec::new();
    // A narrow character in the last column, and a wide one in the last two.
    for (label, fill) in [("narrow", "0123456789"), ("wide", "01234567漢")] {
        let d = Daemon::new();
        let script = format!("printf '{}'; {}printf 'Z'; exec cat", octal(fill), d.gate("next"));
        d.run_sh(5, 10, &script);
        wait_until("the full row", || d.plain().contains(fill));
        assert_eq!(d.cursor(), (10, 0), "{label}: the session's cursor waits to wrap");
        let mut late = d.client(5, 10);
        late.wait_for_text(fill, 10_000).expect("a late client");
        let mut outer = d.attach_outer(5, 10);
        outer.wait_for_text(fill, 10_000).expect("a late outer client");
        d.open("next");
        wait_until("the next character", || d.plain().contains('Z'));
        assert_eq!(lines(&d.plain()), vec![fill.to_string(), "Z".to_string()], "{label}: the session");
        for (name, client) in [("late", &mut late), ("late outer", &mut outer)] {
            wait_until("the client to show it", || client.screenshot().text.contains('Z'));
            if let Some(m) = mismatch(&format!("{label}: {name}"), client, &d) {
                failures.push(m);
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

// ── reattach-replay-exact ──

/// Reattaching, at the session's size or another one, replays the history and
/// the screen exactly once with the cursor where the session's is, and never
/// adds to the session's own history — even for a shell whose line editor
/// redraws on the resize nudge a differently sized attach causes.
#[test]
fn reattaching_replays_the_history_and_screen_once_with_the_cursor_in_place() {
    let d = Daemon::new();
    d.run_sh(10, 40, "exec env PS1='$ ' bash --norc --noprofile");
    let mut first = d.attach_outer(10, 40);
    wait_until("the prompt", || d.plain().contains("$ "));
    first.type_str("seq -f 'HIST-%04g' 1 60\r");
    wait_until("the output", || d.plain().contains("HIST-0060\n$ "));
    first.type_str("echo pending-input");
    wait_until("the typed line", || d.plain().contains("pending-input"));
    std::thread::sleep(Duration::from_millis(150));
    drop(first);
    wait_until("the first client to go", || d.attached() == 0);
    let history = lines(&d.plain_full());

    for (round, (rows, cols)) in [(10u16, 40u16), (12, 50), (10, 40)].into_iter().enumerate() {
        // A client at another size shrinks or grows the session; a fresh
        // terminal each time, the way a new window reattaches.
        for name in ["outer", "socket"] {
            let mut client = if name == "outer" { d.attach_outer(rows, cols) } else { d.client(rows, cols) };
            client.wait_for_text("pending-input", 10_000).expect("the replay");
            wait_until("the size", || json_number(&d.stats(), "cols") == Some(cols as i64));
            std::thread::sleep(Duration::from_millis(250));
            if let Some(m) = mismatch(&format!("round {round}: {name}"), &mut client, &d) {
                panic!("{m}");
            }
            let all = client.screenshot().lines;
            for n in [1, 30, 60] {
                let line = format!("HIST-{n:04}");
                assert_eq!(
                    all.iter().filter(|l| l.trim_end() == line).count(),
                    1,
                    "round {round}: {name} client holds {line} other than once: {all:#?}"
                );
            }
            drop(client);
            wait_until(&format!("round {round}: the {name} client to go"), || d.attached() == 0);
        }
    }
    wait_until("the session back at its size", || json_number(&d.stats(), "cols") == Some(40));
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(lines(&d.plain_full()), history, "reattaching changed the session's history");
}

/// Detaching and reattaching in the same terminal leaves the session's
/// history in that terminal's scrollback once, not once per attach.
#[test]
#[ignore = "fails on main: every attach clears only the visible screen and replays the whole history, so each reattach adds another copy to the terminal's scrollback"]
fn reattaching_in_the_same_terminal_leaves_one_copy_of_the_history() {
    let d = Daemon::new();
    d.run_sh(10, 40, "seq -f 'HIST-%04g' 1 60; printf 'live> '; exec cat");
    wait_until("the output", || d.plain().contains("live> "));
    let attach = format!("'{}' attach {}", pty_bin(), d.name);
    let mut outer = d.outer(
        10,
        40,
        &format!("{attach}; {}{attach}; {}{attach}; exec cat", d.gate("second"), d.gate("third")),
    );
    for (round, gate) in [(1, None), (2, Some("second")), (3, Some("third"))] {
        if let Some(gate) = gate {
            d.open(gate);
        }
        wait_until("the attach", || d.attached() == 1);
        outer.wait_for_text("live> ", 10_000).expect("the replay");
        std::thread::sleep(Duration::from_millis(200));
        let copies = outer.screenshot().lines.iter().filter(|l| l.trim_end() == "HIST-0001").count();
        assert_eq!(copies, 1, "after attach {round} the terminal holds {copies} copies of the history");
        outer.send_keys("\x1c");
        wait_until("the detach", || d.attached() == 0);
    }
}

// ── soft-wrap-logical-lines ──

/// 1000 characters with no spaces, printed as one line.
const LONG_LINE_SCRIPT: &str =
    "i=0; while [ $i -lt 100 ]; do printf 'abcdefghij'; i=$((i+1)); done; printf '\\n'";

/// A plain-text read of the history returns a line the terminal soft-wrapped
/// as the one line the program printed.
#[test]
#[ignore = "fails on main: plain reads format rows without unwrapping, so a soft-wrapped line comes back split at every wrap column"]
fn a_plain_read_joins_soft_wrapped_rows_into_one_logical_line() {
    let d = Daemon::new();
    d.run_sh(10, 40, &format!("{LONG_LINE_SCRIPT}; printf 'END\\n'; exec cat"));
    wait_until("the output", || d.plain().contains("END"));
    let long = "abcdefghij".repeat(100);
    let full = d.plain_full();
    assert!(
        full.lines().any(|l| l == long),
        "no line of the plain history read is the 1000-character line; it came back as {} rows: {:?}",
        full.lines().filter(|l| l.starts_with("abcdefghij")).count(),
        full.lines().take(3).collect::<Vec<_>>()
    );
}

/// Only what the terminal wrapped is a wrap: rows a program wrote itself —
/// a line exactly as wide as the terminal ended with CR LF, or rows placed
/// with cursor moves — stay separate lines in a plain read; and every client,
/// live or late, marks the rows the terminal did wrap as continuations, so a
/// host can join a URL that runs across them.
#[test]
fn only_terminal_soft_wraps_are_wraps_and_every_client_keeps_them() {
    let (rows, cols) = (12u16, 40u16);
    let exact = "x".repeat(cols as usize);
    let script = format!(
        "printf 'https://example.com/a/very/long/path/that/soft/wraps/across/two/rows?q=1\\n'; \
         printf '{exact}\\n'; printf '\\033[9;1Hplaced-row-one\\033[10;1Hplaced-row-two\\n'; printf 'END'; "
    );
    let d = Daemon::new();
    d.run_sh(rows, cols, &format!("{}{script}exec cat", d.gate("go")));
    let mut live = d.attach_outer(rows, cols);
    wait_until("the live client is attached", || d.attached() == 1);
    d.open("go");
    wait_until("the output", || d.plain().contains("END"));
    let text = lines(&d.plain());
    for row in [exact.as_str(), "placed-row-one", "placed-row-two"] {
        assert!(text.iter().any(|l| l == row), "{row:?} is not a line of its own: {text:#?}");
    }

    let mut bare = Session::spawn(
        "sh",
        &["-c", &format!("{script}exec cat")],
        SpawnOptions {
            rows: Some(rows),
            cols: Some(cols),
            ..Default::default()
        },
    )
    .expect("spawn the bare terminal");
    bare.wait_for_text("END", 10_000).expect("bare output");
    let wrapped = bare.actor().snapshot(0).wrapped;
    assert_eq!(&wrapped[..3], &[false, true, false], "the URL wraps once, the exact-width line not at all");
    let mut late = d.client(rows, cols);
    late.wait_for_text("END", 10_000).expect("a late client");
    let mut late_outer = d.attach_outer(rows, cols);
    late_outer.wait_for_text("END", 10_000).expect("a late outer client");
    for (name, client) in [("live", &mut live), ("late", &mut late), ("late outer", &mut late_outer)] {
        client.wait_for_text("END", 10_000).expect("the output");
        assert_eq!(client.actor().snapshot(0).wrapped, wrapped, "{name} client's wrap flags");
    }
}

// ── read-scrollback ──

/// A read returns the viewport of a fresh session with no history, the whole
/// history once it scrolls, and the cursor — each the same as a client holds:
/// one that was attached throughout and one that attaches afterwards. A
/// clear leaves the session's history the same as the client's, and while a
/// full-screen program runs an ANSI read of the history still carries the
/// primary screen's.
#[test]
fn a_read_returns_history_and_the_cursor_consistent_with_every_client() {
    let (rows, cols) = (10u16, 40u16);
    let d = Daemon::new();
    d.run_sh(rows, cols, "exec env PS1='$ ' bash --norc --noprofile");
    let mut live = d.attach_outer(rows, cols);
    wait_until("the prompt", || d.plain().contains("$ "));

    live.type_str("echo hello123\r");
    wait_until("the output", || d.plain().contains("hello123\n$ "));
    assert!(d.plain_full().contains("hello123"), "a history read of a session with no history");

    live.type_str("seq 1 1500\r");
    wait_until("the output", || d.plain().contains("1500\n$ "));
    let full = lines(&d.plain_full());
    let numbers: Vec<&String> = full.iter().filter(|l| l.parse::<u32>().is_ok()).collect();
    assert_eq!(numbers.len(), 1500, "every line of history, once");
    assert_eq!((numbers[0].as_str(), numbers[1499].as_str()), ("1", "1500"));
    assert!(!lines(&d.plain()).contains(&"1".to_string()), "the viewport read is only the viewport");

    live.type_str("echo before-clear; printf '\\033[H\\033[2J'; echo after-clear\r");
    wait_until("the output", || d.plain().contains("after-clear\n$ "));
    std::thread::sleep(Duration::from_millis(150));
    let mut late = d.client(rows, cols);
    late.wait_for_text("after-clear", 10_000).expect("a late client");
    let full = lines(&d.plain_full());
    for (name, client) in [("live", &mut live), ("late", &mut late)] {
        let shot = client.screenshot();
        assert_eq!(lines(&shot.text), full, "{name} client's history differs from the history read");
        let (x, y, _) = client.actor().cursor();
        assert_eq!((x, y), d.cursor(), "{name} client's cursor differs from the reported one");
    }

    // A full-screen program: the ANSI history read still has the primary
    // screen's history ahead of the program's screen.
    live.type_str("printf '\\033[?1049h\\033[HFULLSCREEN'; sleep 30\r");
    wait_until("the program", || d.plain().contains("FULLSCREEN"));
    let ansi_full = d.peek(&["--full"]);
    assert!(
        ansi_full.contains("\r\n1490\r\n") && ansi_full.contains("FULLSCREEN"),
        "the ANSI history read during a full-screen program lacks the primary history ({} bytes)",
        ansi_full.len()
    );
}

// ── read-is-passive ──

/// A child that looks like a full-screen agent — alternate screen, mouse
/// reporting, alternate scroll — and records every byte of input and every
/// SIGWINCH it gets.
const RECORDING_AGENT: &str = r#"
use strict;
my ($input, $winch) = @ARGV;
open(my $in, '>>', $input) or die; binmode $in;
open(my $w, '>>', $winch) or die;
$SIG{WINCH} = sub { print $w "WINCH\n"; $w->flush; };
system('stty raw -echo');
$| = 1;
print "\e[?1049h\e[?1000h\e[?1006h\e[?1007h\e[H\e[2J";
print "\e[$_;1Hagent line $_" for 1..11;
print "\e[12;1HREADY";
while (1) {
    my $n = sysread(STDIN, my $buf, 4096);
    if (!defined $n) { next if $!{EINTR}; last; }
    last if $n == 0;
    print $in $buf; $in->flush;
}
"#;

/// Reads — plain and ANSI, viewport and history, one-shot and over the
/// socket, `stats` and `list` — send the child no input and no SIGWINCH,
/// send an attached client nothing and leave its screen as it was, and
/// answer at once even when the child is stopped and reads nothing.
#[test]
fn reading_a_session_never_writes_to_the_child_moves_a_client_or_waits_on_the_child() {
    let (rows, cols) = (12u16, 40u16);
    let d = Daemon::new();
    let (input, winch) = (d.file("input.bin"), d.file("winch.log"));
    let agent = d.file("agent.pl");
    std::fs::write(&agent, RECORDING_AGENT).expect("write the agent");
    d.run(
        rows,
        cols,
        "perl",
        &[agent.to_str().unwrap(), input.to_str().unwrap(), winch.to_str().unwrap()],
    );
    let mut outer = d.attach_outer(rows, cols);
    outer.wait_for_text("READY", 10_000).expect("the agent");
    let mut raw = d.raw();
    raw.attach(rows, cols);
    raw.wait_for("the initial screen", |p| p.iter().any(|x| x.type_ == MessageType::Screen));
    std::thread::sleep(Duration::from_millis(300));
    raw.poll();
    let frames_before = raw.packets.len();
    let screen_before = outer.screenshot().ansi;
    let winch_before = std::fs::read_to_string(&winch).unwrap_or_default();

    let reads: &[&[&str]] = &[
        &["peek"],
        &["peek", "--plain"],
        &["peek", "--full"],
        &["peek", "--plain", "--full"],
    ];
    let read_all = |label: &str| {
        for _ in 0..5 {
            for flags in reads {
                let start = Instant::now();
                d.peek(flags);
                assert!(start.elapsed() < Duration::from_secs(2), "{label}: pty peek {flags:?} was slow");
            }
            let start = Instant::now();
            assert!(d.pty(&["stats", "--json", &d.name]).status.success());
            assert!(d.pty(&["list", "--json"]).status.success());
            assert!(start.elapsed() < Duration::from_secs(4), "{label}: stats and list were slow");
            for (plain, full) in [(false, false), (true, false), (false, true), (true, true)] {
                let mut peeker = d.raw();
                peeker.send(&encode_peek(plain, full));
                peeker.wait_for("a PEEK screen", |p| p.iter().any(|x| x.type_ == MessageType::Screen));
            }
        }
    };
    read_all("running");
    // A stopped child reads nothing and writes nothing; a read must not care.
    let pid = d.child_pid();
    unsafe { libc_kill(pid, 19) }; // SIGSTOP
    read_all("stopped");
    unsafe { libc_kill(pid, 18) }; // SIGCONT
    std::thread::sleep(Duration::from_millis(300));

    let got = std::fs::read(&input).unwrap_or_default();
    assert!(got.is_empty(), "reads sent the child input: {got:?}");
    let winch_after = std::fs::read_to_string(&winch).unwrap_or_default();
    assert_eq!(winch_after, winch_before, "reads resized the child");
    raw.poll();
    let extra: Vec<MessageType> = raw.packets[frames_before..].iter().map(|p| p.type_).collect();
    assert!(extra.is_empty(), "reads sent the attached client {extra:?}");
    assert_eq!(outer.screenshot().ansi, screen_before, "the attached client's screen moved");
}

unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

// ── reattach-replay-modes ──

/// The input- and screen-affecting modes a child can set.
const MODES: &[(&str, Mode)] = &[
    ("cursor keys ?1", Mode::DECCKM),
    ("X10 mouse ?9", Mode::X10_MOUSE),
    ("mouse ?1000", Mode::NORMAL_MOUSE),
    ("mouse ?1002", Mode::BUTTON_MOUSE),
    ("mouse ?1003", Mode::ANY_MOUSE),
    ("focus ?1004", Mode::FOCUS_EVENT),
    ("UTF-8 mouse ?1005", Mode::UTF8_MOUSE),
    ("SGR mouse ?1006", Mode::SGR_MOUSE),
    ("alternate scroll ?1007", Mode::ALT_SCROLL),
    ("urxvt mouse ?1015", Mode::URXVT_MOUSE),
    ("SGR pixel mouse ?1016", Mode::SGR_PIXELS_MOUSE),
    ("bracketed paste ?2004", Mode::BRACKETED_PASTE),
    ("cursor visible ?25", Mode::CURSOR_VISIBLE),
    ("grapheme clustering ?2027", Mode::GRAPHEME_CLUSTER),
];

/// Every tracked mode of a terminal, its kitty keyboard flags, and whether
/// the alternate screen is showing.
fn mode_state(s: &mut Session) -> Vec<(String, String)> {
    s.screenshot();
    let a = s.actor();
    let mut v: Vec<(String, String)> = MODES
        .iter()
        .map(|(name, m)| (name.to_string(), a.terminal().mode(*m).unwrap().to_string()))
        .collect();
    v.push(("kitty keyboard flags".into(), a.kitty_flags().to_string()));
    v.push(("alternate screen".into(), a.alt_screen_active().to_string()));
    v
}

fn mode_diff(name: &str, got: &[(String, String)], want: &[(String, String)]) -> Option<String> {
    let diffs: Vec<String> = got
        .iter()
        .zip(want)
        .filter(|(g, w)| g != w)
        .map(|(g, w)| format!("{}: {} (the child left it {})", g.0, g.1, w.1))
        .collect();
    (!diffs.is_empty()).then(|| format!("{name}: {}", diffs.join(", ")))
}

/// The modes a bare terminal is left in by `script`: what the child last set.
fn bare_modes(script: &str, until: &str) -> Vec<(String, String)> {
    let mut bare = Session::spawn("sh", &["-c", &format!("{script}exec cat")], SpawnOptions::default())
        .expect("spawn the bare terminal");
    bare.wait_for_text(until, 10_000).expect("bare output");
    mode_state(&mut bare)
}

/// Clients that attach after `script` ran — `pty attach` in a terminal, a
/// socket client, and a `pty peek -f` observer — each against what the child
/// left, as a list of differences.
fn late_mode_mismatches(label: &str, script: &str, until: &str) -> Vec<String> {
    let want = bare_modes(script, until);
    let d = Daemon::new();
    d.run_sh(24, 80, &format!("{script}exec cat"));
    wait_until("the output", || d.plain().contains(until));
    let mut clients = vec![
        ("pty attach", d.attach_outer(24, 80)),
        ("socket client", d.client(24, 80)),
        ("pty peek -f", d.outer(24, 80, &format!("exec '{}' peek -f {}", pty_bin(), d.name))),
    ];
    let mut out = Vec::new();
    for (name, client) in clients.iter_mut() {
        client.wait_for_text(until, 10_000).expect("the replay");
        std::thread::sleep(Duration::from_millis(100));
        if let Some(m) = mode_diff(&format!("{label}: {name}"), &mode_state(client), &want) {
            out.push(m);
        }
    }
    out
}

/// A client that attaches or observes later is put in exactly the modes the
/// child last set — mouse tracking and its encodings, bracketed paste, focus
/// reporting, cursor keys, alternate scroll, the kitty keyboard flags, cursor
/// visibility, grapheme clustering, the alternate screen — and in no mode the
/// child never set or already turned off.
#[test]
fn late_attachers_and_observers_get_exactly_the_modes_the_child_left() {
    let mut failures = Vec::new();
    // A child that never touched a mode: attaching must not turn mouse
    // tracking (or anything else) on.
    failures.extend(late_mode_mismatches("untouched", "printf 'plain-session'; ", "plain-session"));
    // A full-screen program's typical set.
    failures.extend(late_mode_mismatches(
        "full-screen",
        "printf '\\033[?1049h\\033[?1h\\033[?1002h\\033[?1006h\\033[?2004h\\033[?1004h\\033[?1007l\\033[?25l\\033[>5u\\033[?2027h\\033[HTUI-READY'; ",
        "TUI-READY",
    ));
    // Other mouse reporting and encodings.
    failures.extend(late_mode_mismatches(
        "mouse encodings",
        "printf '\\033[?9h\\033[?1003h\\033[?1005h\\033[?1015h\\033[?1016hMOUSE-READY'; ",
        "MOUSE-READY",
    ));
    // Everything set and then turned off again.
    failures.extend(late_mode_mismatches(
        "set then cleared",
        "printf '\\033[?1049h\\033[?1h\\033[?1000h\\033[?1002h\\033[?1006h\\033[?2004h\\033[?1004h\\033[?25l\\033[>5u'; \
         printf '\\033[<u\\033[?25h\\033[?1004l\\033[?2004l\\033[?1006l\\033[?1002l\\033[?1000l\\033[?1l\\033[?1049lCLEARED'; ",
        "CLEARED",
    ));
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A child that resets its terminal (RIS, `ESC c` — what `reset` and
/// `tput reset` send) after a full-screen program left modes on has turned
/// them all off: a late client must not get them back.
#[test]
#[ignore = "fails on main: the mode record ignores RIS, so a late client gets back the mouse tracking, kitty flags, hidden cursor and alternate screen the child already reset"]
fn a_late_attacher_gets_the_modes_left_after_the_child_resets_the_terminal() {
    let script = "printf '\\033[?1049h\\033[?1000h\\033[?1006h\\033[?2004h\\033[?25l\\033[>1uFULL'; printf '\\033c'; printf 'after-reset'; ";
    let mut failures = late_mode_mismatches("after RIS", script, "after-reset");
    let d = Daemon::new();
    d.run_sh(24, 80, &format!("{script}exec cat"));
    wait_until("the output", || d.plain().contains("after-reset"));
    let stats = d.stats();
    for stale in ["\"sgrMouse\":true", "\"cursorHidden\":true", "\"kittyKeyboard\":true"] {
        if stats.contains(stale) {
            failures.push(format!("pty stats still reports {stale}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Kitty keyboard flags live per screen: flags a program pushed on the
/// alternate screen go away when it leaves it. A client that attaches after
/// that must hold the primary screen's stack, so the child's next pop leaves
/// it where the session is.
#[test]
#[ignore = "fails on main: the mode record keeps one kitty stack for both screens, so a late client is replayed the alternate screen's push and keeps it after the child pops"]
fn kitty_flags_pushed_on_the_alternate_screen_do_not_come_back_with_a_late_attacher() {
    let d = Daemon::new();
    let body = "printf '\\033[>1u\\033[?1049h\\033[>3uFULL\\033[?1049l'; printf 'left'; ";
    d.run_sh(24, 80, &format!("{body}{}printf '\\033[<u'; printf ' popped'; exec cat", d.gate("pop")));
    wait_until("the program to leave", || d.plain().contains("left"));
    let mut late = d.attach_outer(24, 80);
    late.wait_for_text("left", 10_000).expect("a late client");
    d.open("pop");
    late.wait_for_text("popped", 10_000).expect("the pop");
    let want = bare_modes(&format!("{body}printf '\\033[<u'; printf ' popped'; "), "popped");
    if let Some(m) = mode_diff("after the pop: late client", &mode_state(&mut late), &want) {
        panic!("{m}");
    }
}

/// A mode the child turned on reaches a late client even when it is on in
/// libghostty's defaults and off in the client's terminal (alternate scroll,
/// `?1007`, is off by default in xterm): the replay has to say it, not
/// assume it.
#[test]
#[ignore = "fails on main: the replay only carries modes that differ from libghostty's defaults, so an explicit ?1007h never reaches a terminal that defaults it off"]
fn alternate_scroll_the_child_turned_on_reaches_a_terminal_that_defaults_it_off() {
    let d = Daemon::new();
    d.run_sh(24, 80, &format!("{}printf '\\033[?1049h\\033[?1000h\\033[?1007hFULL'; exec cat", d.gate("go")));
    // Both terminals start with alternate scroll off, as xterm does.
    let attach = format!("printf '\\033[?1007l'; exec '{}' attach {}", pty_bin(), d.name);
    let mut live = d.outer(24, 80, &attach);
    wait_until("the live client", || d.attached() == 1);
    d.open("go");
    live.wait_for_text("FULL", 10_000).expect("the live client");
    let mut late = d.outer(24, 80, &attach);
    late.wait_for_text("FULL", 10_000).expect("the late client");
    let live_on = { live.screenshot(); live.actor().terminal().mode(Mode::ALT_SCROLL).unwrap() };
    let late_on = { late.screenshot(); late.actor().terminal().mode(Mode::ALT_SCROLL).unwrap() };
    assert!(live_on, "the live client did not get ?1007h");
    assert!(late_on, "the late client's terminal still has alternate scroll off");
}
