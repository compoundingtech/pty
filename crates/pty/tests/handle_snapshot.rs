//! Typed cell reads through a spawned child. Port of the pty project's
//! `tests/pty-handle.test.ts`.

use std::time::Duration;

use pty::{SpawnOptions, TerminalHandle};
use pty_terminal::{CellGrid, ColorSnap, Wide};

fn spawn(cmd: &str, args: &[&str], rows: u16, cols: u16, scrollback: usize) -> TerminalHandle {
    TerminalHandle::spawn(
        cmd,
        args,
        SpawnOptions {
            rows,
            cols,
            scrollback,
            ..Default::default()
        },
    )
    .expect("spawn")
}

fn sh(script: &str, rows: u16, cols: u16, scrollback: usize) -> TerminalHandle {
    spawn("sh", &["-c", script], rows, cols, scrollback)
}

fn wait_until(h: &TerminalHandle, mut pred: impl FnMut(&TerminalHandle) -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if pred(h) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out; screen:\n{}",
            h.snapshot(0).text()
        );
        h.wait_rev(h.rev(), Duration::from_millis(200));
    }
}

fn find<'a>(g: &'a CellGrid, ch: &str) -> Option<&'a pty_terminal::CellSnap> {
    g.find(ch)
}

// ── cursorRow / cursorCol ──

/// node: tests/pty-handle.test.ts:68-72
#[test]
fn initial_cursor_is_origin() {
    let h = spawn("cat", &[], 24, 80, 0);
    assert_eq!(h.cursor(), (0, 0, true));
    h.kill();
}

/// node: tests/pty-handle.test.ts:74-79
#[test]
fn cursor_moves_when_output_is_written() {
    let h = sh("printf 'hello'; sleep 10", 24, 80, 0);
    wait_until(&h, |h| h.cursor().1 > 0);
    assert_eq!(h.cursor().0, 0);
    assert_eq!(h.cursor().1, 5);
    h.kill();
}

/// node: tests/pty-handle.test.ts:81-86
#[test]
fn cursor_row_advances_with_newlines() {
    let h = sh("printf 'a\\nb\\nc'; sleep 10", 24, 80, 0);
    wait_until(&h, |h| h.cursor().0 >= 2);
    assert_eq!(h.cursor().0, 2);
    assert_eq!(h.cursor().1, 1);
    h.kill();
}

// ── mouseMode ──

/// node: tests/pty-handle.test.ts:91-120
#[test]
fn mouse_mode_tracks_1000_1002_1003() {
    let h = spawn("cat", &[], 24, 80, 0);
    assert!(!h.modes().mouse_tracking());
    h.kill();

    for mode in ["1000", "1002", "1003"] {
        let h = sh(&format!("printf '\\033[?{mode}h'; sleep 10"), 24, 80, 0);
        wait_until(&h, |h| h.modes().mouse_tracking());
        h.kill();
    }

    let h = sh("printf '\\033[?1000h'; sleep 0.1; printf '\\033[?1000l'; sleep 10", 24, 80, 0);
    wait_until(&h, |h| h.modes().mouse_tracking());
    wait_until(&h, |h| !h.modes().mouse_tracking());
    h.kill();
}

// ── alternateScreen ──

/// node: tests/pty-handle.test.ts:125-143
#[test]
fn alternate_screen_flag() {
    let h = spawn("cat", &[], 24, 80, 0);
    assert!(!h.modes().alt_screen);
    h.kill();
    let h = sh("printf '\\033[?1049h'; sleep 0.1; printf '\\033[?1049l'; sleep 10", 24, 80, 0);
    wait_until(&h, |h| h.modes().alt_screen);
    wait_until(&h, |h| !h.modes().alt_screen);
    h.kill();
}

// ── kittyKeyboardFlags ──

/// node: tests/pty-handle.test.ts:148-179
#[test]
fn kitty_keyboard_stack() {
    let h = spawn("cat", &[], 24, 80, 0);
    assert!(h.modes().kitty_stack.is_empty());
    h.kill();

    let h = sh("printf '\\033[>7u'; sleep 10", 24, 80, 0);
    wait_until(&h, |h| !h.modes().kitty_stack.is_empty());
    assert_eq!(h.modes().kitty_stack, vec![7]);
    let mut copy = h.modes().kitty_stack;
    copy.push(99);
    assert_eq!(h.modes().kitty_stack, vec![7], "modes() is a copy");
    h.kill();

    let h = sh("printf '\\033[>1u\\033[>15u'; sleep 10", 24, 80, 0);
    wait_until(&h, |h| h.modes().kitty_stack.len() == 2);
    assert_eq!(h.modes().kitty_stack, vec![1, 15]);
    h.kill();

    let h = sh("printf '\\033[>1u\\033[>15u\\033[<u'; sleep 10", 24, 80, 0);
    wait_until(&h, |h| h.modes().kitty_stack.len() == 1);
    assert_eq!(h.modes().kitty_stack, vec![1]);
    h.kill();
}

// ── readWrappedFlags ──

/// node: tests/pty-handle.test.ts:184-190
#[test]
fn one_wrapped_flag_per_visible_row() {
    let h = spawn("cat", &[], 10, 40, 0);
    let g = h.snapshot(0);
    assert_eq!(g.wrapped.len(), 10);
    assert_eq!(g.rows.len(), 10);
    h.kill();
}

/// node: tests/pty-handle.test.ts:192-208
#[test]
fn continuation_rows_are_flagged_when_a_long_line_overflows() {
    let h = sh("printf 'a%.0s' $(seq 1 120); sleep 5", 12, 40, 0);
    wait_until(&h, |h| h.snapshot(0).wrapped.iter().any(|&f| f));
    let g = h.snapshot(0);
    assert!(!g.wrapped[0]);
    assert!(g.wrapped[1]);
    assert!(g.wrapped[2]);
    assert!(!g.wrapped[3]);
    h.kill();
}

/// node: tests/pty-handle.test.ts:210-217
#[test]
fn short_lines_produce_no_wrapped_flags() {
    let h = sh("printf 'short\\n'; sleep 5", 8, 40, 0);
    wait_until(&h, |h| h.cursor().0 >= 1);
    assert!(h.snapshot(0).wrapped.iter().all(|&f| !f));
    h.kill();
}

/// node: tests/pty-handle.test.ts:219-236
#[test]
fn scroll_offset_keeps_flags_and_cells_aligned() {
    let h = sh("for i in $(seq 1 30); do echo line $i; done; sleep 5", 10, 40, 100);
    wait_until(&h, |h| h.buffer_length() > h.rows() as usize);
    let g0 = h.snapshot(0);
    assert_eq!(g0.wrapped.len(), g0.rows.len());
    let g5 = h.snapshot(5);
    assert_eq!(g5.wrapped.len(), g5.rows.len());
    assert_eq!(g5.start, g0.start - 5);
    h.kill();
}

// ── scrollback ──

/// node: tests/pty-handle.test.ts:241-256
#[test]
fn scrollback_defaults_and_buffer_length() {
    let h = spawn("cat", &[], 24, 80, 0);
    assert_eq!(h.scrollback(), 0);
    assert_eq!(h.base_y(), 0);
    h.kill();
    let h = spawn("cat", &[], 24, 80, 500);
    assert_eq!(h.scrollback(), 500);
    h.kill();
    let h = spawn("cat", &[], 10, 80, 0);
    assert_eq!(h.buffer_length(), 10);
    h.kill();
}

/// node: tests/pty-handle.test.ts:258-279
#[test]
fn buffer_length_grows_into_scrollback_and_base_y_stays_zero_without() {
    let h = sh("for i in $(seq 1 50); do echo line-$i; done; sleep 10", 10, 80, 100);
    wait_until(&h, |h| h.base_y() > 0);
    assert!(h.buffer_length() > 10);
    h.kill();

    let h = sh("for i in $(seq 1 50); do echo line-$i; done; sleep 10", 10, 80, 0);
    wait_until(&h, |h| h.snapshot(0).text().contains("line-50"));
    assert_eq!(h.base_y(), 0);
    assert_eq!(h.buffer_length(), 10);
    h.kill();
}

// ── readCells ──

/// node: tests/pty-handle.test.ts:284-290
#[test]
fn read_cells_is_viewport_sized() {
    let h = spawn("cat", &[], 10, 20, 0);
    let g = h.snapshot(0);
    assert_eq!(g.rows.len(), 10);
    assert_eq!(g.rows[0].len(), 20);
    assert_eq!((g.rows_n, g.cols), (10, 20));
    h.kill();
}

/// node: tests/pty-handle.test.ts:292-333
#[test]
fn read_cells_live_history_and_clamping() {
    let h = sh("for i in $(seq 1 50); do echo line-$i; done; sleep 10", 10, 80, 100);
    wait_until(&h, |h| h.snapshot(0).text().contains("line-50"));
    let live = h.snapshot(0).text();
    assert!(live.contains("line-4") || live.contains("line-5"), "{live}");
    let history = h.snapshot(h.base_y()).text();
    assert!(history.lines().any(|l| l.trim_end() == "line-1"), "{history}");
    let clamped = h.snapshot(99_999);
    assert_eq!(clamped.rows.len(), 10);
    assert_eq!(clamped.start, 0);
    h.kill();

    let h = sh("echo hello-world; sleep 10", 10, 80, 0);
    wait_until(&h, |h| h.snapshot(0).text().contains("hello-world"));
    h.kill();
}

// ── palette-indexed colours ──

/// node: tests/pty-handle.test.ts:363-415
#[test]
fn palette_indices_are_preserved_and_truecolor_is_not_indexed() {
    let cases: &[(&str, &str, ColorSnap, ColorSnap)] = &[
        ("\\033[34mB\\033[0m", "B", ColorSnap::Indexed(4), ColorSnap::Default),
        ("\\033[94mX\\033[0m", "X", ColorSnap::Indexed(12), ColorSnap::Default),
        ("\\033[38;5;17mY\\033[0m", "Y", ColorSnap::Indexed(17), ColorSnap::Default),
        ("\\033[48;5;124mZ\\033[0m", "Z", ColorSnap::Default, ColorSnap::Indexed(124)),
        ("\\033[38;2;10;20;30mT\\033[0m", "T", ColorSnap::Rgb(10, 20, 30), ColorSnap::Default),
        ("D", "D", ColorSnap::Default, ColorSnap::Default),
        ("\\033[31;42mM\\033[0m", "M", ColorSnap::Indexed(1), ColorSnap::Indexed(2)),
    ];
    for (printf, ch, fg, bg) in cases {
        let h = sh(&format!("printf '{printf}'; sleep 10"), 24, 80, 0);
        wait_until(&h, |h| find(&h.snapshot(0), ch).is_some());
        let g = h.snapshot(0);
        let cell = find(&g, ch).unwrap();
        assert_eq!(cell.fg, *fg, "{printf}");
        assert_eq!(cell.bg, *bg, "{printf}");
        h.kill();
    }
}

#[test]
fn attributes_wide_chars_and_graphemes() {
    let h = sh("printf '\\033[1;2;3;4;7;9mS\\033[0m\\344\\270\\255e\\314\\201'; sleep 10", 3, 20, 0);
    wait_until(&h, |h| h.cursor().1 >= 4);
    let g = h.snapshot(0);
    let s = &g.rows[0][0];
    assert!(s.bold && s.dim && s.italic && s.underline && s.inverse && s.strikethrough);
    assert_eq!(g.rows[0][1].text, "中");
    assert_eq!(g.rows[0][1].wide, Wide::Wide);
    assert_eq!(g.rows[0][2].text, "");
    assert_eq!(g.rows[0][2].wide, Wide::Spacer);
    assert_eq!(g.rows[0][3].text, "e\u{301}");
    assert_eq!(g.rows[0][4].text, " ");
    h.kill();
}
