//! What a child says about how its output should look and behave reaches the
//! terminal a person is looking at: underline styles and underline colours,
//! hyperlinks, cursor shape, blink and colour, the cursor's position and
//! visibility, bells, desktop notifications and progress, window titles and
//! the working directory, focus reporting, and synchronized updates.
//!
//! Every test here drives a real session through the `pty` binary and a real
//! `pty attach` running in a pseudo-terminal of its own. That pseudo-terminal
//! is the "host terminal": every byte the attach client writes is logged and
//! fed to a libghostty terminal, so a test can ask both what arrived and what
//! the host terminal now shows. Two moments matter for each property: while a
//! client is attached (the bytes must pass through unchanged), and when a
//! client attaches after the child already said it (the screen replay must
//! carry it).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use libghostty_vt::render::{CursorVisualStyle, RenderState};
use libghostty_vt::style::{PaletteIndex, RgbColor, Style, StyleColor, Underline};
use libghostty_vt::terminal::{Mode, Point, PointCoordinate};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};
use pty_terminal::{Notification, Range, TerminalActor, TerminalEvent};

const ROWS: u16 = 24;
const COLS: u16 = 80;
const WAIT: Duration = Duration::from_secs(8);

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

/// A private registry for one test, with the sessions it started. Dropping
/// it kills them and removes the directory.
struct Root {
    dir: PathBuf,
    bin: String,
    names: Vec<String>,
}

impl Root {
    fn new() -> Root {
        use_local_pty();
        // Short: a session socket path has to fit 104 bytes.
        let dir = std::env::temp_dir().join(format!("po-{}", pty_testkit::server::random_id()));
        std::fs::create_dir_all(&dir).expect("create the registry");
        Root {
            dir,
            bin: pty_testkit::server::pty_bin(),
            names: Vec::new(),
        }
    }

    /// Start `sh -c <script>` as a detached session at 24x80.
    fn start(&mut self, name: &str, script: &str) {
        self.start_sized(name, script, ROWS, COLS);
    }

    fn start_sized(&mut self, name: &str, script: &str, rows: u16, cols: u16) {
        pty_testkit::server::spawn_daemon(
            &self.bin,
            &self.dir,
            name,
            "sh",
            &["-c", script],
            rows,
            cols,
            None,
            &[],
        )
        .expect("start a session");
        self.names.push(name.to_string());
    }

    /// Run the `pty` CLI against this registry and return its stdout.
    fn pty(&self, args: &[&str]) -> String {
        let out = Command::new(&self.bin)
            .args(args)
            .env("PTY_ROOT", &self.dir)
            .env_remove("PTY_SESSION")
            .env_remove("PTY_SESSION_GENERATION")
            .env_remove("PTY_SESSION_DIR")
            .output()
            .expect("run pty");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// The retained events of one type, as raw JSON lines.
    fn events(&self, name: &str, kind: &str) -> Vec<String> {
        self.pty(&["events", "--recent", "--json", "--type", kind, name])
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Attach a new client in a host terminal of its own.
    fn attach(&self, name: &str) -> Host {
        Host::attach(self, name, ROWS, COLS)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        for name in &self.names {
            for verb in ["kill", "rm"] {
                let _ = Command::new(&self.bin)
                    .args([verb, name])
                    .env("PTY_ROOT", &self.dir)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A host terminal with `pty attach <name>` running in it.
struct Host {
    /// What the host terminal shows: every byte the attach client wrote.
    term: TerminalActor,
    /// Those bytes, verbatim.
    log: Vec<u8>,
    /// Bells, notifications and titles the host terminal saw.
    events: Vec<TerminalEvent>,
    rx: Receiver<Vec<u8>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl Host {
    fn attach(root: &Root, name: &str, rows: u16, cols: u16) -> Host {
        let pair = pty_spawn::open(rows, cols).expect("open the host pty");
        let mut cmd = CommandBuilder::new(&root.bin);
        cmd.args(["attach", name]);
        cmd.env("PTY_ROOT", &root.dir);
        cmd.env("TERM", "xterm-256color");
        // Not nested: the attach must not think it runs inside a session.
        for key in ["PTY_SESSION", "PTY_SESSION_GENERATION", "PTY_SESSION_DIR"] {
            cmd.env_remove(key);
        }
        let child = pair.slave.spawn_command(cmd).expect("spawn pty attach");
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().expect("host reader");
        let writer = pair.master.take_writer().expect("host writer");
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 16384];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Host {
            term: TerminalActor::new(rows, cols, pty_terminal::actor::DEFAULT_SCROLLBACK),
            log: Vec::new(),
            events: Vec::new(),
            rx,
            writer,
            master: pair.master,
            child,
        }
    }

    /// Move everything the attach client wrote so far into the host
    /// terminal, and answer its questions the way a real terminal would.
    fn pump(&mut self) {
        while let Ok(chunk) = self.rx.try_recv() {
            self.log.extend_from_slice(&chunk);
            self.term.write(&chunk);
        }
        self.events.extend(self.term.take_events());
        let replies = self.term.take_pty_replies();
        if !replies.is_empty() {
            let _ = self.writer.write_all(&replies);
            let _ = self.writer.flush();
        }
    }

    fn wait_until(&mut self, what: &str, ok: impl Fn(&Host) -> bool) {
        let deadline = Instant::now() + WAIT;
        loop {
            self.pump();
            if ok(self) {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {what}\nhost screen:\n{}\nhost bytes: {:?}",
                    self.screen(),
                    String::from_utf8_lossy(&self.log)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_text(&mut self, text: &str) {
        self.wait_until(&format!("{text:?} on the host screen"), |h| h.screen().contains(text));
    }

    /// Everything on the host terminal's active screen, history included.
    fn screen(&self) -> String {
        self.term.plain(Range::Full)
    }

    /// Keystrokes from the person at the host terminal.
    fn send(&mut self, keys: &str) {
        self.writer.write_all(keys.as_bytes()).expect("type into the host");
        self.writer.flush().expect("flush the host");
    }

    /// Resize the host terminal, the way a person dragging a window would.
    fn resize(&mut self, rows: u16, cols: u16) {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize the host");
        self.term.resize(cols, rows);
    }

    /// Press the detach key and wait for the attach client to leave.
    fn detach(&mut self) {
        self.send("\x1c");
        let deadline = Instant::now() + WAIT;
        while !matches!(self.child.try_wait(), Ok(Some(_))) {
            assert!(Instant::now() < deadline, "the attach client did not detach");
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(50));
        self.pump();
    }

    fn bells(&self) -> usize {
        self.events.iter().filter(|e| matches!(e, TerminalEvent::Bell)).count()
    }

    fn notifications(&self) -> Vec<Notification> {
        self.events
            .iter()
            .filter_map(|e| match e {
                TerminalEvent::Notification(n) => Some(n.clone()),
                _ => None,
            })
            .collect()
    }

    fn received(&self, bytes: &[u8]) -> bool {
        self.log.windows(bytes.len()).any(|w| w == bytes)
    }

    fn count(&self, bytes: &[u8]) -> usize {
        self.log.windows(bytes.len()).filter(|w| *w == bytes).count()
    }

    /// `(x, y, visible)` of the host terminal's cursor.
    fn cursor(&self) -> (u16, u16, bool) {
        self.term.cursor()
    }

    /// The shape and blink of the host terminal's cursor.
    fn cursor_look(&self) -> (CursorVisualStyle, bool) {
        let mut rs = RenderState::new().expect("render state");
        let snap = rs.update(self.term.terminal()).expect("render update");
        (
            snap.cursor_visual_style().expect("cursor style"),
            snap.cursor_blinking().expect("cursor blink"),
        )
    }

    fn cursor_color(&self) -> Option<RgbColor> {
        self.term.terminal().cursor_color().expect("cursor colour")
    }

    fn mode(&self, mode: Mode) -> bool {
        self.term.terminal().mode(mode).expect("mode")
    }

    /// The first cell of `needle` on the host's active screen.
    fn cell_of(&self, needle: &str) -> (u16, u32) {
        let viewport = self.term.plain(Range::Viewport);
        for (y, line) in viewport.split('\n').enumerate() {
            if let Some(i) = line.find(needle) {
                return (line[..i].chars().count() as u16, y as u32);
            }
        }
        panic!("{needle:?} is not on the host screen:\n{viewport}");
    }

    fn style_of(&self, needle: &str) -> Style {
        let (x, y) = self.cell_of(needle);
        self.term
            .terminal()
            .grid_ref(Point::Active(PointCoordinate { x, y }))
            .expect("grid ref")
            .style()
            .expect("cell style")
    }

    fn link_of(&self, needle: &str) -> String {
        let (x, y) = self.cell_of(needle);
        let mut buf = vec![0u8; 4096];
        let n = self
            .term
            .terminal()
            .grid_ref(Point::Active(PointCoordinate { x, y }))
            .expect("grid ref")
            .hyperlink_uri(&mut buf)
            .expect("hyperlink");
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn rgb(r: u8, g: u8, b: u8) -> StyleColor {
    StyleColor::Rgb(RgbColor { r, g, b })
}

// ── underline styles and underline colour ──

/// The styled line every underline test draws: each underline shape, an
/// RGB underline colour in both the colon and the semicolon spelling, a
/// palette underline colour, SGR 59 / 24 resets, and every attribute mixed
/// into one sequence.
const UNDERLINES: &str = r"printf '\033[4:3;58:2::255:0:0mCURLY\033[0m \033[4:2mDOUBLE\033[0m \033[4:4mDOTTED\033[0m \033[4:5mDASHED\033[0m\n'; printf '\033[38;2;0;255;0m\033[4:3m\033[58;2;255;0;0mGREENTEXT\033[59m\033[24m\033[39m PLAIN \033[4:3m\033[58:5:196mINDEXED\033[0m \033[38;2;255;255;255;4:3;58;2;255;95;95mMIXED\033[0m\n'";

fn assert_underlines(host: &Host) {
    let curly = host.style_of("CURLY");
    assert_eq!(curly.underline, Underline::Curly, "CURLY: {curly:?}");
    assert_eq!(curly.underline_color, rgb(255, 0, 0), "CURLY: {curly:?}");
    assert_eq!(host.style_of("DOUBLE").underline, Underline::Double);
    assert_eq!(host.style_of("DOTTED").underline, Underline::Dotted);
    assert_eq!(host.style_of("DASHED").underline, Underline::Dashed);

    let green = host.style_of("GREENTEXT");
    assert_eq!(green.fg_color, rgb(0, 255, 0), "GREENTEXT: {green:?}");
    assert_eq!(green.underline, Underline::Curly, "GREENTEXT: {green:?}");
    assert_eq!(
        green.underline_color,
        rgb(255, 0, 0),
        "the underline keeps its own colour, not the text's: {green:?}"
    );
    let plain = host.style_of("PLAIN");
    assert_eq!(plain.underline, Underline::None, "SGR 24 ends it: {plain:?}");
    assert_eq!(plain.underline_color, StyleColor::None, "SGR 59 ends it: {plain:?}");

    let indexed = host.style_of("INDEXED");
    assert_eq!(indexed.underline, Underline::Curly, "INDEXED: {indexed:?}");
    assert_eq!(
        indexed.underline_color,
        StyleColor::Palette(PaletteIndex(196)),
        "INDEXED: {indexed:?}"
    );

    let mixed = host.style_of("MIXED");
    assert_eq!(mixed.fg_color, rgb(255, 255, 255), "MIXED: {mixed:?}");
    assert_eq!(mixed.underline, Underline::Curly, "MIXED: {mixed:?}");
    assert_eq!(mixed.underline_color, rgb(255, 95, 95), "MIXED: {mixed:?}");
}

#[test]
fn underline_styles_and_colours_reach_an_attached_client_unchanged() {
    let mut root = Root::new();
    let script = format!("stty -echo; printf 'READY\\n'; read x; {UNDERLINES}; printf 'DONE\\n'; exec cat");
    root.start("ul-live", &script);
    let mut host = root.attach("ul-live");
    host.wait_for_text("READY");
    host.send("\r");
    host.wait_for_text("DONE");

    // The child's own bytes, parameters and separators intact.
    for seq in [
        &b"\x1b[4:3;58:2::255:0:0mCURLY"[..],
        b"\x1b[58;2;255;0;0mGREENTEXT\x1b[59m\x1b[24m",
        b"\x1b[58:5:196mINDEXED",
        b"\x1b[38;2;255;255;255;4:3;58;2;255;95;95mMIXED",
    ] {
        assert!(
            host.received(seq),
            "{:?} did not reach the host unchanged",
            String::from_utf8_lossy(seq)
        );
    }
    assert_underlines(&host);
}

#[test]
fn underline_styles_and_colours_survive_a_reattach() {
    let mut root = Root::new();
    let script = format!("stty -echo; printf 'READY\\n'; read x; {UNDERLINES}; printf 'DONE\\n'; exec cat");
    root.start("ul-again", &script);
    let mut first = root.attach("ul-again");
    first.wait_for_text("READY");
    first.send("\r");
    first.wait_for_text("DONE");
    assert_underlines(&first);
    first.detach();

    // A new client, filled only by the daemon's replay.
    let mut second = root.attach("ul-again");
    second.wait_for_text("DONE");
    assert_underlines(&second);
}

#[test]
#[ignore = "fails on main: the screen replay does not restore the SGR pen, so text printed after an attach loses the undercurl and its colour"]
fn an_underline_turned_on_before_a_client_attaches_still_underlines_later_text() {
    let mut root = Root::new();
    // The child turns the undercurl on, prints part of a line, and finishes
    // it later without repeating the SGR — what a program streaming styled
    // text in pieces does.
    root.start(
        "ul-pen",
        r"stty -echo; printf '\033[4:3;58:2::255:0:0mBEFORE '; read x; printf 'AFTER\033[0m\n'; exec cat",
    );
    let mut host = root.attach("ul-pen");
    host.wait_for_text("BEFORE");
    let before = host.style_of("BEFORE");
    assert_eq!(before.underline, Underline::Curly, "{before:?}");
    host.send("\r");
    host.wait_for_text("AFTER");
    let after = host.style_of("AFTER");
    assert_eq!(
        (after.underline, after.underline_color),
        (Underline::Curly, rgb(255, 0, 0)),
        "text the child printed after the attach lost the underline it had turned on: {after:?}"
    );
}

// ── cursor shape, blink and colour ──

#[test]
fn cursor_shape_blink_and_colour_reach_an_attached_client() {
    let mut root = Root::new();
    root.start(
        "cur-live",
        r"stty -echo; printf 'READY\n'; read x; printf '\033[6 qBAR\n'; read x; printf '\033[5 qBLINKBAR\n'; read x; printf '\033[4 qUNDER\n'; read x; printf '\033]12;#ff5f5f\033\\COLOURED\n'; read x; printf '\033]112\033\\\033[0 qRESET\n'; exec cat",
    );
    let mut host = root.attach("cur-live");
    host.wait_for_text("READY");
    let (default_shape, default_blink) = host.cursor_look();
    let default_colour = host.cursor_color();

    host.send("\r");
    host.wait_for_text("BAR");
    assert_eq!(host.cursor_look(), (CursorVisualStyle::Bar, false), "DECSCUSR 6");
    assert!(host.received(b"\x1b[6 q"));

    host.send("\r");
    host.wait_for_text("BLINKBAR");
    assert_eq!(host.cursor_look(), (CursorVisualStyle::Bar, true), "DECSCUSR 5");

    host.send("\r");
    host.wait_for_text("UNDER");
    assert_eq!(host.cursor_look(), (CursorVisualStyle::Underline, false), "DECSCUSR 4");

    host.send("\r");
    host.wait_for_text("COLOURED");
    assert_eq!(host.cursor_color(), Some(RgbColor { r: 0xff, g: 0x5f, b: 0x5f }), "OSC 12");
    assert!(host.received(b"\x1b]12;#ff5f5f\x1b\\"));

    host.send("\r");
    host.wait_for_text("RESET");
    assert_eq!(host.cursor_color(), default_colour, "OSC 112 gives the host its own colour back");
    assert_eq!(host.cursor_look(), (default_shape, default_blink), "DECSCUSR 0");
}

#[test]
#[ignore = "fails on main: the screen replay carries no DECSCUSR, so a reattached client shows a block where the child asked for a bar"]
fn a_bar_cursor_is_restored_for_a_client_that_reattaches() {
    let mut root = Root::new();
    root.start(
        "cur-shape",
        r"stty -echo; printf 'READY\n'; read x; printf '\033[6 qBAR\n'; exec cat",
    );
    let mut first = root.attach("cur-shape");
    first.wait_for_text("READY");
    first.send("\r");
    first.wait_for_text("BAR");
    assert_eq!(first.cursor_look(), (CursorVisualStyle::Bar, false));
    first.detach();

    let mut second = root.attach("cur-shape");
    second.wait_for_text("BAR");
    assert_eq!(
        second.cursor_look(),
        (CursorVisualStyle::Bar, false),
        "the replay lost the cursor shape; replay bytes: {:?}",
        String::from_utf8_lossy(&second.log)
    );
}

#[test]
#[ignore = "fails on main: the screen replay carries no OSC 12, so a reattached client loses the cursor colour the child set"]
fn the_cursor_colour_is_restored_for_a_client_that_reattaches() {
    let mut root = Root::new();
    root.start(
        "cur-colour",
        r"stty -echo; printf 'READY\n'; read x; printf '\033]12;#ff5f5f\033\\COLOURED\n'; exec cat",
    );
    let mut first = root.attach("cur-colour");
    first.wait_for_text("READY");
    first.send("\r");
    first.wait_for_text("COLOURED");
    let red = Some(RgbColor { r: 0xff, g: 0x5f, b: 0x5f });
    assert_eq!(first.cursor_color(), red);
    first.detach();

    let mut second = root.attach("cur-colour");
    second.wait_for_text("COLOURED");
    assert_eq!(
        second.cursor_color(),
        red,
        "the replay lost the cursor colour; replay bytes: {:?}",
        String::from_utf8_lossy(&second.log)
    );
}

// ── the host cursor sits at the child's cursor ──

#[test]
fn the_host_cursor_follows_the_child_cursor_and_its_visibility() {
    let mut root = Root::new();
    root.start(
        "at-live",
        r"stty -echo; printf 'READY\n'; read x; printf '\033[10;20HAT-TEN-TWENTY\033[5;7H'; read x; printf '\033[?25l'; read x; printf '\033[12;3H\033[?25h'; exec cat",
    );
    let mut host = root.attach("at-live");
    host.wait_for_text("READY");

    host.send("\r");
    host.wait_for_text("AT-TEN-TWENTY");
    host.wait_until("the cursor at row 5, column 7", |h| h.cursor() == (6, 4, true));

    host.send("\r");
    host.wait_until("the cursor hidden where it was", |h| h.cursor() == (6, 4, false));

    host.send("\r");
    host.wait_until("the cursor shown at row 12, column 3", |h| h.cursor() == (2, 11, true));

    // Nothing but the child moved or toggled the cursor: the bytes the
    // child wrote arrived as one run, with no hide/show or move around them.
    assert!(host.received(b"\x1b[10;20HAT-TEN-TWENTY\x1b[5;7H"));
    assert_eq!(host.count(b"\x1b[?25l"), 1, "only the child's own hide");
    assert_eq!(host.count(b"\x1b[?25h"), 1, "only the child's own show");
}

#[test]
fn a_client_that_attaches_later_puts_the_host_cursor_where_the_child_left_it() {
    let mut root = Root::new();
    root.start("at-shown", r"printf 'READY\n\033[10;20HMARK\033[5;7H'; exec cat");
    root.start("at-hidden", r"printf 'READY\n\033[10;20HMARK\033[5;7H\033[?25l'; exec cat");
    root.start(
        "at-alt",
        r"printf 'NORMAL\n\033[?1049h\033[HFULLSCREEN\033[8;30H'; exec cat",
    );

    let mut shown = root.attach("at-shown");
    shown.wait_for_text("MARK");
    shown.wait_until("the cursor at row 5, column 7, visible", |h| h.cursor() == (6, 4, true));

    let mut hidden = root.attach("at-hidden");
    hidden.wait_for_text("MARK");
    hidden.wait_until("the cursor at row 5, column 7, hidden", |h| h.cursor() == (6, 4, false));

    let mut alt = root.attach("at-alt");
    alt.wait_for_text("FULLSCREEN");
    alt.wait_until("the cursor at row 8, column 30 of the full-screen program", |h| {
        h.cursor() == (29, 7, true)
    });
}

// ── hyperlinks ──

const LINKS: &str = r"printf '\033]8;;https://example.com/a\033\\LINK-A\033]8;;\033\\ PLAIN \033]8;id=b;https://example.com/b\033\\LINK-B\033]8;;\033\\\n'";

#[test]
fn hyperlinks_reach_an_attached_client() {
    let mut root = Root::new();
    let script = format!("stty -echo; printf 'READY\\n'; read x; {LINKS}; printf 'DONE\\n'; exec cat");
    root.start("ln-live", &script);
    let mut host = root.attach("ln-live");
    host.wait_for_text("READY");
    host.send("\r");
    host.wait_for_text("DONE");

    assert!(host.received(b"\x1b]8;;https://example.com/a\x1b\\LINK-A\x1b]8;;\x1b\\"));
    assert!(host.received(b"\x1b]8;id=b;https://example.com/b\x1b\\LINK-B"));
    assert_eq!(host.link_of("LINK-A"), "https://example.com/a");
    assert_eq!(host.link_of("PLAIN"), "");
    assert_eq!(host.link_of("LINK-B"), "https://example.com/b");
}

#[test]
#[ignore = "fails on main: the screen replay drops OSC 8, so a reattached client shows the link text without the link"]
fn hyperlinks_survive_a_reattach() {
    let mut root = Root::new();
    let script = format!("stty -echo; printf 'READY\\n'; read x; {LINKS}; printf 'DONE\\n'; exec cat");
    root.start("ln-again", &script);
    let mut first = root.attach("ln-again");
    first.wait_for_text("READY");
    first.send("\r");
    first.wait_for_text("DONE");
    assert_eq!(first.link_of("LINK-A"), "https://example.com/a");
    first.detach();

    let mut second = root.attach("ln-again");
    second.wait_for_text("DONE");
    assert_eq!(
        second.link_of("LINK-A"),
        "https://example.com/a",
        "the replay dropped the hyperlink; replay bytes: {:?}",
        String::from_utf8_lossy(&second.log)
    );
    assert_eq!(second.link_of("LINK-B"), "https://example.com/b");
}

/// A full bottom row of hyperlinks, then a scrolling region above it that
/// scrolls `scrolls` times, then `SURVIVED` inside the region.
fn links_under_a_scrolling_region(scrolls: usize) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(b"\x1b[40;1H");
    for i in 0..30 {
        payload.extend_from_slice(format!("\x1b]8;;https://example.com/{i}\x1b\\LNK \x1b]8;;\x1b\\").as_bytes());
    }
    payload.extend_from_slice(b"\x1b[1;30r\x1b[1;1H");
    for _ in 0..scrolls {
        payload.extend_from_slice(b"hello\r\n\x1b[K");
    }
    payload.extend_from_slice(b"\x1b[r\x1b[35;1HSURVIVED");
    payload
}

#[test]
#[ignore = "fails on main: libghostty aborts in Screen.clearCells on this output and the session's daemon dies"]
fn a_row_of_hyperlinks_below_a_scrolling_region_does_not_take_the_session_down() {
    let mut root = Root::new();
    let file = root.dir.join("links.bin");
    std::fs::write(&file, links_under_a_scrolling_region(600)).expect("write the payload");
    root.start_sized(
        "ln-scroll",
        &format!("cat '{}'; exec cat", file.display()),
        40,
        120,
    );

    let mut host = Host::attach(&root, "ln-scroll", 40, 120);
    host.wait_for_text("SURVIVED");
    host.wait_for_text("LNK LNK LNK");
    host.send("still-here\r");
    host.wait_for_text("still-here");
}

/// The same output fed to libghostty alone, in a spawn-mode session: no
/// daemon, no replay, no client. Running it aborts the test process.
#[test]
#[ignore = "fails on main: libghostty alone aborts (Screen.clearCells assertion) on a hyperlink row below a scrolling region; aborts this test process"]
fn a_row_of_hyperlinks_below_a_scrolling_region_in_the_terminal_alone() {
    let dir = std::env::temp_dir().join(format!("po-{}", pty_testkit::server::random_id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let file = dir.join("links.bin");
    std::fs::write(&file, links_under_a_scrolling_region(600)).expect("write the payload");
    let path = file.display().to_string();
    let mut s = pty_testkit::Session::spawn(
        "sh",
        &["-c", &format!("cat '{path}'; exec cat")],
        pty_testkit::SpawnOptions {
            rows: Some(40),
            cols: Some(120),
            ..Default::default()
        },
    )
    .expect("spawn");
    let seen = s.wait_for_text("SURVIVED", 30_000);
    s.close();
    let _ = std::fs::remove_dir_all(&dir);
    seen.expect("the terminal took the output");
}

// ── bells ──

#[test]
fn a_bell_from_the_child_rings_every_attached_client() {
    let mut root = Root::new();
    root.start(
        "bell-two",
        r"stty -echo; printf 'READY\n'; read x; printf 'RING\a\n'; exec cat",
    );
    let mut one = root.attach("bell-two");
    one.wait_for_text("READY");
    let mut two = root.attach("bell-two");
    two.wait_for_text("READY");

    one.send("\r");
    one.wait_for_text("RING");
    two.wait_for_text("RING");
    assert_eq!(one.bells(), 1, "the client that typed");
    assert_eq!(two.bells(), 1, "the other client, which did nothing");
    assert!(one.received(b"RING\x07") && two.received(b"RING\x07"));

    let deadline = Instant::now() + WAIT;
    while root.events("bell-two", "bell").len() != 1 {
        assert!(Instant::now() < deadline, "the session logged no bell");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_bel_that_ends_an_osc_string_is_not_a_bell() {
    let mut root = Root::new();
    root.start(
        "bell-osc",
        r"stty -echo; printf 'READY\n'; read x; i=0; while [ $i -lt 5 ]; do printf '\033]0;title %s\a' $i; i=$((i+1)); done; printf '\033]2;split'; sleep 0.3; printf ' title\a'; printf '\033]7;file://localhost/tmp\a'; printf 'TITLES-DONE\n'; read x; printf 'RING\a\n'; exec cat",
    );
    let mut host = root.attach("bell-osc");
    host.wait_for_text("READY");

    host.send("\r");
    host.wait_for_text("TITLES-DONE");
    // Give a stray bell time to arrive before saying there was none.
    std::thread::sleep(Duration::from_millis(300));
    host.pump();
    assert_eq!(host.bells(), 0, "an OSC terminator rang the host's bell");
    assert_eq!(host.term.title(), "split title", "the split title arrived whole");
    assert!(!host.screen().contains("title"), "title text leaked: {}", host.screen());
    assert!(root.events("bell-osc", "bell").is_empty(), "the session logged a bell");

    host.send("\r");
    host.wait_for_text("RING");
    assert_eq!(host.bells(), 1, "a bare BEL still rings");
    let deadline = Instant::now() + WAIT;
    while root.events("bell-osc", "bell").len() != 1 {
        assert!(Instant::now() < deadline, "the session logged no bell");
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ── desktop notifications and progress ──

#[test]
fn notifications_and_progress_reach_an_attached_client_unchanged() {
    let mut root = Root::new();
    root.start(
        "notify",
        r"stty -echo; printf 'READY\n'; read x; printf '\033]9;Build done\033\\'; printf '\033]99;i=1:d=0;Metadata title\033\\'; printf '\033]99;i=1:p=body;Metadata body\033\\'; printf '\033]777;notify;Title;Body\033\\'; printf '\033]9;4;1;50\033\\'; printf '\033]9;4;0\a'; printf 'NOTIFIED\n'; exec cat",
    );
    let mut host = root.attach("notify");
    host.wait_for_text("READY");
    host.send("\r");
    host.wait_for_text("NOTIFIED");

    for seq in [
        &b"\x1b]9;Build done\x1b\\"[..],
        b"\x1b]99;i=1:d=0;Metadata title\x1b\\",
        b"\x1b]99;i=1:p=body;Metadata body\x1b\\",
        b"\x1b]777;notify;Title;Body\x1b\\",
        b"\x1b]9;4;1;50\x1b\\",
        b"\x1b]9;4;0\x07",
    ] {
        assert!(
            host.received(seq),
            "{:?} did not reach the host unchanged",
            String::from_utf8_lossy(seq)
        );
    }
    let seen = host.notifications();
    assert!(
        seen.iter().any(|n| n.source == "osc9" && n.body.as_deref() == Some("Build done")),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|n| n.source == "osc777"
            && n.title.as_deref() == Some("Title")
            && n.body.as_deref() == Some("Body")),
        "{seen:?}"
    );
    assert!(!host.screen().contains("Build done"), "{}", host.screen());

    let deadline = Instant::now() + WAIT;
    while root.events("notify", "notification").len() < 2 {
        assert!(Instant::now() < deadline, "the session logged no notification");
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ── titles and working directory ──

#[test]
fn titles_and_the_working_directory_reach_the_client_and_never_show_as_text() {
    let mut root = Root::new();
    root.start(
        "title",
        r"stty -echo; printf 'READY\n'; read x; printf '\033]0;first title\a'; printf '\033]2;second title\033\\'; printf '\033]7;file://localhost/tmp/some/dir\033\\'; printf 'AFTER-TITLES\n'; exec cat",
    );
    let mut host = root.attach("title");
    host.wait_for_text("READY");
    host.send("\r");
    host.wait_for_text("AFTER-TITLES");

    assert!(host.received(b"\x1b]0;first title\x07"));
    assert!(host.received(b"\x1b]2;second title\x1b\\"));
    assert!(host.received(b"\x1b]7;file://localhost/tmp/some/dir\x1b\\"));
    assert_eq!(host.term.title(), "second title");
    assert_eq!(host.term.terminal().pwd().unwrap(), "file://localhost/tmp/some/dir");
    let screen = host.screen();
    for leak in ["title", "file:", "]0;", "]7;"] {
        assert!(!screen.contains(leak), "{leak:?} shown as text:\n{screen}");
    }
    let peek = root.pty(&["peek", "--plain", "title"]);
    assert!(!peek.contains("title") && !peek.contains("file:"), "{peek}");

    // The session keeps track of the title itself.
    let deadline = Instant::now() + WAIT;
    loop {
        let titles = root.events("title", "title_change");
        if titles.len() == 2
            && titles[0].contains("first title")
            && titles[1].contains("second title")
        {
            break;
        }
        assert!(Instant::now() < deadline, "title changes not tracked: {titles:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn titles_after_a_full_screen_program_exits_do_not_show_as_text() {
    let mut root = Root::new();
    root.start(
        "title-alt",
        r"stty -echo; printf 'READY\n'; read x; printf '\033[?1049h\033[HFULLSCREEN'; read x; printf '\033[?1049l'; printf '\033]2;user@host: ~\a\033]7;file://localhost/home\033\\'; printf 'PROMPT$ '; exec cat",
    );
    let mut host = root.attach("title-alt");
    host.wait_for_text("READY");
    host.send("\r");
    host.wait_for_text("FULLSCREEN");

    // A client that arrives while the full-screen program runs.
    let mut late = root.attach("title-alt");
    late.wait_for_text("FULLSCREEN");

    host.send("\r");
    for h in [&mut host, &mut late] {
        h.wait_for_text("PROMPT$");
        let screen = h.screen();
        assert!(!screen.contains("user@host"), "the title leaked:\n{screen}");
        assert!(!screen.contains("file:"), "the cwd leaked:\n{screen}");
        assert!(!screen.contains("FULLSCREEN"), "the full-screen program is gone:\n{screen}");
        assert_eq!(h.term.title(), "user@host: ~");
    }
}

/// bash in vi mode with a title-setting prompt, driven through the same
/// keys twice: once in a session seen through `pty attach`, once directly
/// in a terminal. Recalling a line with `/` search redraws the prompt; the
/// two screens and titles must be the same.
#[test]
fn a_title_setting_prompt_redraws_as_in_a_bare_terminal_during_vi_history_search() {
    let mut root = Root::new();
    let cwd = root.dir.join("a-fairly-long-directory-name").join("and-another-long-component-for-width");
    std::fs::create_dir_all(&cwd).expect("prompt directory");
    let cwd_s = cwd.display().to_string();
    let env = |hist: &str| {
        vec![
            ("PS1".to_string(), r"\[\e]0;\u@\h: \w\a\]\u@\h:\w\$ ".to_string()),
            ("HISTFILE".to_string(), root.dir.join(hist).display().to_string()),
            ("INPUTRC".to_string(), "/dev/null".to_string()),
            ("TERM".to_string(), "xterm-256color".to_string()),
        ]
    };
    let args = ["--norc", "--noprofile", "-o", "vi"];

    pty_testkit::server::spawn_daemon(
        &root.bin,
        &root.dir,
        "vi-prompt",
        "bash",
        &args,
        ROWS,
        COLS,
        Some(&cwd_s),
        &env("hist-session"),
    )
    .expect("start bash in a session");
    root.names.push("vi-prompt".to_string());
    let mut host = root.attach("vi-prompt");
    let mut bare = pty_testkit::Session::spawn(
        "bash",
        &args,
        pty_testkit::SpawnOptions {
            rows: Some(ROWS),
            cols: Some(COLS),
            cwd: Some(cwd.clone()),
            env: env("hist-bare"),
        },
    )
    .expect("start bash in a bare terminal");

    host.wait_for_text("$ ");
    bare.wait_for_text("$ ", 8000).expect("bare prompt");
    let trim = |s: &str| s.lines().map(str::trim_end).collect::<Vec<_>>().join("\n");
    let mut step = |keys: &[&str], done: &dyn Fn(&str) -> bool, what: &str| {
        for k in keys {
            host.send(k);
            bare.type_str(k);
            // A lone ESC has to outlast readline's key-sequence timeout
            // (500 ms) or it is read as a Meta prefix.
            let pause = if *k == "\x1b" { 800 } else { 150 };
            std::thread::sleep(Duration::from_millis(pause));
        }
        host.wait_until(what, |h| done(&h.term.plain(Range::Viewport)));
        bare.wait_for(|ss| done(&ss.text), 8000, what).expect(what);
        std::thread::sleep(Duration::from_millis(300));
        host.pump();
        let host_view = trim(&host.term.plain(Range::Viewport));
        let bare_view = trim(&bare.actor().plain(Range::Viewport));
        assert_eq!(host_view, bare_view, "{what}: the session's screen differs from a bare terminal's");
        assert!(!host_view.contains("]0;"), "the title leaked:\n{host_view}");
    };
    step(
        &["echo first-command\r", "echo second-command\r"],
        &|s: &str| s.contains("second-command\n"),
        "two commands in history",
    );
    // Search mode takes over the prompt line; whatever bash draws for it,
    // the session must draw the same.
    step(
        &["\x1b", "/", "first"],
        &|s: &str| s.trim_end().ends_with("first"),
        "the search line",
    );
    // Accepting it redraws the whole prompt with the recalled command.
    step(
        &["\r"],
        &|s: &str| s.matches("echo first-command").count() == 2,
        "the recalled line under a redrawn prompt",
    );
    assert_eq!(host.term.title(), bare.title());
    assert!(host.term.title().ends_with("and-another-long-component-for-width"), "{}", host.term.title());
    bare.close();
}

// ── focus reporting ──

/// A child that turns focus reporting on and off on command and shows every
/// other line it reads, control characters made visible.
const FOCUS_ECHO: &str = r#"stty -echo; printf 'READY\n'; while IFS= read -r line; do case "$line" in on) printf '\033[?1004h'; echo ON;; off) printf '\033[?1004l'; echo OFF;; *) printf 'GOT:%s:\n' "$(printf '%s' "$line" | cat -v)";; esac; done"#;

#[test]
fn focus_reports_reach_the_child_only_while_it_asks_for_them() {
    let mut root = Root::new();
    root.start("focus", FOCUS_ECHO);
    let mut host = root.attach("focus");
    host.wait_for_text("READY");
    assert!(!host.mode(Mode::FOCUS_EVENT), "the host reports focus before anyone asked");
    assert_eq!(host.term.encode_focus(false), None);

    host.send("on\r");
    host.wait_for_text("ON");
    host.wait_until("the host reporting focus", |h| h.mode(Mode::FOCUS_EVENT));

    // The person switches away from the host terminal and back.
    let out = host.term.encode_focus(false).expect("focus-out report");
    host.send(std::str::from_utf8(&out).unwrap());
    host.send("\r");
    host.wait_for_text("GOT:^[[O:");
    let back = host.term.encode_focus(true).expect("focus-in report");
    host.send(std::str::from_utf8(&back).unwrap());
    host.send("\r");
    host.wait_for_text("GOT:^[[I:");

    host.send("off\r");
    host.wait_for_text("OFF");
    host.wait_until("the host no longer reporting focus", |h| !h.mode(Mode::FOCUS_EVENT));
    assert_eq!(host.term.encode_focus(false), None);
    assert_eq!(host.screen().matches("GOT:").count(), 2, "{}", host.screen());
}

#[test]
fn a_client_that_attaches_later_reports_focus_to_a_child_that_asked() {
    let mut root = Root::new();
    root.start("focus-late", FOCUS_ECHO);
    let mut first = root.attach("focus-late");
    first.wait_for_text("READY");
    first.send("on\r");
    first.wait_for_text("ON");
    first.detach();
    assert!(!first.mode(Mode::FOCUS_EVENT), "detach leaves the host reporting focus");

    let mut second = root.attach("focus-late");
    second.wait_for_text("ON");
    second.wait_until("the replay turning focus reporting on", |h| h.mode(Mode::FOCUS_EVENT));
    let out = second.term.encode_focus(false).expect("focus-out report");
    second.send(std::str::from_utf8(&out).unwrap());
    second.send("\r");
    second.wait_for_text("GOT:^[[O:");
}

#[test]
fn attaching_detaching_resizing_and_a_second_client_send_the_child_no_focus_reports() {
    let mut root = Root::new();
    root.start("focus-quiet", FOCUS_ECHO);
    let mut first = root.attach("focus-quiet");
    first.wait_for_text("READY");
    first.send("on\r");
    first.wait_for_text("ON");

    first.resize(20, 70);
    std::thread::sleep(Duration::from_millis(200));
    let mut second = root.attach("focus-quiet");
    second.wait_for_text("ON");
    second.detach();
    first.resize(ROWS, COLS);
    std::thread::sleep(Duration::from_millis(200));
    first.detach();
    let mut third = root.attach("focus-quiet");
    third.wait_for_text("ON");

    // Any focus report the runtime made up would sit in the child's line
    // buffer ahead of this empty line.
    third.send("\r");
    third.wait_for_text("GOT:");
    std::thread::sleep(Duration::from_millis(200));
    third.pump();
    let screen = third.screen();
    assert_eq!(screen.matches("GOT:").count(), 1, "{screen}");
    assert!(screen.contains("GOT::"), "the child read something it was not sent:\n{screen}");
}

// ── synchronized output ──

#[test]
fn a_synchronized_frame_reaches_the_client_with_its_brackets() {
    let mut root = Root::new();
    root.start(
        "sync-live",
        r"stty -echo; printf 'READY\n'; read x; printf '\033[?2026h\033[2J\033[HFRAME-TOP'; read x; printf '\033[12;1HFRAME-BOTTOM\033[?2026l'; exec cat",
    );
    let mut host = root.attach("sync-live");
    host.wait_for_text("READY");
    assert!(!host.mode(Mode::SYNC_OUTPUT));

    host.send("\r");
    host.wait_for_text("FRAME-TOP");
    assert!(host.mode(Mode::SYNC_OUTPUT), "the host is not holding the half-drawn frame");
    assert!(host.received(b"\x1b[?2026h\x1b[2J\x1b[HFRAME-TOP"));

    host.send("\r");
    host.wait_for_text("FRAME-BOTTOM");
    assert!(!host.mode(Mode::SYNC_OUTPUT), "the frame never ended on the host");
    assert!(host.received(b"\x1b[12;1HFRAME-BOTTOM\x1b[?2026l"));
}

#[test]
fn a_client_that_attaches_mid_frame_holds_the_half_drawn_frame() {
    let mut root = Root::new();
    root.start(
        "sync-late",
        r"stty -echo; printf 'OLD-SCREEN\n'; printf '\033[?2026h\033[2J\033[HHALF-FRAME'; read x; printf '\033[12;1HREST-OF-FRAME\033[?2026l'; exec cat",
    );
    let mut host = root.attach("sync-late");
    host.wait_for_text("HALF-FRAME");
    assert!(
        host.mode(Mode::SYNC_OUTPUT),
        "the replay of a half-drawn frame does not keep the host holding it; replay: {:?}",
        String::from_utf8_lossy(&host.log)
    );

    host.send("\r");
    host.wait_for_text("REST-OF-FRAME");
    assert!(!host.mode(Mode::SYNC_OUTPUT));
}
