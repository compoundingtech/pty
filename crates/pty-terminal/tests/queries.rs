//! Terminal query answers: the exact bytes the actor gives. The round trip
//! through a real child that echoes them is the `pty` crate's handle test.
//!
//! Port of the pty project's `tests/terminal-queries.test.ts:93-149`.

use pty_terminal::{Range, TerminalActor};

fn actor() -> TerminalActor {
    TerminalActor::new(24, 80, 100)
}

// ── byte-exact answers ──

/// node: src/server.ts:397-405 (DA1), tests/terminal-queries.test.ts:94-105
#[test]
fn da1_answer_is_nodes_bytes_and_stripped_from_data() {
    let mut a = actor();
    for q in [&b"\x1b[c"[..], b"\x1b[0c"] {
        let data = a.write(q);
        assert_eq!(data, b"", "DA1 must not reach DATA");
        assert_eq!(a.take_pty_replies(), b"\x1b[?62;22c");
    }
}

/// node: src/server.ts:491-498 (DA2), tests/terminal-queries.test.ts:140-149
#[test]
fn da2_answer_is_nodes_bytes() {
    let mut a = actor();
    let data = a.write(b"\x1b[>c");
    assert_eq!(data, b"");
    assert_eq!(a.take_pty_replies(), b"\x1b[>0;382;0c");
    assert_eq!(a.write(b"\x1b[>0c"), b"");
    assert_eq!(a.take_pty_replies(), b"\x1b[>0;382;0c");
}

/// node: src/server.ts:499-508 (DSR), tests/terminal-queries.test.ts:127-138
#[test]
fn dsr_reports_one_based_cursor() {
    let mut a = actor();
    assert_eq!(a.write(b"\x1b[6n"), b"");
    assert_eq!(a.take_pty_replies(), b"\x1b[1;1R");
    // Cursor at row 10, column 20 (1-based) → ESC[10;20R.
    let data = a.write(b"\x1b[10;20H\x1b[6n");
    assert_eq!(data, b"\x1b[10;20H");
    assert_eq!(a.take_pty_replies(), b"\x1b[10;20R");
}

/// node: src/server.ts:509-516 (XTVERSION)
#[test]
fn xtversion_answer_is_nodes_bytes() {
    let mut a = actor();
    assert_eq!(a.write(b"\x1b[>0q"), b"");
    assert_eq!(a.take_pty_replies(), b"\x1bP>|pty(0.8)\x1b\\");
    assert_eq!(a.write(b"\x1b[>q"), b"");
    assert_eq!(a.take_pty_replies(), b"\x1bP>|pty(0.8)\x1b\\");
}

/// node: src/server.ts:459-490 (OSC 10/11/4), tests/terminal-queries.test.ts:107-126
#[test]
fn color_queries_answer_with_st_whatever_the_query_terminator() {
    let mut a = actor();
    for (q, reply) in [
        (&b"\x1b]10;?\x1b\\"[..], &b"\x1b]10;rgb:c0c0/c0c0/c0c0\x1b\\"[..]),
        (b"\x1b]10;?\x07", b"\x1b]10;rgb:c0c0/c0c0/c0c0\x1b\\"),
        (b"\x1b]11;?\x1b\\", b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
        (b"\x1b]11;?\x07", b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
        (b"\x1b]4;7;?\x07", b"\x1b]4;7;rgb:0000/0000/0000\x1b\\"),
        (b"\x1b]4;255;?\x1b\\", b"\x1b]4;255;rgb:0000/0000/0000\x1b\\"),
        (b"\x1b]4;0;?\x1b\\", b"\x1b]4;0;rgb:0000/0000/0000\x1b\\"),
    ] {
        let data = a.write(q);
        assert_eq!(data, b"", "{q:?} must not reach DATA");
        assert_eq!(a.take_pty_replies(), reply, "{q:?}");
    }
    // Answer every queried slot in a multi-query, in request order.
    assert_eq!(a.write(b"\x1b]4;1;?;2;?\x07"), b"");
    assert_eq!(
        a.take_pty_replies(),
        b"\x1b]4;1;rgb:0000/0000/0000\x1b\\\x1b]4;2;rgb:0000/0000/0000\x1b\\"
    );
    // A non-query OSC 10 (a set) passes through and is not answered.
    let set = b"\x1b]10;rgb:ffff/0000/0000\x07";
    assert_eq!(a.write(set), set);
    assert_eq!(a.take_pty_replies(), b"");
}

/// Replies come out in stream order even when a colour query sits between
/// two device queries in one chunk.
#[test]
fn replies_keep_stream_order() {
    let mut a = actor();
    let data = a.write(b"a\x1b[c\x1b]11;?\x07b\x1b[>c c");
    assert_eq!(data, b"ab c");
    assert_eq!(
        a.take_pty_replies(),
        b"\x1b[?62;22c\x1b]11;rgb:0000/0000/0000\x1b\\\x1b[>0;382;0c"
    );
    assert_eq!(a.plain(Range::Viewport), "ab c");
}

/// A query split across two PTY reads is still answered once and never
/// reaches DATA.
#[test]
fn split_query_is_answered_once() {
    let mut a = actor();
    let mut data = a.write(b"x\x1b]1");
    data.extend(a.write(b"1;?\x1b"));
    data.extend(a.write(b"\\y\x1b["));
    data.extend(a.write(b"cz"));
    assert_eq!(data, b"xyz");
    assert_eq!(
        a.take_pty_replies(),
        b"\x1b]11;rgb:0000/0000/0000\x1b\\\x1b[?62;22c"
    );
}

#[test]
fn clipboard_read_without_a_client_has_a_bounded_empty_reply() {
    let mut a = actor();
    let mut broadcast = a.write(b"text\x1b]52;c;");
    broadcast.extend(a.write(b"?\x07more"));
    assert_eq!(broadcast, b"textmore");
    assert_eq!(a.take_pty_replies(), b"\x1b]52;c;\x1b\\");
}

#[test]
fn clipboard_read_reaches_an_attached_terminal() {
    let mut a = actor();
    a.set_clipboard_client_available(true);
    assert_eq!(a.write(b"\x1b]52;c;?\x07"), b"\x1b]52;c;?\x07");
    assert!(a.take_pty_replies().is_empty());
}

#[test]
fn screen_cell_query_reports_the_virtual_surface_without_leaking_to_clients() {
    let mut a = actor();
    assert_eq!(a.write(b"\x1b[19t"), b"");
    assert_eq!(a.take_pty_replies(), b"\x1b[9;24;80t");
}

#[test]
fn input_recovery_retains_history_and_clears_both_screen_keyboard_stacks() {
    let mut a = actor();
    a.write(b"copper history\r\n\x1b[>27u\x1b[?1049h\x1b[>31u\x1b[?1003h\x1b[?1016h\x1b[?2004h\x1b[>4;2m");
    let output = a.reset_input_modes();
    assert!(!output.is_empty());
    assert!(!a.modes().mouse_reporting());
    assert!(!a.modes().bracketed_paste);
    assert_eq!(a.kitty_flags(), 0);
    assert!(a.modes().kitty_stack.is_empty());
    assert!(a.plain(Range::Full).contains("copper history"));
    a.write(b"\x1b[?1049h");
    assert_eq!(a.kitty_flags(), 0, "inactive screen stack is also reset");
    a.write(b"\x1b[?1049l\x1b[?u\x1b[?1003$p");
    assert_eq!(a.take_pty_replies(), b"\x1b[?0u\x1b[?1003;2$y");
    assert_eq!(
        a.encode_key(&pty_terminal::KeyEvent::typed(
            pty_terminal::Key::A,
            "a",
            Some('a')
        )),
        b"a"
    );
}

#[test]
fn modify_other_keys_query_answers_the_selected_level_in_stream_order() {
    let mut a = actor();
    assert_eq!(a.write(b"\x1b[?4m"), b"");
    assert_eq!(a.take_pty_replies(), b"\x1b[>4;0m");
    a.write(b"\x1b[>4;2m\x1b[?4m\x1b[c");
    assert_eq!(a.take_pty_replies(), b"\x1b[>4;2m\x1b[?62;22c");
    a.reset_input_modes();
    a.write(b"\x1b[?4m");
    assert_eq!(a.take_pty_replies(), b"\x1b[>4;0m");
}

#[test]
fn normal_screen_recovery_does_not_restore_an_unrelated_saved_cursor() {
    let mut a = actor();
    a.write(b"copper history\r\n\x1b[?1003h\x1b[>27u");
    a.reset_input_modes();
    a.write(b"after recovery\r\n");
    let text = a.plain(Range::Full);
    assert!(text.contains("copper history"), "{text:?}");
    assert!(text.contains("after recovery"), "{text:?}");
}
