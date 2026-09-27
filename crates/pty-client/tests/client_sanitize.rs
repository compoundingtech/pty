//! The terminal reset string must be byte-for-byte the Node client's
//! (`src/client.ts:37-59`); `tests/sanitize.test.ts` pins its effect on xterm.

use pty_client::{CLEAR_SCREEN_HOME, CURSOR_TO_BOTTOM, TERMINAL_SANITIZE};

/// node: tests/sanitize.test.ts:35-188
#[test]
fn terminal_sanitize_is_the_exact_node_byte_string() {
    let node = "\x1b[?1049l\x1b[?1l\x1b[?7h\x1b[?6l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[?25h\x1b[?2004l\x1b[4l\x1b[r\x1b[0m\x1b[0 q\x1b>\x1b(B\x1b[<99u";
    assert_eq!(TERMINAL_SANITIZE.as_bytes(), node.as_bytes());
    for required in [
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
