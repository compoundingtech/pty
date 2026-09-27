//! Parity tests: pin pty-rust behavior to the node pty reference for the
//! divergences the nesting verification found. Node is the reference; these
//! assert the values node emits so the two projects share one behavioral spec.

use libghostty_vt::terminal::{Options, Terminal};
use pty_client::{resolve_seq_delay_ms, DEFAULT_SEQ_DELAY_MS};
use pty_terminal::screenshot::serialize_for_replay;
use pty_testkit::{Session, SpawnOptions};

// ── #3: send --seq pacing (port of node's resolveSeqDelayMs coverage) ──

#[test]
fn seq_delay_defaults_to_300ms() {
    assert_eq!(DEFAULT_SEQ_DELAY_MS, 300);
    assert_eq!(resolve_seq_delay_ms(None), 300);
}

#[test]
fn seq_delay_zero_is_straight_stream() {
    assert_eq!(resolve_seq_delay_ms(Some(0.0)), 0);
}

#[test]
fn seq_delay_n_resolves_to_n_seconds() {
    assert_eq!(resolve_seq_delay_ms(Some(0.1)), 100);
    assert_eq!(resolve_seq_delay_ms(Some(0.5)), 500);
    assert_eq!(resolve_seq_delay_ms(Some(2.0)), 2000);
}

#[test]
fn seq_delay_rounds_not_truncates() {
    // node uses Math.round(sec * 1000): 0.4285 -> 429 (not 428). pty-claude
    // pinned this edge in the node reference tests, so rust matches.
    assert_eq!(resolve_seq_delay_ms(Some(0.4285)), 429);
    assert_eq!(resolve_seq_delay_ms(Some(0.4284)), 428);
}

// ── #2: plain screenshot keeps a trailing WRITTEN space (cursor cell) ──

#[test]
fn plain_capture_keeps_trailing_written_space() {
    // A program writes "PROMPT$ " with NO newline — the trailing space is real
    // written content (like a shell PS1 "$ "). Node's plain serialization keeps
    // it (translateToString(true) trims only never-written cells); pty-rust must
    // match, so the captured line ends with the written space, not trimmed.
    let mut s = Session::spawn(
        "sh",
        &["-c", "printf 'PROMPT$ '; sleep 30"],
        SpawnOptions {
            rows: Some(24),
            cols: Some(80),
            ..Default::default()
        },
    )
    .expect("spawn");
    s.wait_for_text("PROMPT$", 5000).expect("prompt written");
    let ss = s.screenshot();
    assert!(
        ss.lines.iter().any(|l| l == "PROMPT$ "),
        "expected a line exactly 'PROMPT$ ' (trailing written space kept); lines: {:?}",
        ss.lines
    );
    // And it must NOT have been trimmed to "PROMPT$".
    assert!(
        !ss.lines.iter().any(|l| l == "PROMPT$"),
        "trailing written space was trimmed (diverges from node): {:?}",
        ss.lines
    );
    s.close();
}

// ── #D: attach SCREEN replay carries terminal mode-state ──

#[test]
fn replay_serialization_restores_terminal_modes() {
    // A TUI enables mouse tracking (1000) + SGR mouse (1006) and hides the
    // cursor (25l). The attach/replay serialization must carry those modes so a
    // reattaching client restores them (functional parity with node's SCREEN
    // payload — byte-exact ANSI isn't the target, the restored mode-set is).
    let mut t = Terminal::new(Options {
        cols: 80,
        rows: 24,
        max_scrollback: 100,
    })
    .unwrap();
    t.vt_write(b"\x1b[?1000h\x1b[?1006h\x1b[?25lTUI-BODY");
    let replay = serialize_for_replay(&t);

    assert!(replay.contains("TUI-BODY"), "content missing: {replay:?}");
    assert!(
        replay.contains("\x1b[?1000h"),
        "mouse tracking mode not restored: {replay:?}"
    );
    assert!(
        replay.contains("\x1b[?1006h"),
        "SGR mouse mode not restored: {replay:?}"
    );
    assert!(
        replay.contains("\x1b[?25l"),
        "cursor-hidden mode not restored: {replay:?}"
    );
}

#[test]
fn plain_capture_still_drops_never_written_trailing_cells() {
    // "abc\n" — the rest of the row is never written; those cells are dropped
    // (no 80-col padding), matching node.
    let mut s = Session::spawn(
        "sh",
        &["-c", "printf 'abc\\n'; sleep 30"],
        SpawnOptions {
            rows: Some(24),
            cols: Some(80),
            ..Default::default()
        },
    )
    .expect("spawn");
    s.wait_for_text("abc", 5000).expect("written");
    let ss = s.screenshot();
    assert!(
        ss.lines.iter().any(|l| l == "abc"),
        "expected exactly 'abc' (never-written trailing cells dropped): {:?}",
        ss.lines
    );
    s.close();
}
