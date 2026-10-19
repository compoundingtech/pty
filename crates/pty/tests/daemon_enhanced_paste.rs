//! Paste events (`?5522`, kitty's enhanced paste) in the daemon: the
//! replay carries the mode a program set before a client attached, and
//! never a read it made; DECRQM reports the mode only while a client can
//! answer it; and once the program exits, a late attach is offered nothing.

mod daemon_support;

use std::path::Path;
use std::time::Duration;

use daemon_support::*;
use pty_core::protocol::MessageType;

const T: Duration = Duration::from_secs(5);

/// `image/png text/plain`, base64.
const PNG_AND_TEXT: &str = "aW1hZ2UvcG5nIHRleHQvcGxhaW4=";

/// Wait for the daemon's own terminal to show `needle`, so what the child
/// printed by then reaches a later attacher only through the replay.
fn wait_peek(d: &Daemon, needle: &str) {
    assert!(
        wait_until(T, || {
            let mut peek = d.connect();
            peek.peek_flags(true, false);
            peek.wait_text(needle, Duration::from_millis(500))
        }),
        "{} never showed {needle:?}",
        d.name
    );
}

/// Wait for a file the child writes to reach `len` bytes, and read it.
fn wait_file(path: &Path, len: usize) -> Vec<u8> {
    assert!(
        wait_until(T, || std::fs::metadata(path)
            .is_ok_and(|m| m.len() >= len as u64)),
        "{} never reached {len} bytes",
        path.display()
    );
    std::fs::read(path).unwrap()
}

#[test]
fn the_replay_carries_paste_events_and_never_a_read() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let script = format!(
        "printf '\\033[?5522h\\033]5522;type=read:id=old;{PNG_AND_TEXT}\\033\\\\READY\\r\\n'; exec sleep 60"
    );
    let d = Daemon::start(&root, config(&unique_name("paste"), "sh", &["-c", &script]));
    wait_peek(&d, "READY");
    let mut c = d.connect();
    c.attach(24, 80);
    assert!(c.wait_type(MessageType::Screen, T));
    let screen = c.screen().unwrap();
    assert!(screen.contains("READY"), "{screen:?}");
    assert!(screen.contains("\x1b[?5522h"), "{screen:?}");
    assert!(!screen.contains("5522;"), "{screen:?}");
    c.detach();
}

/// The daemon answers DECRQM for the mode itself: unrecognized (0) with no
/// client that can answer paste events, which a read-only client or a
/// command socket cannot; recognized once a writable client is attached.
#[test]
fn decrqm_reports_paste_events_only_while_a_writable_client_is_attached() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    // `CSI ? 5522 ; Ps $ y` is 11 bytes.
    let ask = |name: &str| {
        format!(
            "printf '\\033[?5522$p'; dd bs=1 count=11 of='{}' 2>/dev/null; ",
            root.join(name).display()
        )
    };
    let wait_byte = "dd bs=1 count=1 >/dev/null 2>&1; ";
    let script = format!(
        "stty raw -echo; {alone}printf 'READY\\r\\n'; {wait_byte}{peeked}{wait_byte}{attached}\
         printf '\\033[?5522h'; {on}exec sleep 60",
        alone = ask("alone"),
        peeked = ask("peeked"),
        attached = ask("attached"),
        on = ask("on"),
    );
    let d = Daemon::start(&root, config(&unique_name("paste"), "sh", &["-c", &script]));
    assert_eq!(wait_file(&root.join("alone"), 11), b"\x1b[?5522;0$y");
    wait_peek(&d, "READY");

    let mut peek = d.connect();
    peek.peek();
    assert!(peek.wait_type(MessageType::Screen, T));
    let mut command = d.connect();
    command.data("x");
    assert_eq!(wait_file(&root.join("peeked"), 11), b"\x1b[?5522;0$y");

    let mut live = d.connect();
    live.attach(24, 80);
    assert!(live.wait_type(MessageType::Screen, T));
    command.data("x");
    assert_eq!(wait_file(&root.join("attached"), 11), b"\x1b[?5522;2$y");
    assert_eq!(wait_file(&root.join("on"), 11), b"\x1b[?5522;1$y");
    live.detach();
}

/// Once the program exits nothing can answer paste events, so an attach
/// after the exit replays the screen without them.
#[test]
fn an_exited_program_leaves_no_paste_events_in_the_replay() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let script =
        "stty raw -echo; printf '\\033[?5522hREADY\\r\\n'; dd bs=1 count=1 >/dev/null 2>&1; exit 3";
    let d = Daemon::start(&root, config(&unique_name("paste"), "sh", &["-c", script]));
    let mut live = d.connect();
    live.attach(24, 80);
    assert!(live.wait_text("READY", T));
    assert!(live.output().contains("\x1b[?5522h"), "{:?}", live.output());
    live.data("x");
    assert!(live.wait_type(MessageType::Exit, T));
    // At once, inside the daemon's exit grace.
    let mut late = d.connect();
    late.attach(24, 80);
    assert!(late.wait_type(MessageType::Exit, T));
    let screen = late.screen().unwrap();
    assert!(screen.contains("READY"), "{screen:?}");
    assert!(!screen.contains("5522"), "{screen:?}");
}
