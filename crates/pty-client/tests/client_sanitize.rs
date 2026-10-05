//! The terminal reset string extends the Node client's sequence
//! (`src/client.ts:37-59`) with host color resets and a modifyOtherKeys reset.

use pty_client::{CLEAR_SCREEN_HOME, CURSOR_TO_BOTTOM, TERMINAL_SANITIZE};

/// The base sequence follows node: tests/sanitize.test.ts:35-188.
#[test]
fn terminal_sanitize_resets_host_colors_after_node_modes() {
    let expected = "\x1b[?1049l\x1b[?1l\x1b[?7h\x1b[?6l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[?25h\x1b[?2004l\x1b[4l\x1b[r\x1b[0m\x1b]104\x1b\\\x1b]110\x1b\\\x1b]111\x1b\\\x1b]112\x1b\\\x1b[0 q\x1b>\x1b(B\x1b[<99u\x1b[>4m";
    assert_eq!(TERMINAL_SANITIZE.as_bytes(), expected.as_bytes());
    for required in [
        // A session's vim or Claude Code sets modifyOtherKeys 2. Left on,
        // iTerm2 sends `CSI 27 ; 5 ; 99 ~` for Ctrl+C at the shell prompt.
        "\x1b[>4m",
        "\x1b>",
        "\x1b[?1004l",
        "\x1b[0 q",
        "\x1b(B",
        "\x1b[4l",
        "\x1b[r",
        "\x1b[?7h",
    ] {
        assert!(TERMINAL_SANITIZE.contains(required), "missing {required:?}");
    }
}

#[test]
fn cursor_and_clear_sequences() {
    assert_eq!(CURSOR_TO_BOTTOM, "\x1b[999;1H");
    assert_eq!(CLEAR_SCREEN_HOME, "\x1b[2J\x1b[H");
}
