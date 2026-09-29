//! Port of tests/connection.test.ts: the `SessionConnection`, `sendData`
//! and `peekScreen` library helpers, re-expressed as what they do on the
//! wire and at the CLI. `SessionConnection` is a raw ATTACH socket (the
//! first SCREEN resolves `connect()`, `write`/`press` are DATA, `exit` is
//! the EXIT frame); `sendData` is `pty send` (bracketed-paste markers are
//! observed through a child that dumps its raw stdin); `peekScreen` is
//! `pty peek`. "not found or not running" rejections are the CLI's
//! `Session "<ref>" not found.` refusal, exit 1.
//!
//! Left out: nothing — the empty-payload paste case is the CLI's
//! `Nothing to send.` refusal, with the same "no bare marker pair" check.

use pty_conformance::*;
use pty_core::protocol::{MessageType, decode_exit};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn screen_of(conn: &mut Conn) -> String {
    let p = conn.wait_for(MessageType::Screen, deadline()).expect("SCREEN");
    String::from_utf8_lossy(&p.payload).into_owned()
}

// **Wait for the output, not for a fixed time.** A session's output is
// produced by its child after the daemon is up, and these tests used to sleep
// 300 ms and then look. That is a guess about scheduling, and it lost under
// load on 2026-09-28: `peek_returns_screen_content` and
// `peek_returns_plain_text_when_plain` each failed once in 200 runs of this
// binary, and `connects_and_receives_initial_screen` twice in 24
// whole-workspace runs. What each test checks is that the output arrives;
// how soon was never the point.

/// `pty <args>` (a peek) once it exits 0 and its stdout shows `needle`.
fn peek_once_it_shows(rig: &Rig, args: &[&str], needle: &str) -> Out {
    let mut out = rig.pty(args);
    wait_until(&format!("`pty {}` to show {needle:?}", args.join(" ")), || {
        out = rig.pty(args);
        out.status == 0 && out.stdout().contains(needle)
    });
    out
}

/// DATA from `conn` until it contains every one of `needles`.
fn data_until(conn: &mut Conn, needles: &[&str]) -> String {
    let mut received = Vec::new();
    wait_until(&format!("DATA containing {needles:?}"), || {
        received.extend(data_bytes(&conn.drain(Duration::from_millis(50))));
        let text = String::from_utf8_lossy(&received);
        needles.iter().all(|n| text.contains(n))
    });
    String::from_utf8_lossy(&received).into_owned()
}

fn start_dumper(rig: &Rig, id: &str) -> PathBuf {
    let dump = rig.root().join("dump.bin");
    let script = format!("stty raw -echo; printf '\\033[?2004h'; cat > '{}'", dump.display());
    rig.daemon(id, &["sh", "-c", &script], DaemonOpts::no_display_name());
    std::thread::sleep(Duration::from_millis(150));
    dump
}

fn wait_for_dump(dump: &Path, min_len: usize) -> Vec<u8> {
    let _ = poll_for(Duration::from_secs(3), || {
        std::fs::read(dump).map(|b| b.len() >= min_len).unwrap_or(false)
    });
    std::fs::read(dump).unwrap_or_default()
}

/// node: tests/connection.test.ts:102
#[test]
fn connects_and_receives_initial_screen() {
    let rig = Rig::new();
    rig.daemon("c1", &["sh", "-c", "echo hello-screen; exec cat"], DaemonOpts::no_display_name());
    // The attach must replay output produced before it, so first wait until
    // that output is on the daemon's screen.
    peek_once_it_shows(&rig, &["peek", "--plain", "c1"], "hello-screen");
    let mut conn = rig.connect("c1");
    conn.attach(24, 80);
    let screen = screen_of(&mut conn);
    assert!(screen.contains("hello-screen"), "{screen:?}");
    conn.detach();
    // The daemon closes the socket after DETACH.
    wait_until("socket closed after DETACH", || {
        conn.next_packet(Duration::from_millis(50));
        conn.is_eof()
    });
}

/// node: tests/connection.test.ts:118
#[test]
fn receives_data_events_after_connect() {
    let rig = Rig::new();
    rig.daemon("c2", &["cat"], DaemonOpts::no_display_name());
    let mut conn = rig.connect("c2");
    conn.attach(24, 80);
    screen_of(&mut conn);
    conn.data(b"test-input");
    let text = data_until(&mut conn, &["test-input"]);
    assert!(text.contains("test-input"), "{text:?}");
    conn.detach();
}

/// node: tests/connection.test.ts:135
#[test]
fn press_sends_named_keys() {
    let rig = Rig::new();
    rig.daemon("c3", &["cat"], DaemonOpts::no_display_name());
    let mut conn = rig.connect("c3");
    conn.attach(24, 80);
    screen_of(&mut conn);
    conn.data(b"hello");
    conn.data(b"\r");
    let text = data_until(&mut conn, &["hello", "\r"]);
    assert!(text.contains("hello"), "{text:?}");
    assert!(text.contains('\r'), "{text:?}");
    conn.detach();
}

/// node: tests/connection.test.ts:155
#[test]
fn emits_exit_when_process_exits() {
    let rig = Rig::new();
    rig.daemon("c4", &["sh", "-c", "sleep 0.5; exit 7"], DaemonOpts::no_display_name());
    let mut conn = rig.connect("c4");
    conn.attach(24, 80);
    screen_of(&mut conn);
    let exit = conn.wait_for(MessageType::Exit, deadline()).expect("EXIT");
    assert_eq!(decode_exit(&exit.payload), 7);
    conn.detach();
}

/// node: tests/connection.test.ts:175
#[test]
fn rejects_on_nonexistent_session() {
    let rig = Rig::new();
    assert!(Conn::try_open(&rig.socket_path("nonexistent")).is_none());
    let out = rig.pty(&["send", "nonexistent", "x"]);
    expect_status(&out, 1);
    expect_contains(&out.stderr(), "Session \"nonexistent\" not found.");
}

/// node: tests/connection.test.ts:188
#[test]
fn resize_sends_new_dimensions() {
    let rig = Rig::new();
    rig.daemon("c5", &["cat"], DaemonOpts::no_display_name());
    let mut conn = rig.connect("c5");
    conn.attach(24, 80);
    screen_of(&mut conn);
    conn.resize(30, 100);
    std::thread::sleep(Duration::from_millis(100));
    // The only writable client selects the size: the daemon confirms it.
    let g = conn.wait_for(GEOMETRY, deadline()).expect("GEOMETRY after RESIZE");
    assert_eq!(pty_core::protocol::decode_geometry(&g.payload), (30, 100));
    let mut stats = rig.connect("c5");
    let s = stats.status_json(Duration::from_secs(2));
    assert_eq!(s["terminal"]["rows"], 30);
    assert_eq!(s["terminal"]["cols"], 100);
    conn.detach();
}

/// node: tests/connection.test.ts:214
#[test]
fn send_data_sends_text_to_a_session() {
    let rig = Rig::new();
    rig.daemon("s1", &["sh", "-c", "stty raw -echo; cat"], DaemonOpts::no_display_name());
    std::thread::sleep(Duration::from_millis(150));
    expect_status(&rig.pty(&["send", "s1", "hello-send"]), 0);
    let out = peek_once_it_shows(&rig, &["peek", "--plain", "s1"], "hello-send");
    expect_status(&out, 0);
    expect_contains(&out.stdout(), "hello-send");
}

/// node: tests/connection.test.ts:224
#[test]
fn send_data_rejects_for_nonexistent_session() {
    let rig = Rig::new();
    let out = rig.pty(&["send", "nonexistent", "test"]);
    expect_status(&out, 1);
    expect_contains(&out.stderr(), "Session \"nonexistent\" not found.");
}

/// node: tests/connection.test.ts:257
#[test]
fn paste_wraps_the_payload_in_bracketed_paste_markers() {
    let rig = Rig::new();
    let dump = start_dumper(&rig, "p1");
    expect_status(&rig.pty(&["send", "p1", "--paste", "hello-paste"]), 0);
    let received = wait_for_dump(&dump, "hello-paste".len() + 12);
    assert_eq!(received, b"\x1b[200~hello-paste\x1b[201~");
}

/// node: tests/connection.test.ts:270
#[test]
fn paste_wraps_a_multi_item_payload_as_one_bracket_pair() {
    let rig = Rig::new();
    let dump = start_dumper(&rig, "p2");
    expect_status(
        &rig.pty(&["send", "p2", "--with-delay", "0", "--paste", "--seq", "line1\n", "--seq", "line2\n", "--seq", "line3"]),
        0,
    );
    let expected = b"\x1b[200~line1\nline2\nline3\x1b[201~";
    let received = wait_for_dump(&dump, expected.len());
    let text = String::from_utf8_lossy(&received).into_owned();
    assert_eq!(text.matches("\x1b[200~").count(), 1);
    assert_eq!(text.matches("\x1b[201~").count(), 1);
    assert_eq!(received, expected);
}

/// node: tests/connection.test.ts:285
#[test]
fn no_paste_does_not_add_bracketed_paste_markers() {
    let rig = Rig::new();
    let dump = start_dumper(&rig, "p3");
    expect_status(&rig.pty(&["send", "p3", "no-paste"]), 0);
    let received = wait_for_dump(&dump, "no-paste".len());
    assert_eq!(received, b"no-paste");
}

/// node: tests/connection.test.ts:298
#[test]
fn paste_with_nothing_to_send_does_not_emit_markers_alone() {
    let rig = Rig::new();
    let dump = start_dumper(&rig, "p4");
    let out = rig.pty(&["send", "p4", "--paste"]);
    expect_status(&out, 1);
    expect_contains(&out.stderr(), "Nothing to send.");
    std::thread::sleep(Duration::from_millis(300));
    let received = std::fs::read(&dump).unwrap_or_default();
    assert!(received.is_empty(), "{received:?}");
}

/// node: tests/connection.test.ts:318
#[test]
fn peek_returns_screen_content() {
    let rig = Rig::new();
    rig.daemon("k1", &["sh", "-c", "echo peek-test; exec cat"], DaemonOpts::no_display_name());
    let out = peek_once_it_shows(&rig, &["peek", "k1"], "peek-test");
    expect_status(&out, 0);
    expect_contains(&out.stdout(), "peek-test");
}

/// node: tests/connection.test.ts:330
#[test]
fn peek_returns_plain_text_when_plain() {
    let rig = Rig::new();
    rig.daemon("k2", &["sh", "-c", "echo plain-test; exec cat"], DaemonOpts::no_display_name());
    let out = peek_once_it_shows(&rig, &["peek", "--plain", "k2"], "plain-test");
    expect_status(&out, 0);
    expect_contains(&out.stdout(), "plain-test");
    expect_not_regex(&out.stdout(), "\x1b\\[");
}

/// node: tests/connection.test.ts:343
#[test]
fn peek_rejects_for_nonexistent_session() {
    let rig = Rig::new();
    let out = rig.pty(&["peek", "nonexistent"]);
    expect_status(&out, 1);
    expect_contains(&out.stderr(), "Session \"nonexistent\" not found.");
}
