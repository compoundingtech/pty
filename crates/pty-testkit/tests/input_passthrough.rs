//! Input passthrough: what a person types into `pty attach` reaches the child
//! byte for byte, the host terminal's keyboard, mouse and paste modes are the
//! child's own while a client is attached, programmatic input (`pty send`)
//! arrives whole and paced, and detaching hands the host terminal back the
//! way it was found.
//!
//! The host terminal here is a libghostty terminal: a spawn-mode [`Session`]
//! running `pty attach`. Its modes are whatever the child's output and the
//! client's own bytes made of it, and its encoders produce the bytes a real
//! terminal in that state sends for a key, a click or a paste. The child
//! records what it reads into a file, so every claim about input is an exact
//! byte comparison.
//!
//! Every test uses its own `PTY_ROOT` in a temporary directory.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use libghostty_vt::terminal::Mode;
use pty_core::protocol::encode_data;
use pty_terminal::{
    Key, KeyAction, KeyEvent, Mods, MouseButton, MouseEvent, Range, TerminalActor,
};
use pty_testkit::{Session, SpawnOptions};

/// How long any one wait may take.
const WAIT: u64 = 8000;

/// Environment a test must not pass on: it would tie the processes it starts
/// to whatever session or registry the test itself runs in.
const AMBIENT: [&str; 5] = [
    "PTY_SESSION",
    "PTY_SESSION_GENERATION",
    "PTY_SESSION_DIR",
    "PTY_REAP_ON_EXIT",
    "PTY_CREATION_LOCK_OWNER_PID",
];

// ── harness ──────────────────────────────────────────────────────────────

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

/// A private registry. Every session started in it is killed and the
/// directory removed when it drops.
struct Root {
    dir: PathBuf,
    ids: RefCell<Vec<String>>,
}

impl Root {
    fn new() -> Root {
        use_local_pty();
        // Short: a session socket path has to fit 104 bytes.
        let dir = std::env::temp_dir().join(format!("pi-{}", pty_testkit::server::random_id()));
        std::fs::create_dir_all(&dir).expect("create a registry");
        Root {
            dir,
            ids: RefCell::new(Vec::new()),
        }
    }

    fn pty(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(pty_bin());
        cmd.args(args).env("PTY_ROOT", &self.dir);
        for key in AMBIENT {
            cmd.env_remove(key);
        }
        cmd.output().expect("run pty")
    }

    /// `pty run -d` a `sh -c script` session of `rows` x `cols`.
    fn start_sized(&self, id: &str, rows: u16, cols: u16, script: &str) {
        let (r, c) = (rows.to_string(), cols.to_string());
        let out = self.pty(&[
            "run", "-d", "--no-display-name", "--id", id, "--rows", &r, "--cols", &c, "--", "sh",
            "-c", script,
        ]);
        assert!(
            out.status.success(),
            "pty run failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        self.ids.borrow_mut().push(id.to_string());
    }

    fn start(&self, id: &str, script: &str) {
        self.start_sized(id, 24, 80, script);
    }

    fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn env(&self) -> Vec<(String, String)> {
        vec![("PTY_ROOT".to_string(), self.dir.to_string_lossy().into_owned())]
    }

    /// A libghostty host terminal running `command`.
    fn host(&self, command: &str, args: &[&str], rows: u16, cols: u16) -> Session {
        Session::spawn(
            command,
            args,
            SpawnOptions {
                rows: Some(rows),
                cols: Some(cols),
                env: self.env(),
                ..Default::default()
            },
        )
        .expect("spawn a host terminal")
    }

    /// `pty attach <id>` in a fresh 24x80 host terminal.
    fn attach(&self, id: &str) -> Session {
        self.host(&pty_bin(), &["attach", id], 24, 80)
    }

    /// A host whose shell runs `pty attach <id>` and, once the client has
    /// exited, writes `after` (a printf format) to the host terminal the way
    /// the next program run from that shell would, then says `HOST-AFTER`.
    fn attach_then(&self, id: &str, after: &str) -> Session {
        let script =
            format!("\"$0\" attach {id}; printf '{after}'; printf 'HOST-AFTER\\r\\n'; exec sleep 30");
        self.host("sh", &["-c", &script, &pty_bin()], 24, 80)
    }

    fn send(&self, id: &str, args: &[&str]) -> Output {
        let mut all = vec!["send", id];
        all.extend_from_slice(args);
        self.pty(&all)
    }

    /// Wait for a file a child creates once it is ready for input.
    fn wait_for_file(&self, name: &str) {
        let path = self.file(name);
        let deadline = Instant::now() + Duration::from_millis(WAIT);
        while !path.exists() {
            assert!(Instant::now() < deadline, "{} never appeared", path.display());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn daemon_pid(&self, id: &str) -> String {
        std::fs::read_to_string(self.file(&format!("{id}.pid")))
            .expect("the daemon's pid file")
            .trim()
            .to_string()
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        for id in self.ids.borrow().iter() {
            let _ = self.pty(&["kill", id]);
            let _ = self.pty(&["rm", id]);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A child that records every byte it reads to `out`.
///
/// It says `WAITING`, waits for one line in cooked mode, then puts its
/// terminal in raw mode, writes `modes` (a printf format, so `\033` is ESC),
/// says `READY` and copies its input to `out` unchanged. The modes are
/// written only after a client is attached, so they reach that client as
/// live output rather than as part of the attach replay.
fn recorder(out: &Path, modes: &str) -> String {
    format!(
        "printf 'WAITING\\r\\n'; IFS= read -r go; stty raw -echo -iexten; \
         printf '{modes}'; printf 'READY\\r\\n'; exec cat > '{}'",
        out.display()
    )
}

/// A recorder for programmatic input: raw mode and `modes` at once, then
/// `ready` is created and everything read goes to `out`.
fn quiet_recorder(out: &Path, ready: &Path, modes: &str) -> String {
    format!(
        "stty raw -echo -iexten; printf '{modes}'; : > '{}'; exec cat > '{}'",
        ready.display(),
        out.display()
    )
}

/// A child that logs each `read(2)` it makes as hex, `|` after each, so the
/// boundaries between reads are visible.
fn read_logger(out: &Path, ready: &Path) -> String {
    format!(
        "stty raw -echo -iexten; printf 'LOGGING\\r\\n'; : > '{}'; \
         while :; do dd bs=65536 count=1 2>/dev/null | od -An -tx1 -v | tr -d ' \\n'; \
         printf '|'; done > '{}'",
        ready.display(),
        out.display()
    )
}

/// Attach to a [`recorder`] and start it: its modes are then live on the
/// host terminal.
fn attach_live(root: &Root, id: &str) -> Session {
    let mut host = root.attach(id);
    start_recording(&mut host);
    host
}

fn start_recording(host: &mut Session) {
    host.wait_for_text("WAITING", WAIT)
        .expect("the attach replayed the screen");
    host.type_str("go\r");
    host.wait_for_text("READY", WAIT)
        .expect("the child is recording");
}

/// Wait until `path` holds at least `len` bytes, then a moment longer so a
/// duplicate would have landed too, and return what it holds.
fn recorded(path: &Path, len: usize) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_millis(WAIT);
    loop {
        let got = std::fs::read(path).unwrap_or_default();
        if got.len() >= len || Instant::now() >= deadline {
            std::thread::sleep(Duration::from_millis(150));
            return std::fs::read(path).unwrap_or_default();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Wait until the text file at `path` equals `want`, and return what it
/// holds when it does or when time runs out.
fn logged(path: &Path, want: &str) -> String {
    let deadline = Instant::now() + Duration::from_millis(WAIT);
    loop {
        let got = std::fs::read_to_string(path).unwrap_or_default();
        if got == want || got.len() > want.len() || Instant::now() >= deadline {
            std::thread::sleep(Duration::from_millis(150));
            return std::fs::read_to_string(path).unwrap_or_default();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Bytes with ESC and the other controls made visible.
fn shown(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '\x1b' => out.push_str("\\e"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Press Ctrl+\ (the detach key) and wait for the client to say it left.
fn detach(host: &mut Session, id: &str) {
    host.type_str("\x1c");
    host.wait_for_text(&format!("[detached from {id}]"), WAIT)
        .expect("the client detached");
}

/// The bytes the host terminal sends for `ev` in its current state.
fn host_key(host: &mut Session, ev: &KeyEvent) -> Vec<u8> {
    host.screenshot();
    host.actor().encode_key(ev)
}

fn alt_a() -> KeyEvent {
    KeyEvent::typed(Key::A, "a", Some('a')).with_mods(Mods::ALT)
}

fn ctrl_backslash(mods: Mods) -> KeyEvent {
    KeyEvent {
        unshifted: Some('\\'),
        ..KeyEvent::press(Key::Backslash)
    }
    .with_mods(Mods::CTRL | mods)
}

fn mode(host: &mut Session, m: Mode) -> bool {
    host.screenshot();
    host.actor().terminal().mode(m).unwrap_or(false)
}

/// A host terminal as it is before anything attaches to it.
fn fresh() -> TerminalActor {
    TerminalActor::new(24, 80, 100)
}

/// What a terminal in the state `modes` puts on the wire for `ev`.
fn encoded_in(modes: &[u8], ev: &KeyEvent) -> Vec<u8> {
    let mut term = fresh();
    term.write(modes);
    term.encode_key(ev)
}

/// A host terminal that can be handed bytes that are not UTF-8 (a mouse
/// report past column 95 in the default encoding, half of a character).
/// Otherwise the same as a spawn-mode [`Session`] running `pty attach`.
struct RawHost {
    actor: TerminalActor,
    writer: Box<dyn Write + Send>,
    rx: Receiver<Vec<u8>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl RawHost {
    fn attach(root: &Root, id: &str, rows: u16, cols: u16) -> RawHost {
        let pair = pty_spawn::open(rows, cols).expect("open a pty");
        let mut cmd = portable_pty::CommandBuilder::new(pty_bin());
        cmd.args(["attach", id]);
        cmd.env("PTY_ROOT", &root.dir);
        cmd.env("TERM", "xterm-256color");
        for key in AMBIENT {
            cmd.env_remove(key);
        }
        let child = pair.slave.spawn_command(cmd).expect("spawn pty attach");
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        let writer = pair.master.take_writer().expect("pty writer");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
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
        RawHost {
            actor: TerminalActor::new(rows, cols, 100),
            writer,
            rx,
            child,
            _master: pair.master,
        }
    }

    fn pump(&mut self) {
        while let Ok(chunk) = self.rx.try_recv() {
            self.actor.write(&chunk);
        }
        let replies = self.actor.take_pty_replies();
        if !replies.is_empty() {
            self.write(&replies);
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write to the host pty");
        self.writer.flush().expect("flush the host pty");
    }

    fn wait_for_text(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_millis(WAIT);
        loop {
            self.pump();
            let screen = self.actor.plain(Range::Viewport);
            if screen.contains(text) {
                return;
            }
            assert!(Instant::now() < deadline, "no {text:?} on the host screen:\n{screen}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for RawHost {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── bytes typed into an attached client ──────────────────────────────────

/// What hosts send for the keys that runtimes have been known to re-encode,
/// drop, double or keep for themselves. One write each.
const TYPED: &[&str] = &[
    // Enter, a host keybind that types LF (Shift+Enter), Alt+Enter.
    "\r", "\n", "\x1b\r",
    // Ctrl+/ (0x1f), Ctrl+^ (0x1e), Ctrl+H, Backspace, Alt+Backspace, Ctrl+Space.
    "\x1f", "\x1e", "\x08", "\x7f", "\x1b\x7f", "\0",
    // Control chords a tty would act on: C, Z, Q, S, V, O, W, D, U, J.
    "\x03", "\x1a", "\x11", "\x13", "\x16", "\x0f", "\x17", "\x04", "\x15", "\x0a",
    // F1-F4 in their SS3, VT220 and kitty forms (NumLock bit included).
    "\x1bOP", "\x1bOQ", "\x1bOR", "\x1bOS", "\x1b[11~", "\x1b[12~", "\x1b[13~", "\x1b[14~",
    "\x1b[P", "\x1b[Q", "\x1b[S", "\x1b[13;1:1~", "\x1b[13;129:1~",
    // Delete (plain and kitty with alternates), PageUp, PageDown, keypad.
    "\x1b[3~", "\x1b[3::99;1:2~", "\x1b[5~", "\x1b[6~", "\x1bOp", "\x1bOy", "\x1bOM",
    // Alt/Option chords with Shift kept, Ctrl+Alt+F, Alt+V.
    "\x1bA", "\x1b<", "\x1b\x06", "\x1bv",
    // kitty key events: Ctrl+J, a release, Caps Lock, a keypad digit, Enter
    // with its text, an emoji with its text, a layout key with its
    // base-layout key, an arrow release, Ctrl+C, Ctrl+U.
    "\x1b[106;5u", "\x1b[106;1:3u", "\x1b[97;65u", "\x1b[57399u", "\x1b[13;1;13u",
    "\x1b[128512;1;128512u", "\x1b[12640::98;5u", "\x1b[1;1:3A", "\x1b[99;5u", "\x1b[117;5u",
    // modifyOtherKeys and win32-input-mode forms of Shift+Enter.
    "\x1b[27;2;13~", "\x1b[13;28;13;1;16;1_",
    // IME commits, dead keys, AltGr characters, non-Latin text, an emoji.
    "שלום", "한국어", "中文输入", "åäö", "ā", "ê", "#", "~", "😀",
    // Focus reports and SGR mouse reports.
    "\x1b[I", "\x1b[O", "\x1b[<0;10;5M", "\x1b[<0;10;5m",
    // A bracketed paste from the host.
    "\x1b[200~first line\nsecond line\x1b[201~",
];

#[test]
fn keys_typed_into_an_attached_client_reach_the_child_byte_for_byte() {
    let root = Root::new();
    let out = root.file("typed.bin");
    root.start("typed", &recorder(&out, ""));
    let mut host = attach_live(&root, "typed");

    let mut want = Vec::new();
    for key in TYPED {
        host.type_str(key);
        want.extend_from_slice(key.as_bytes());
    }
    // One key at a time, too: nothing may be doubled or dropped.
    for _ in 0..200 {
        host.type_str("x");
        want.push(b'x');
    }

    let got = recorded(&out, want.len());
    assert_eq!(
        shown(&got),
        shown(&want),
        "the child must read exactly what was typed, nothing re-encoded, dropped or doubled"
    );
    assert!(!host.has_exited(), "the client is still attached");
}

#[test]
fn bytes_that_are_not_utf8_and_characters_split_across_writes_reach_the_child_unchanged() {
    let root = Root::new();
    let out = root.file("split.bin");
    root.start("split", &recorder(&out, ""));
    let mut host = RawHost::attach(&root, "split", 24, 80);
    host.wait_for_text("WAITING");
    host.write(b"go\r");
    host.wait_for_text("READY");

    // "ש" split across two writes, then bytes that are not UTF-8 at all
    // (a Latin-1 é, a lone continuation byte, 0xff).
    host.write(&[0xd7]);
    std::thread::sleep(Duration::from_millis(60));
    host.write(&[0xa9]);
    host.write(&[0xe9, 0x80, 0xff]);
    let want = [0xd7, 0xa9, 0xe9, 0x80, 0xff];

    let got = recorded(&out, want.len());
    assert_eq!(got, want, "the child must read the bytes as typed");
}

// ── the host terminal's keyboard mode is the child's ─────────────────────

#[test]
fn the_host_keyboard_mode_is_legacy_until_the_child_asks_for_another() {
    let root = Root::new();
    let out = root.file("legacy.bin");
    root.start("legacy", &recorder(&out, ""));
    let mut host = attach_live(&root, "legacy");

    host.screenshot();
    assert_eq!(host.actor().kitty_flags(), 0, "no kitty flags the child did not push");
    assert_eq!(
        host_key(&mut host, &alt_a()),
        b"\x1ba",
        "no modifyOtherKeys the child did not ask for"
    );
    let ctrl_a = KeyEvent::typed(Key::A, "a", Some('a')).with_mods(Mods::CTRL);
    assert_eq!(host_key(&mut host, &ctrl_a), b"\x01");
}

#[test]
fn kitty_flags_the_child_pushes_reach_the_host_and_its_keys_reach_the_child() {
    let root = Root::new();
    let out = root.file("kitty.bin");
    root.start("kitty", &recorder(&out, "\\033[>11u"));
    let mut host = attach_live(&root, "kitty");

    host.screenshot();
    assert_eq!(host.actor().kitty_flags(), 11, "the host carries the child's flags");

    // A tap of j, as the host reports it under those flags: press and release.
    let press = KeyEvent::typed(Key::J, "j", Some('j'));
    let release = KeyEvent {
        action: KeyAction::Release,
        ..press.clone()
    };
    let mut want = host_key(&mut host, &press);
    want.extend(host_key(&mut host, &release));
    assert_eq!(shown(&want), "\\e[106u\\e[106;1:3u");
    host.type_str(std::str::from_utf8(&want).unwrap());

    let got = recorded(&out, want.len());
    assert_eq!(shown(&got), shown(&want), "press and release reach the child as reported");
}

#[test]
fn modify_other_keys_the_child_sets_reaches_the_host() {
    let root = Root::new();
    let out = root.file("mok.bin");
    root.start("mok", &recorder(&out, "\\033[>4;2m"));
    let mut host = attach_live(&root, "mok");

    let alt = host_key(&mut host, &alt_a());
    assert_eq!(shown(&alt), "\\e[27;3;97~", "the host reports modified keys the child's way");
    host.type_str(std::str::from_utf8(&alt).unwrap());
    assert_eq!(recorded(&out, alt.len()), alt);
}

#[test]
fn detach_pops_the_child_kitty_flags_and_reattach_pushes_them_again() {
    let root = Root::new();
    let out = root.file("kr.bin");
    root.start("kr", &recorder(&out, "\\033[>11u"));
    let mut host = attach_live(&root, "kr");
    host.screenshot();
    assert_eq!(host.actor().kitty_flags(), 11);

    detach(&mut host, "kr");
    host.screenshot();
    assert_eq!(host.actor().kitty_flags(), 0, "detach pops the flags off the host");

    let mut again = root.attach("kr");
    again.wait_for_text("READY", WAIT).expect("replay");
    again.screenshot();
    assert_eq!(again.actor().kitty_flags(), 11, "reattach restores the child's flags");
}

/// A program that pushes kitty flags on the alternate screen and leaves it
/// without popping has left the main screen's flags alone: the stacks are
/// per screen. Likewise a shell that pushed flags on the main screen has not
/// given them to a program on the alternate screen. The host must carry the
/// flags of the screen the child is on, after a reattach as before it.
#[test]
#[ignore = "fails on main: reattach pushes kitty flags that belong to the other screen, so the host reports keys the child never asked for"]
fn reattach_gives_the_host_the_kitty_flags_of_the_screen_the_child_is_on() {
    let root = Root::new();
    let mut wrong = Vec::new();
    for (id, modes, what) in [
        (
            "left-alt",
            "\\033[?1049h\\033[>1u\\033[?1049l",
            "flags pushed on the alternate screen, which the child then left",
        ),
        (
            "on-alt",
            "\\033[>1u\\033[?1049h",
            "flags pushed on the main screen before the child entered the alternate one",
        ),
    ] {
        let out = root.file(&format!("{id}.bin"));
        root.start(id, &recorder(&out, modes));
        let mut host = attach_live(&root, id);
        host.screenshot();
        let live = host.actor().kitty_flags();
        detach(&mut host, id);

        let mut again = root.attach(id);
        again.wait_for_text("READY", WAIT).expect("replay");
        let ctrl_a = KeyEvent::typed(Key::A, "a", Some('a')).with_mods(Mods::CTRL);
        let sends = host_key(&mut again, &ctrl_a);
        let flags = again.actor().kitty_flags();
        if live != 0 || flags != 0 {
            wrong.push(format!(
                "{what}: host flags {live} while live, {flags} after reattach; Ctrl+A now reaches the child as {}",
                shown(&sends)
            ));
        }
    }
    assert!(wrong.is_empty(), "the child's screen has kitty flags 0, but:\n{}", wrong.join("\n"));
}

#[test]
#[ignore = "fails on main: detach leaves the host in modifyOtherKeys mode 2 (the reset sequence does not undo CSI > 4 ; 2 m)"]
fn detach_takes_modify_other_keys_off_the_host() {
    let root = Root::new();
    let out = root.file("mokd.bin");
    root.start("mokd", &recorder(&out, "\\033[>4;2m"));
    let mut host = attach_live(&root, "mokd");
    assert_eq!(shown(&host_key(&mut host, &alt_a())), "\\e[27;3;97~");

    detach(&mut host, "mokd");
    let after = host_key(&mut host, &alt_a());
    assert_eq!(
        shown(&after),
        "\\ea",
        "after detach the host must send Alt+a to the shell the way it did before attach"
    );
}

#[test]
#[ignore = "fails on main: the attach replay does not carry modifyOtherKeys, so a reattached host sends legacy keys to a child that asked for mode 2"]
fn reattach_gives_the_host_the_child_modify_other_keys_mode() {
    let root = Root::new();
    let out = root.file("mokr.bin");
    root.start("mokr", &recorder(&out, "\\033[>4;2m"));
    let mut host = attach_live(&root, "mokr");
    assert_eq!(shown(&host_key(&mut host, &alt_a())), "\\e[27;3;97~");
    detach(&mut host, "mokr");

    let mut again = root.attach("mokr");
    again.wait_for_text("READY", WAIT).expect("replay");
    let after = host_key(&mut again, &alt_a());
    assert_eq!(
        shown(&after),
        "\\e[27;3;97~",
        "the reattached host must report keys in the mode the child still has"
    );
}

// ── detaching restores the host terminal ─────────────────────────────────

/// Mode switches a child makes: mouse, focus, paste, cursor and screen modes,
/// kitty flags pushed on the main screen, then the alternate screen.
const CHILD_MODES: &str = "\\033[?1000h\\033[?1002h\\033[?1003h\\033[?1006h\\033[?1004h\
\\033[?2004h\\033[?1h\\033[?25l\\033[4h\\033[?6h\\033[?7l\\033[>7u\\033[?1049h";

const RESTORED: [(Mode, &str); 11] = [
    (Mode::NORMAL_MOUSE, "?1000 mouse tracking"),
    (Mode::BUTTON_MOUSE, "?1002 mouse tracking"),
    (Mode::ANY_MOUSE, "?1003 mouse tracking"),
    (Mode::SGR_MOUSE, "?1006 SGR mouse encoding"),
    (Mode::FOCUS_EVENT, "?1004 focus reporting"),
    (Mode::BRACKETED_PASTE, "?2004 bracketed paste"),
    (Mode::DECCKM, "?1 application cursor keys"),
    (Mode::CURSOR_VISIBLE, "?25 cursor visibility"),
    (Mode::INSERT, "4 insert mode"),
    (Mode::ORIGIN, "?6 origin mode"),
    (Mode::WRAPAROUND, "?7 autowrap"),
];

/// Every mode in `RESTORED` that differs from a fresh terminal's, plus the
/// kitty flags and the screen.
fn changed_modes(host: &mut Session) -> Vec<String> {
    host.screenshot();
    let clean = fresh();
    let mut changed = Vec::new();
    for (m, name) in RESTORED {
        let now = host.actor().terminal().mode(m).unwrap_or(false);
        if now != clean.terminal().mode(m).unwrap_or(false) {
            changed.push(format!("{name} is {}", if now { "on" } else { "off" }));
        }
    }
    if host.actor().kitty_flags() != 0 {
        changed.push(format!("kitty flags {}", host.actor().kitty_flags()));
    }
    if host.actor().alt_screen_active() {
        changed.push("alternate screen".to_string());
    }
    changed
}

#[test]
fn detach_restores_the_mouse_paste_focus_screen_cursor_and_keyboard_modes() {
    let root = Root::new();
    let out = root.file("restore.bin");
    root.start("restore", &recorder(&out, CHILD_MODES));
    let mut host = attach_live(&root, "restore");
    let live = changed_modes(&mut host);
    assert_eq!(
        live.len(),
        RESTORED.len() + 1,
        "every mode and the alternate screen are live on the host: {live:?}"
    );
    assert_eq!(
        host.actor().modes().kitty_stack,
        vec![7],
        "and the main screen carries the child's kitty flags"
    );

    detach(&mut host, "restore");
    let left = changed_modes(&mut host);
    assert!(left.is_empty(), "detach left the host changed: {left:?}");
}

#[test]
fn a_session_that_ends_while_attached_restores_the_host_terminal() {
    let root = Root::new();
    let out = root.file("killed.bin");
    root.start("killed", &recorder(&out, CHILD_MODES));
    let mut host = attach_live(&root, "killed");
    assert!(!changed_modes(&mut host).is_empty());

    let killed = root.pty(&["kill", "killed"]);
    assert!(killed.status.success(), "{}", String::from_utf8_lossy(&killed.stderr));
    let deadline = Instant::now() + Duration::from_millis(WAIT);
    while !host.has_exited() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(host.has_exited(), "the client ends with its session");
    let left = changed_modes(&mut host);
    assert!(left.is_empty(), "the host was left changed: {left:?}");
}

/// A daemon that dies with this client's keystrokes still unread closes the
/// socket with a reset rather than an end of file. The client exits either
/// way, and either way the host terminal is the client's to restore.
#[test]
#[ignore = "fails on main: when the connection is reset the client exits without writing the terminal reset, leaving the host in the child's mouse, paste and screen modes"]
fn a_client_whose_connection_is_reset_still_restores_the_host_terminal() {
    let root = Root::new();
    let out = root.file("reset.bin");
    root.start("reset", &recorder(&out, CHILD_MODES));
    let mut host = attach_live(&root, "reset");
    assert!(!changed_modes(&mut host).is_empty());

    // Stop the daemon so what the client sends next stays unread, then kill
    // it: the kernel resets a socket closed with unread data.
    let pid = root.daemon_pid("reset");
    let stop = Command::new("kill").args(["-STOP", &pid]).status().unwrap();
    assert!(stop.success());
    host.type_str("typed while the daemon is stopped");
    std::thread::sleep(Duration::from_millis(200));
    let kill = Command::new("kill").args(["-KILL", &pid]).status().unwrap();
    assert!(kill.success());

    let deadline = Instant::now() + Duration::from_millis(WAIT);
    while !host.has_exited() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(host.has_exited(), "the client ends when its connection does");
    let left = changed_modes(&mut host);
    let screen = host.screenshot().text;
    assert!(
        left.is_empty(),
        "the host was left changed: {left:?}\nscreen:\n{}",
        screen.trim_end()
    );
}

/// `?9` is its own mode, but resetting `?1000` (which the client's reset
/// does) ends every kind of mouse tracking, `?9` included, in libghostty as
/// in xterm.
#[test]
fn detach_stops_x10_mouse_reports() {
    let root = Root::new();
    let out = root.file("x10.bin");
    root.start("x10", &recorder(&out, "\\033[?9h"));
    let mut host = attach_live(&root, "x10");
    let click = MouseEvent::press(MouseButton::Left, 5, 5);
    host.screenshot();
    assert!(host.actor().encode_mouse(&click).is_some(), "the child's ?9 is live on the host");

    detach(&mut host, "x10");
    host.screenshot();
    assert_eq!(
        host.actor().encode_mouse(&click).map(|c| shown(&c)),
        None,
        "after detach a click in the parent shell must not type a mouse report"
    );
}

/// A program run from the parent shell after detach that turns on mouse
/// reporting without choosing an encoding must get the default one, not the
/// encoding the session's child chose. (The `?1005`/`?1015`/`?1016` mode bits
/// stay set on the host, but resetting `?1006` puts libghostty's encoder,
/// like xterm's, back to the default.)
#[test]
fn detach_leaves_the_next_program_the_default_mouse_encoding() {
    let root = Root::new();
    let out = root.file("menc.bin");
    root.start("menc", &recorder(&out, "\\033[?1005h\\033[?1015h\\033[?1016h"));
    let mut host = root.attach_then("menc", "\\033[?1000h");
    start_recording(&mut host);
    for m in [Mode::UTF8_MOUSE, Mode::URXVT_MOUSE, Mode::SGR_PIXELS_MOUSE] {
        assert!(mode(&mut host, m), "the child's encoding {} is live", m.value());
    }

    host.type_str("\x1c");
    host.wait_for_text("HOST-AFTER", WAIT)
        .expect("the client detached and the parent shell's next program enabled the mouse");
    host.screenshot();
    let click = host
        .actor()
        .encode_mouse(&MouseEvent::press(MouseButton::Left, 10, 4))
        .map(|c| shown(&c));
    assert_eq!(
        click.as_deref(),
        Some("\\e[M +%"),
        "a click at column 11, row 5 in the default encoding"
    );
}

#[test]
#[ignore = "fails on main: detach leaves the colours the child set with OSC 4/10/11/12 on the host"]
fn detach_restores_the_host_colors() {
    let root = Root::new();
    let out = root.file("colors.bin");
    root.start(
        "colors",
        &recorder(
            &out,
            "\\033]10;#ff0000\\007\\033]11;#00ff00\\007\\033]12;#0000ff\\007\\033]4;1;#123456\\007",
        ),
    );
    let mut host = attach_live(&root, "colors");
    let colors = |host: &mut Session| {
        host.screenshot();
        let t = host.actor().terminal();
        format!(
            "fg {:?} bg {:?} cursor {:?} color1 {:?}",
            t.fg_color().ok().flatten(),
            t.bg_color().ok().flatten(),
            t.cursor_color().ok().flatten(),
            t.color_palette().ok().map(|p| p.0[1])
        )
    };
    let clean = {
        let t = fresh();
        let t = t.terminal();
        format!(
            "fg {:?} bg {:?} cursor {:?} color1 {:?}",
            t.fg_color().ok().flatten(),
            t.bg_color().ok().flatten(),
            t.cursor_color().ok().flatten(),
            t.color_palette().ok().map(|p| p.0[1])
        )
    };
    assert_ne!(colors(&mut host), clean, "the child's colours are live on the host");

    detach(&mut host, "colors");
    assert_eq!(colors(&mut host), clean, "the host's colours after detach");
}

/// Kitty flag stacks are per screen. A child that pushed flags on the
/// alternate screen and is detached from there must not leave them on the
/// host's alternate screen, where the next full-screen program run from the
/// parent shell would find them.
#[test]
#[ignore = "fails on main: the reset leaves the alternate screen before popping kitty flags, so the host's alternate-screen stack keeps the child's flags"]
fn detach_clears_kitty_flags_the_child_pushed_on_the_alternate_screen() {
    let root = Root::new();
    let out = root.file("altk.bin");
    root.start("altk", &recorder(&out, "\\033[?1049h\\033[>1u"));
    let mut host = root.attach_then("altk", "\\033[?1049h");
    start_recording(&mut host);
    host.screenshot();
    assert_eq!(host.actor().kitty_flags(), 1, "the child's flags are live");

    // The parent shell's next program takes the screen as soon as the client
    // is gone, so wait for it rather than for the detach message.
    host.type_str("\x1c");
    host.wait_for_text("HOST-AFTER", WAIT)
        .expect("the client detached and the parent shell entered the alternate screen");
    let esc = KeyEvent::press(Key::Escape);
    let sends = host_key(&mut host, &esc);
    assert_eq!(
        host.actor().kitty_flags(),
        0,
        "a program on the alternate screen after detach inherits the child's flags; Escape reaches it as {}",
        shown(&sends)
    );
}

// ── the detach key under every keyboard mode ─────────────────────────────

#[test]
fn the_detach_key_detaches_under_every_keyboard_mode_the_child_can_choose() {
    let root = Root::new();
    let mut failed = Vec::new();
    for (i, modes) in [
        "",
        "\\033[>1u",
        "\\033[>3u",
        "\\033[>11u",
        "\\033[>31u",
        "\\033[>4;2m",
        "\\033[?1h\\033=",
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("dk{i}");
        let out = root.file(&format!("{id}.bin"));
        root.start(&id, &recorder(&out, modes));
        let mut host = attach_live(&root, &id);
        // A tap: press, and the release when the host reports releases.
        let press = ctrl_backslash(Mods::empty());
        let release = KeyEvent {
            action: KeyAction::Release,
            ..press.clone()
        };
        let mut tap = host_key(&mut host, &press);
        tap.extend(host_key(&mut host, &release));
        host.type_str(std::str::from_utf8(&tap).unwrap());
        if host
            .wait_for_text(&format!("[detached from {id}]"), 3000)
            .is_err()
        {
            failed.push(format!("{modes}: {} did not detach", shown(&tap)));
            continue;
        }
        // A release that comes after the detach is not reported to the
        // parent shell as text.
        let late = host_key(&mut host, &release);
        if !late.is_empty() {
            failed.push(format!("{modes}: after detach a release still sends {}", shown(&late)));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

#[test]
#[ignore = "fails on main: under kitty flags a host with Num Lock or Caps Lock on sends Ctrl+\\ as CSI 92;133u / CSI 92;69u, which the client forwards to the child instead of detaching"]
fn the_detach_key_detaches_under_kitty_flags_with_a_lock_key_on() {
    let root = Root::new();
    let mut failed = Vec::new();
    for (i, (lock, name)) in [(Mods::NUM_LOCK, "Num Lock"), (Mods::CAPS_LOCK, "Caps Lock")]
        .into_iter()
        .enumerate()
    {
        let id = format!("lk{i}");
        let out = root.file(&format!("{id}.bin"));
        root.start(&id, &recorder(&out, "\\033[>1u"));
        let mut host = attach_live(&root, &id);
        let press = host_key(&mut host, &ctrl_backslash(lock));
        host.type_str(std::str::from_utf8(&press).unwrap());
        if host
            .wait_for_text(&format!("[detached from {id}]"), 2000)
            .is_err()
        {
            let child_got = recorded(&out, press.len());
            failed.push(format!(
                "{name} on: the host sent {}; no detach, and the child read {}",
                shown(&press),
                shown(&child_got)
            ));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

// ── bracketed paste ──────────────────────────────────────────────────────

#[test]
fn a_host_paste_is_bracketed_exactly_when_the_child_enabled_bracketed_paste() {
    let root = Root::new();
    let text = "echo one\necho two\n";

    let on = root.file("bp-on.bin");
    root.start("bpon", &recorder(&on, "\\033[?2004h"));
    let mut host = attach_live(&root, "bpon");
    host.screenshot();
    let paste = host.actor().encode_paste(text);
    assert!(paste.starts_with(b"\x1b[200~") && paste.ends_with(b"\x1b[201~"));
    host.type_str(std::str::from_utf8(&paste).unwrap());
    assert_eq!(shown(&recorded(&on, paste.len())), shown(&paste));

    let off = root.file("bp-off.bin");
    root.start("bpoff", &recorder(&off, "\\033[?2004l"));
    let mut host = attach_live(&root, "bpoff");
    host.screenshot();
    let paste = host.actor().encode_paste(text);
    host.type_str(std::str::from_utf8(&paste).unwrap());
    let got = recorded(&off, paste.len());
    assert_eq!(shown(&got), shown(&paste));
    assert!(
        !got.windows(6).any(|w| w == b"\x1b[200~" || w == b"\x1b[201~"),
        "no markers without ?2004: {}",
        shown(&got)
    );
}

/// Some terminals signal an image paste with an empty bracketed-paste pair;
/// the program then reads the image from the OS clipboard itself.
#[test]
fn an_empty_bracketed_paste_signal_reaches_the_child() {
    let root = Root::new();
    let out = root.file("image-paste.bin");
    root.start("image-paste", &recorder(&out, "\\033[?2004h"));
    let mut host = attach_live(&root, "image-paste");
    let signal = b"\x1b[200~\x1b[201~";
    host.type_str(std::str::from_utf8(signal).unwrap());
    assert_eq!(recorded(&out, signal.len()), signal);
}

#[test]
fn reattach_turns_bracketed_paste_back_on_for_a_child_that_enabled_it() {
    let root = Root::new();
    let out = root.file("bpr.bin");
    root.start("bpr", &recorder(&out, "\\033[?2004h"));
    let mut host = attach_live(&root, "bpr");
    detach(&mut host, "bpr");
    assert!(!mode(&mut host, Mode::BRACKETED_PASTE), "detach turned it off");

    let mut again = root.attach("bpr");
    again.wait_for_text("READY", WAIT).expect("replay");
    assert!(mode(&mut again, Mode::BRACKETED_PASTE), "reattach turned it on again");
    let paste = again.actor().encode_paste("one\ntwo");
    again.type_str(std::str::from_utf8(&paste).unwrap());
    assert_eq!(shown(&recorded(&out, paste.len())), shown(&paste));
    assert!(paste.starts_with(b"\x1b[200~"));
}

#[test]
#[ignore = "fails on main: pty send --paste wraps the text in bracketed-paste markers even when the child has not enabled ?2004, so the markers reach it as text"]
fn a_programmatic_paste_is_bracketed_only_when_the_child_enabled_bracketed_paste() {
    let root = Root::new();
    let out = root.file("sp.bin");
    let ready = root.file("sp.ready");
    root.start("sp", &quiet_recorder(&out, &ready, "\\033[?2004l"));
    root.wait_for_file("sp.ready");

    let sent = root.send("sp", &["--paste", "echo one\necho two"]);
    assert!(sent.status.success(), "{}", String::from_utf8_lossy(&sent.stderr));
    let got = recorded(&out, "echo one\necho two".len());
    assert!(
        !got.windows(6).any(|w| w == b"\x1b[200~" || w == b"\x1b[201~"),
        "a child without ?2004 read the markers as text: {}",
        shown(&got)
    );
}

// ── large input ──────────────────────────────────────────────────────────

/// `len` bytes of numbered lines, so a gap or a reordering shows.
fn numbered(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 16);
    let mut i = 0u64;
    while out.len() < len {
        out.extend_from_slice(format!("{i:09}\n").as_bytes());
        i += 1;
    }
    out.truncate(len);
    out
}

fn first_difference(a: &[u8], b: &[u8]) -> String {
    let at = a.iter().zip(b).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
    format!("{} bytes arrived of {}; first difference at byte {at}", a.len(), b.len())
}

#[test]
fn a_multi_megabyte_paste_into_an_attached_client_arrives_whole_and_in_order() {
    let root = Root::new();
    let out = root.file("big.bin");
    root.start("big", &recorder(&out, ""));
    let mut host = attach_live(&root, "big");

    let payload = numbered(5 * 1024 * 1024);
    host.type_str(std::str::from_utf8(&payload).unwrap());
    let got = recorded(&out, payload.len());
    assert!(got == payload, "{}", first_difference(&got, &payload));

    // The client is still attached and still delivering.
    host.type_str("after\n");
    let mut want = payload.clone();
    want.extend_from_slice(b"after\n");
    let got = recorded(&out, want.len());
    assert!(got == want, "{}", first_difference(&got, &want));
    assert!(!host.has_exited(), "the client stayed attached");
}

#[test]
fn a_multi_megabyte_programmatic_send_arrives_whole_or_is_refused_whole() {
    let root = Root::new();
    let out = root.file("bigsend.bin");
    let ready = root.file("bigsend.ready");
    root.start("bigsend", &quiet_recorder(&out, &ready, ""));
    root.wait_for_file("bigsend.ready");
    let sock = root.file("bigsend.sock");

    // One 5 MiB DATA frame.
    let payload = numbered(5 * 1024 * 1024);
    let mut s = UnixStream::connect(&sock).expect("connect");
    s.write_all(&encode_data(&payload)).expect("send 5 MiB");
    let _ = s.shutdown(std::net::Shutdown::Write);
    let got = recorded(&out, payload.len());
    assert!(got == payload, "{}", first_difference(&got, &payload));

    // The largest argument `pty send` can be given.
    let arg = String::from_utf8(numbered(120 * 1024)).unwrap();
    let sent = root.send("bigsend", &[&arg]);
    assert!(sent.status.success(), "{}", String::from_utf8_lossy(&sent.stderr));
    let mut want = payload.clone();
    want.extend_from_slice(arg.as_bytes());
    let got = recorded(&out, want.len());
    assert!(got == want, "{}", first_difference(&got, &want));

    // A frame over the protocol's limit is refused as a whole: the sender
    // sees its connection end, and not one byte of it reaches the child.
    let huge = vec![b'z'; pty_core::protocol::MAX_PACKET_LENGTH + 1];
    let mut s = UnixStream::connect(&sock).expect("connect");
    let wrote = s.write_all(&encode_data(&huge));
    let mut buf = [0u8; 16];
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let read = s.read(&mut buf);
    assert!(
        wrote.is_err() || matches!(read, Ok(0) | Err(_)),
        "the oversized frame was neither refused nor answered by a close"
    );
    std::thread::sleep(Duration::from_millis(500));
    let got = std::fs::read(&out).unwrap_or_default();
    assert!(got == want, "nothing of the refused frame may arrive: {}", first_difference(&got, &want));

    // And the session still takes input.
    let sent = root.send("bigsend", &["still-here"]);
    assert!(sent.status.success());
    want.extend_from_slice(b"still-here");
    let got = recorded(&out, want.len());
    assert!(got == want, "{}", first_difference(&got, &want));
}

#[test]
fn a_long_command_line_sent_to_a_fresh_shell_runs_whole() {
    let root = Root::new();
    let count = root.file("fresh.count");
    root.start("fresh", "exec sh");
    let word = "x".repeat(2200);
    // Straight after start: the shell may not have printed a prompt yet.
    let line = format!("echo {word} | wc -c > '{}'\r", count.display());
    let sent = root.send("fresh", &[&line]);
    assert!(sent.status.success(), "{}", String::from_utf8_lossy(&sent.stderr));
    let deadline = Instant::now() + Duration::from_millis(WAIT);
    let mut got = String::new();
    while Instant::now() < deadline {
        got = std::fs::read_to_string(&count).unwrap_or_default();
        if !got.trim().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(got.trim(), "2201", "the whole 2200-character word must reach the shell");
}

// ── input keeps its boundaries ───────────────────────────────────────────

#[test]
fn text_and_enter_sent_separately_reach_the_child_as_separate_reads() {
    let root = Root::new();
    let out = root.file("reads.log");
    let ready = root.file("reads.ready");
    root.start("reads", &read_logger(&out, &ready));
    root.wait_for_file("reads.ready");

    // `pty send` with its default pacing.
    let sent = root.send("reads", &["--seq", "hello", "--seq", "key:return"]);
    assert!(sent.status.success(), "{}", String::from_utf8_lossy(&sent.stderr));
    let want = "68656c6c6f|0d|";
    assert_eq!(logged(&out, want), want, "hello, then Enter on its own");

    // Typed into an attached client, a moment apart.
    let mut host = root.attach("reads");
    host.wait_for_text("LOGGING", WAIT).expect("attached");
    host.type_str("world");
    std::thread::sleep(Duration::from_millis(150));
    host.type_str("\r");
    let want = "68656c6c6f|0d|776f726c64|0d|";
    assert_eq!(logged(&out, want), want, "world, then Enter on its own");
}

// ── mouse ────────────────────────────────────────────────────────────────

#[test]
fn mouse_reports_reach_the_child_in_the_encoding_it_chose() {
    let root = Root::new();

    // The default (X10-style) encoding past column 95, where the coordinate
    // is a byte above 0x7f.
    let out = root.file("m-x10.bin");
    root.start_sized("mx10", 24, 120, &recorder(&out, "\\033[?1000h"));
    let mut host = RawHost::attach(&root, "mx10", 24, 120);
    host.wait_for_text("WAITING");
    host.write(b"go\r");
    host.wait_for_text("READY");
    host.pump();
    let click = host
        .actor
        .encode_mouse(&MouseEvent::press(MouseButton::Left, 100, 3))
        .expect("the host reports clicks");
    assert_eq!(&click[..3], b"\x1b[M");
    assert!(click[4] > 0x7f, "a high coordinate byte: {click:?}");
    host.write(&click);
    assert_eq!(recorded(&out, click.len()), click, "X10 report with a high byte");
    drop(host);

    // SGR: a click and a horizontal wheel notch.
    let out = root.file("m-sgr.bin");
    root.start("msgr", &recorder(&out, "\\033[?1000h\\033[?1006h"));
    let mut host = attach_live(&root, "msgr");
    host.screenshot();
    let mut want = host
        .actor()
        .encode_mouse(&MouseEvent::press(MouseButton::Left, 10, 4))
        .expect("click");
    let wheel = MouseEvent {
        any_button_pressed: false,
        ..MouseEvent::press(MouseButton::Six, 10, 4)
    };
    want.extend(host.actor().encode_mouse(&wheel).expect("horizontal wheel"));
    assert_eq!(shown(&want), "\\e[<0;11;5M\\e[<66;11;5M");
    host.type_str(std::str::from_utf8(&want).unwrap());
    assert_eq!(shown(&recorded(&out, want.len())), shown(&want));

    // SGR-pixels: motion reported in pixels.
    let out = root.file("m-px.bin");
    root.start("mpx", &recorder(&out, "\\033[?1003h\\033[?1006h\\033[?1016h"));
    let mut host = attach_live(&root, "mpx");
    assert!(mode(&mut host, Mode::SGR_PIXELS_MOUSE));
    let motion = MouseEvent {
        action: pty_terminal::MouseAction::Motion,
        button: None,
        any_button_pressed: false,
        ..MouseEvent::press(MouseButton::Left, 40, 12)
    };
    let want = host.actor().encode_mouse(&motion).expect("motion");
    host.type_str(std::str::from_utf8(&want).unwrap());
    assert_eq!(shown(&recorded(&out, want.len())), shown(&want));
}

#[test]
fn a_mouse_report_split_across_reads_reaches_the_child_whole_and_alone() {
    let root = Root::new();
    let out = root.file("msplit.bin");
    root.start("msplit", &recorder(&out, "\\033[?1003h\\033[?1006h"));
    let mut host = attach_live(&root, "msplit");

    host.type_str("\x1b[<35;10;20");
    std::thread::sleep(Duration::from_millis(200));
    host.type_str("M");
    host.type_str("\x1b");
    std::thread::sleep(Duration::from_millis(200));
    host.type_str("[<0;5;5M");
    let want = "\x1b[<35;10;20M\x1b[<0;5;5M";
    assert_eq!(
        shown(&recorded(&out, want.len())),
        shown(want.as_bytes()),
        "the reports arrive intact, with no ESC or text of their own"
    );
}

#[test]
fn reattach_sends_mouse_reports_to_the_child_that_still_wants_them() {
    let root = Root::new();
    let out = root.file("mre.bin");
    root.start("mre", &recorder(&out, "\\033[?1003h\\033[?1006h"));
    let mut host = attach_live(&root, "mre");
    detach(&mut host, "mre");
    host.screenshot();
    assert!(!host.actor().modes().mouse_reporting(), "detach stops mouse reporting");

    let mut again = root.attach("mre");
    again.wait_for_text("READY", WAIT).expect("replay");
    again.screenshot();
    let motion = MouseEvent {
        action: pty_terminal::MouseAction::Motion,
        button: None,
        any_button_pressed: false,
        ..MouseEvent::press(MouseButton::Left, 7, 3)
    };
    let report = again
        .actor()
        .encode_mouse(&motion)
        .expect("the reattached host reports motion again");
    again.type_str(std::str::from_utf8(&report).unwrap());
    assert_eq!(shown(&recorded(&out, report.len())), "\\e[<35;8;4M");
}

#[test]
fn the_wheel_over_a_child_on_the_primary_screen_is_left_to_the_host() {
    let root = Root::new();
    let out = root.file("wheel.bin");
    root.start("wheel", &recorder(&out, ""));
    let check = |host: &mut Session, when: &str| {
        host.screenshot();
        assert!(
            !host.actor().alt_screen_active(),
            "{when}: the host stays on its primary screen, where the wheel scrolls rather than sending arrows"
        );
        assert!(!host.actor().modes().mouse_reporting(), "{when}: no mouse reporting");
        for up in [true, false] {
            assert_eq!(
                host.actor().encode_mouse(&MouseEvent::wheel(up, 10, 10)),
                None,
                "{when}: a wheel notch is not the child's input"
            );
        }
    };
    let mut host = attach_live(&root, "wheel");
    check(&mut host, "attached");
    detach(&mut host, "wheel");

    let mut again = root.attach("wheel");
    again.wait_for_text("READY", WAIT).expect("replay");
    check(&mut again, "reattached");
    drop(again);
    assert_eq!(recorded(&out, 0), b"", "the child read nothing");
}

// ── named keys ───────────────────────────────────────────────────────────

/// A recorder started for `pty send`, ready for input when this returns.
fn start_quiet(root: &Root, id: &str, modes: &str) -> PathBuf {
    let out = root.file(&format!("{id}.bin"));
    let ready = root.file(&format!("{id}.ready"));
    root.start(id, &quiet_recorder(&out, &ready, modes));
    root.wait_for_file(&format!("{id}.ready"));
    out
}

#[test]
fn named_keys_reach_a_legacy_child_as_the_host_would_send_them() {
    let root = Root::new();
    let out = start_quiet(&root, "named", "");
    let keys = [
        ("ctrl+d", KeyEvent::typed(Key::D, "d", Some('d')).with_mods(Mods::CTRL)),
        ("ctrl+c", KeyEvent::typed(Key::C, "c", Some('c')).with_mods(Mods::CTRL)),
        ("shift+tab", KeyEvent::press(Key::Tab).with_mods(Mods::SHIFT)),
        ("return", KeyEvent::press(Key::Enter)),
        ("enter", KeyEvent::press(Key::Enter)),
        ("escape", KeyEvent::press(Key::Escape)),
        ("tab", KeyEvent::press(Key::Tab)),
        ("backspace", KeyEvent::press(Key::Backspace)),
        ("up", KeyEvent::press(Key::ArrowUp)),
        ("pageup", KeyEvent::press(Key::PageUp)),
        ("delete", KeyEvent::press(Key::Delete)),
    ];
    let mut args = vec!["--with-delay".to_string(), "0".to_string()];
    let mut want = Vec::new();
    for (name, ev) in &keys {
        args.push("--seq".into());
        args.push(format!("key:{name}"));
        want.extend(encoded_in(b"", ev));
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let sent = root.send("named", &args);
    assert!(sent.status.success(), "{}", String::from_utf8_lossy(&sent.stderr));
    assert_eq!(shown(&recorded(&out, want.len())), shown(&want));
}

#[test]
#[ignore = "fails on main: pty send rejects f1-f12 and ctrl+/ as unknown keys"]
fn named_function_keys_and_ctrl_slash_can_be_sent() {
    let root = Root::new();
    let out = start_quiet(&root, "fkeys", "");
    let mut failed = Vec::new();
    let mut want = Vec::new();
    for (name, ev) in [
        ("f1", KeyEvent::press(Key::F1)),
        ("f3", KeyEvent::press(Key::F3)),
        ("f12", KeyEvent::press(Key::F12)),
        (
            "ctrl+/",
            KeyEvent::typed(Key::Slash, "/", Some('/')).with_mods(Mods::CTRL),
        ),
    ] {
        let sent = root.send("fkeys", &["--seq", &format!("key:{name}")]);
        if sent.status.success() {
            want.extend(encoded_in(b"", &ev));
        } else {
            failed.push(format!(
                "{name}: {}",
                String::from_utf8_lossy(&sent.stderr).lines().next().unwrap_or("")
            ));
        }
    }
    assert!(failed.is_empty(), "refused:\n{}", failed.join("\n"));
    assert_eq!(shown(&recorded(&out, want.len())), shown(&want));
}

#[test]
#[ignore = "fails on main: named keys use one fixed table, so an arrow ignores application cursor mode and a Ctrl chord or Escape ignores the child's kitty flags"]
fn named_keys_follow_the_child_keyboard_mode() {
    let root = Root::new();
    let mut wrong = Vec::new();
    for (id, modes, raw_modes, name, ev) in [
        ("ckm", "\\033[?1h", &b"\x1b[?1h"[..], "up", KeyEvent::press(Key::ArrowUp)),
        (
            "kc",
            "\\033[>1u",
            &b"\x1b[>1u"[..],
            "ctrl+c",
            KeyEvent::typed(Key::C, "c", Some('c')).with_mods(Mods::CTRL),
        ),
        ("ke", "\\033[>1u", &b"\x1b[>1u"[..], "escape", KeyEvent::press(Key::Escape)),
    ] {
        let out = start_quiet(&root, id, modes);
        let sent = root.send(id, &["--seq", &format!("key:{name}")]);
        assert!(sent.status.success(), "{}", String::from_utf8_lossy(&sent.stderr));
        let want = encoded_in(raw_modes, &ev);
        let got = recorded(&out, want.len());
        if got != want {
            wrong.push(format!(
                "{name} with {}: child read {}, the host would send {}",
                shown(raw_modes),
                shown(&got),
                shown(&want)
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

// ── lone Escape ──────────────────────────────────────────────────────────

#[test]
fn a_lone_escape_reaches_the_child_unmerged_and_unswallowed() {
    let root = Root::new();
    let out = root.file("esc.bin");
    root.start("esc", &recorder(&out, ""));
    let mut host = attach_live(&root, "esc");

    // Escape and Left in one read.
    host.type_str("\x1b\x1b[D");
    // Escape, then a mouse report 30 ms later.
    host.type_str("\x1b");
    std::thread::sleep(Duration::from_millis(30));
    host.type_str("\x1b[<35;10;20M");
    // Escape on its own, and Escape followed by its kitty release.
    host.type_str("\x1b");
    std::thread::sleep(Duration::from_millis(100));
    host.type_str("\x1b\x1b[27;1:3u");
    let want = "\x1b\x1b[D\x1b\x1b[<35;10;20M\x1b\x1b\x1b[27;1:3u";
    assert_eq!(shown(&recorded(&out, want.len())), shown(want.as_bytes()));
}

#[test]
fn a_named_escape_then_text_reach_the_child_as_separate_reads() {
    let root = Root::new();
    let out = root.file("nesc.log");
    let ready = root.file("nesc.ready");
    root.start("nesc", &read_logger(&out, &ready));
    root.wait_for_file("nesc.ready");
    let sent = root.send("nesc", &["--seq", "key:escape", "--seq", "test"]);
    assert!(sent.status.success(), "{}", String::from_utf8_lossy(&sent.stderr));
    let want = "1b|74657374|";
    assert_eq!(logged(&out, want), want, "Escape alone, then the text: not Alt+t");
}
