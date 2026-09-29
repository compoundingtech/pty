//! Port of tests/send-paste.test.ts: `pty send --paste` bracketed-paste
//! framing, strict flag parsing, and key notation. The session runs
//! `sh -c 'stty raw -echo; cat > dump'` so the exact bytes written to the
//! pty can be read back from the dump file.

use pty_conformance::*;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn start_dump_session(rig: &Rig, name: &str) -> PathBuf {
    let dump = rig.root().join(format!("{name}.dump.bin"));
    let script = format!("stty raw -echo; printf '\\033[?2004h'; cat > '{}'", dump.display());
    rig.daemon(name, &["sh", "-c", &script], DaemonOpts::no_display_name());
    // Let `stty raw -echo` take effect before the first write.
    std::thread::sleep(Duration::from_millis(150));
    dump
}

fn wait_for_dump(dump: &Path, min_bytes: usize, timeout: Duration) -> Vec<u8> {
    let _ = poll_for(timeout, || {
        std::fs::read(dump).map(|b| b.len() >= min_bytes).unwrap_or(false)
    });
    std::fs::read(dump).unwrap_or_default()
}

fn dump_str(dump: &Path, min_bytes: usize, timeout_ms: u64) -> String {
    String::from_utf8_lossy(&wait_for_dump(dump, min_bytes, Duration::from_millis(timeout_ms))).into_owned()
}

/// node: tests/send-paste.test.ts:122
#[test]
fn paste_wraps_positional_text_in_bracketed_paste_markers() {
    let rig = Rig::new();
    let name = unique_id("sp");
    let dump = start_dump_session(&rig, &name);
    let out = rig.pty(&["send", &name, "--paste", "hello-paste"]);
    expect_status(&out, 0);
    let received = dump_str(&dump, "hello-paste".len() + 12, 3000);
    assert_eq!(received, "\x1b[200~hello-paste\x1b[201~");
}

/// node: tests/send-paste.test.ts:135
#[test]
fn paste_flag_after_the_text_works() {
    let rig = Rig::new();
    let name = unique_id("sp");
    let dump = start_dump_session(&rig, &name);
    let out = rig.pty(&["send", &name, "post-paste", "--paste"]);
    expect_status(&out, 0);
    let received = dump_str(&dump, "post-paste".len() + 12, 3000);
    assert_eq!(received, "\x1b[200~post-paste\x1b[201~");
}

/// node: tests/send-paste.test.ts:148
#[test]
fn paste_wraps_an_ordered_seq_payload_as_one_paste() {
    let rig = Rig::new();
    let name = unique_id("sp");
    let dump = start_dump_session(&rig, &name);
    let out = rig.pty(&[
        "send", &name, "--paste", "--seq", "first ", "--seq", "second ", "--seq", "third",
    ]);
    expect_status(&out, 0);
    let expected = "\x1b[200~first second third\x1b[201~";
    let received = dump_str(&dump, expected.len(), 3000);
    assert_eq!(received.matches("\x1b[200~").count(), 1);
    assert_eq!(received.matches("\x1b[201~").count(), 1);
    assert_eq!(received, expected);
}

/// node: tests/send-paste.test.ts:168
#[test]
fn paste_composes_with_with_delay() {
    let rig = Rig::new();
    let name = unique_id("sp");
    let dump = start_dump_session(&rig, &name);
    let out = rig.pty(&[
        "send", &name, "--with-delay", "0.05", "--paste", "--seq", "A", "--seq", "B",
    ]);
    expect_status(&out, 0);
    let expected = "\x1b[200~AB\x1b[201~";
    let received = dump_str(&dump, expected.len(), 3000);
    assert_eq!(received.matches("\x1b[200~").count(), 1);
    assert_eq!(received.matches("\x1b[201~").count(), 1);
    assert_eq!(received, expected);
}

/// node: tests/send-paste.test.ts:188
#[test]
fn without_paste_no_markers_are_emitted() {
    let rig = Rig::new();
    let name = unique_id("sp");
    let dump = start_dump_session(&rig, &name);
    let out = rig.pty(&["send", &name, "plain-text"]);
    expect_status(&out, 0);
    let received = dump_str(&dump, "plain-text".len(), 3000);
    assert_eq!(received, "plain-text");
}

/// node: tests/send-paste.test.ts:203
#[test]
fn paste_keeps_a_multi_line_payload_in_one_paste() {
    let rig = Rig::new();
    let name = unique_id("sp");
    let dump = start_dump_session(&rig, &name);
    let out = rig.pty(&["send", &name, "--paste", "line-one\nline-two\n"]);
    expect_status(&out, 0);
    let expected = "\x1b[200~line-one\nline-two\n\x1b[201~";
    let received = dump_str(&dump, expected.len(), 3000);
    assert_eq!(received, expected);
    assert_eq!(received.matches("\x1b[200~").count(), 1);
    assert_eq!(received.matches("\x1b[201~").count(), 1);
}

// ── strict flag parsing (#20) ──

/// node: tests/send-paste.test.ts:224
#[test]
fn rejects_an_unknown_flag_after_positional_text() {
    let rig = Rig::new();
    let out = rig.pty(&["send", "somename", "hello world", "--bogus"]);
    expect_failure(&out);
    let err = out.stderr();
    expect_contains(&err, "Unexpected argument");
    expect_contains(&err, "--bogus");
}

/// node: tests/send-paste.test.ts:231
#[test]
fn suggests_seq_key_return_for_enter() {
    let rig = Rig::new();
    let out = rig.pty(&["send", "somename", "sudo cmd", "--enter"]);
    expect_failure(&out);
    let err = out.stderr();
    expect_contains(&err, "--enter");
    expect_contains(&err, "--seq");
    expect_contains(&err, "key:return");
}

/// node: tests/send-paste.test.ts:239
#[test]
fn suggests_the_real_syntax_for_newline_return_and_cr() {
    let rig = Rig::new();
    for flag in ["--newline", "--return", "--cr"] {
        let out = rig.pty(&["send", "somename", "text", flag]);
        expect_failure(&out);
        let err = out.stderr();
        expect_contains(&err, flag);
        expect_contains(&err, "key:return");
    }
}

/// node: tests/send-paste.test.ts:248
#[test]
fn plain_positional_text_still_works() {
    let rig = Rig::new();
    let name = unique_id("sp");
    let dump = start_dump_session(&rig, &name);
    let out = rig.pty(&["send", &name, "still-works"]);
    expect_status(&out, 0);
    let received = dump_str(&dump, "still-works".len(), 3000);
    assert_eq!(received, "still-works");
}

// ── key notation (#164) ──

/// node: tests/send-paste.test.ts:263
#[test]
fn control_key_spellings_are_equivalent() {
    let rig = Rig::new();
    let name = unique_id("sp");
    let dump = start_dump_session(&rig, &name);
    let out = rig.pty(&[
        "send", &name, "--with-delay", "0", "--seq", "key:ctrl+u", "--seq", "key:ctrl-u", "--seq",
        "key:ctrl_u", "--seq", "key:C-u",
    ]);
    expect_status(&out, 0);
    assert_eq!(wait_for_dump(&dump, 4, Duration::from_millis(3000)), b"\x15\x15\x15\x15");
}

/// node: tests/send-paste.test.ts:288
#[test]
fn validates_the_whole_sequence_before_delivering_anything() {
    let rig = Rig::new();
    let name = unique_id("sp");
    let dump = start_dump_session(&rig, &name);
    let out = rig.pty(&[
        "send", &name, "--with-delay", "0", "--seq", "PARTIAL", "--seq", "key:ctrl-", "--seq", "AFTER",
    ]);
    expect_failure(&out);
    expect_regex(&out.stderr(), "(?is)Incomplete key spec.*ctrl-u.*supported keys");
    assert_eq!(wait_for_dump(&dump, 1, Duration::from_millis(500)), b"");
}
