//! Port of tests/sanitize.test.ts. Node unit-tests its `TERMINAL_SANITIZE`
//! constant against an in-process xterm; a binary under test exposes the
//! same bytes through `pty attach` (written to the terminal when the
//! session exits or the client detaches) and through a non-plain `pty peek`
//! (appended after the screen). Each Node case becomes a check that the
//! reset sequence it relies on is present in what the CLI actually emits,
//! plus one test pinning the whole string and the trailers around it.
//!
//! The behavioural half of each Node case (feeding the bytes into an
//! emulator and checking the mode really flipped) is not re-expressed: the
//! sequences are standard and the byte check is what a drop-in binary must
//! reproduce.

use pty_conformance::*;
use std::time::Duration;

/// The Node sanitize sequence, split around the optional Rust host-color reset.
const TERMINAL_SANITIZE_PREFIX: &str = concat!(
    "\x1b[?1049l", "\x1b[?1l", "\x1b[?7h", "\x1b[?6l", "\x1b[?1000l", "\x1b[?1002l", "\x1b[?1003l",
    "\x1b[?1004l", "\x1b[?1006l", "\x1b[?25h", "\x1b[?2004l", "\x1b[4l", "\x1b[r", "\x1b[0m",
);
const HOST_COLOR_RESETS: &str = "\x1b]104\x1b\\\x1b]110\x1b\\\x1b]111\x1b\\\x1b]112\x1b\\";
const TERMINAL_SANITIZE_SUFFIX: &str = "\x1b[0 q\x1b>\x1b(B\x1b[<99u";
/// Rust also turns xterm modifyOtherKeys off, which Node leaves set.
const MODIFY_OTHER_KEYS_RESET: &str = "\x1b[>4m";
const CURSOR_TO_BOTTOM: &str = "\x1b[999;1H";

/// Run `pty attach` in a tty on a session that prints READY and exits with
/// code 3 once the attached client sends a line; return (exit code, raw
/// bytes written to the terminal).
fn attach_exit_bytes(rig: &Rig, id: &str) -> (Option<i32>, String) {
    rig.daemon(id, &["sh", "-c", "echo READY; read _line; exit 3"], DaemonOpts::keep());
    let mut t = rig.pty_tty_raw(&[], &[], &["attach", id], 20, 60);
    assert!(t.wait_for_text("READY", deadline()), "attach never showed READY: {:?}", t.output_str());
    t.write(b"go\n");
    let code = t.wait_exit(deadline());
    (code, t.output_str())
}

/// Run `pty attach` in a tty on a live `cat` session and detach with a
/// single Ctrl-\; return (exit code, raw bytes).
fn attach_detach_bytes(rig: &Rig, id: &str) -> (Option<i32>, String) {
    rig.daemon(id, &["sh", "-c", "echo READY; exec cat"], DaemonOpts::no_display_name());
    let mut t = rig.pty_tty_raw(&[], &[], &["attach", id], 20, 60);
    assert!(t.wait_for_text("READY", deadline()), "attach never showed READY: {:?}", t.output_str());
    t.write(&[0x1c]);
    let code = t.wait_exit(deadline());
    (code, t.output_str())
}

fn after_sanitize(out: &str) -> &str {
    let i = out.find(TERMINAL_SANITIZE_PREFIX).unwrap_or_else(|| panic!("no sanitize prefix in {out:?}"));
    let tail = &out[i + TERMINAL_SANITIZE_PREFIX.len()..];
    let tail = tail.strip_prefix(HOST_COLOR_RESETS).unwrap_or(tail);
    let tail = tail.strip_prefix(TERMINAL_SANITIZE_SUFFIX).unwrap_or_else(|| panic!("no sanitize suffix in {out:?}"));
    tail.strip_prefix(MODIFY_OTHER_KEYS_RESET).unwrap_or(tail)
}

/// The whole reset string, then cursor-to-bottom and the exit trailer, when
/// the attached session exits.
/// node: tests/sanitize.test.ts:28
#[test]
fn attach_emits_sanitize_then_exit_trailer() {
    let rig = Rig::new();
    let (code, out) = attach_exit_bytes(&rig, "sx");
    assert_eq!(code, Some(3), "attach exit code; output {out:?}");
    expect_contains(&out, TERMINAL_SANITIZE_PREFIX);
    let tail = after_sanitize(&out);
    assert!(tail.starts_with(CURSOR_TO_BOTTOM), "cursor-to-bottom must follow sanitize: {tail:?}");
    expect_contains(tail, "[sx exited with code 3 after ");
    expect_contains(tail, "  restart: pty attach sx");
    assert_eq!(out.matches(TERMINAL_SANITIZE_PREFIX).count(), 1, "sanitize emitted once: {out:?}");
}

/// The same reset string and a `[detached from <id>]` trailer on a local detach.
/// node: tests/sanitize.test.ts:28
#[test]
fn detach_emits_sanitize_then_detached_trailer() {
    let rig = Rig::new();
    let (code, out) = attach_detach_bytes(&rig, "sd");
    assert_eq!(code, Some(0), "detach exit code; output {out:?}");
    expect_contains(&out, TERMINAL_SANITIZE_PREFIX);
    let tail = after_sanitize(&out);
    assert!(tail.starts_with(CURSOR_TO_BOTTOM), "cursor-to-bottom must follow sanitize: {tail:?}");
    expect_contains(tail, "[detached from sd]");
    expect_contains(tail, "  reattach: pty attach sd");
    expect_not_contains(&out, "exited with code");
}

/// A non-plain `pty peek` appends the reset string, cursor-to-bottom and a
/// newline after the screen; `--plain` emits none of it.
/// node: tests/sanitize.test.ts:28
#[test]
fn peek_emits_sanitize_after_screen() {
    let rig = Rig::new();
    rig.daemon("sp", &["sh", "-c", "echo PEEK-READY; exec cat"], DaemonOpts::no_display_name());
    wait_until("output on screen", || rig.pty(&["peek", "--plain", "sp"]).stdout().contains("PEEK-READY"));
    let out = rig.pty(&["peek", "sp"]);
    expect_status(&out, 0);
    let s = out.stdout();
    expect_contains(&s, "PEEK-READY");
    let screen_end = s.find(TERMINAL_SANITIZE_PREFIX).unwrap_or_else(|| panic!("no sanitize in peek output {s:?}"));
    assert!(screen_end > 0);
    assert_eq!(after_sanitize(&s), format!("{CURSOR_TO_BOTTOM}\n"));
    let plain = rig.pty(&["peek", "--plain", "sp"]);
    expect_status(&plain, 0);
    expect_not_contains(&plain.stdout(), "\x1b[");
}

/// node: tests/sanitize.test.ts:35
#[test]
fn re_enables_autowrap() {
    let rig = Rig::new();
    let (_, out) = attach_exit_bytes(&rig, "aw");
    expect_contains(&out, "\x1b[?7h");
}

/// node: tests/sanitize.test.ts:56
#[test]
fn resets_g0_charset_to_ascii() {
    let rig = Rig::new();
    let (_, out) = attach_exit_bytes(&rig, "g0");
    expect_contains(&out, "\x1b(B");
}

/// node: tests/sanitize.test.ts:79
#[test]
fn resets_insert_mode() {
    let rig = Rig::new();
    let (_, out) = attach_exit_bytes(&rig, "irm");
    expect_contains(&out, "\x1b[4l");
}

/// node: tests/sanitize.test.ts:104
#[test]
fn resets_origin_mode_and_scroll_region() {
    let rig = Rig::new();
    let (_, out) = attach_exit_bytes(&rig, "om");
    expect_contains(&out, "\x1b[?6l");
    expect_contains(&out, "\x1b[r");
}

/// node: tests/sanitize.test.ts:128
#[test]
fn resets_scroll_region_on_detach() {
    let rig = Rig::new();
    let (_, out) = attach_detach_bytes(&rig, "sr");
    expect_contains(&out, "\x1b[r");
}

/// node: tests/sanitize.test.ts:162
#[test]
fn includes_keypad_mode_reset() {
    let rig = Rig::new();
    let (_, out) = attach_exit_bytes(&rig, "kp");
    expect_contains(&out, "\x1b>");
}

/// node: tests/sanitize.test.ts:172
#[test]
fn disables_focus_event_reporting() {
    let rig = Rig::new();
    let (_, out) = attach_exit_bytes(&rig, "fo");
    expect_contains(&out, "\x1b[?1004l");
}

/// node: tests/sanitize.test.ts:182
#[test]
fn resets_cursor_style() {
    let rig = Rig::new();
    let (_, out) = attach_detach_bytes(&rig, "cs");
    assert!(out.contains("\x1b[0 q") || out.contains("\x1b[ q"), "no cursor style reset in {out:?}");
    let _ = Duration::ZERO;
}
