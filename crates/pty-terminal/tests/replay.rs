//! The SCREEN replay: Node's mode prefix, the modes libghostty serializes
//! itself, and the ratatui-style round trip (serialize in one actor, parse in
//! another, same picture).

use pty_terminal::{
    CellSize, ColorSnap, Modes, Notification, Range, SerializeOpts, TerminalActor, TerminalEvent,
};
use libghostty_vt::style::{RgbColor, StyleColor, Underline};

fn actor() -> TerminalActor {
    TerminalActor::new(24, 80, 100)
}

#[test]
fn every_palette_slot_in_one_query_gets_a_reply() {
    let mut a = actor();
    a.write(b"\x1b]4;1;?;2;?\x07");
    assert_eq!(
        a.take_pty_replies(),
        b"\x1b]4;1;rgb:0000/0000/0000\x1b\\\x1b]4;2;rgb:0000/0000/0000\x1b\\"
    );
}

#[test]
fn pixel_queries_use_a_declared_cell_size() {
    let mut a = actor();
    a.set_cell_size(CellSize { width: 10, height: 21 });
    a.write(b"\x1b[16t\x1b[14t");
    assert_eq!(a.take_pty_replies(), b"\x1b[6;21;10t\x1b[4;504;800t");
}

#[test]
fn replay_restores_the_pen_for_text_written_after_attach() {
    let mut source = actor();
    source.write(b"\x1b[4:3;58:2::255:0:0mBEFORE ");
    let mut late = actor();
    late.write(source.serialize(SerializeOpts::ATTACH).as_bytes());
    late.write(b"AFTER");
    let pen = late.terminal().cursor_style().unwrap();
    assert_eq!(pen.underline, Underline::Curly);
    assert_eq!(pen.underline_color, StyleColor::Rgb(RgbColor { r: 255, g: 0, b: 0 }));
}

// ── alt-screen prefix (tests/screen-replay-altscreen.test.ts) ──

/// node: tests/screen-replay-altscreen.test.ts:62-83
#[test]
fn attach_prefixes_1049h_at_byte_zero_when_in_alt_screen() {
    let mut a = actor();
    a.write(b"\x1b[?1049h\x1b[Halt-marker");
    let screen = a.serialize(SerializeOpts::ATTACH);
    assert!(screen.starts_with("\x1b[?1049h"), "{screen:?}");
    assert!(screen.contains("alt-marker"));
    assert!(a.modes().alt_screen);
    assert!(a.alt_screen_active());
}

/// node: tests/screen-replay-altscreen.test.ts:85-100
#[test]
fn attach_does_not_prefix_1049h_on_main_screen() {
    let mut a = actor();
    a.write(b"main-only");
    let screen = a.serialize(SerializeOpts::ATTACH);
    assert!(!screen.starts_with("\x1b[?1049h"), "{screen:?}");
    assert!(screen.contains("main-only"));
}

/// node: tests/screen-replay-altscreen.test.ts:102-117
#[test]
fn attach_stops_prefixing_after_child_leaves_alt_screen() {
    let mut a = actor();
    a.write(b"\x1b[?1049h\x1b[?1049lmain-again");
    let screen = a.serialize(SerializeOpts::ATTACH);
    assert!(!screen.starts_with("\x1b[?1049h"), "{screen:?}");
    assert!(!a.modes().alt_screen);
}

/// node: tests/screen-replay-altscreen.test.ts:127-143
#[test]
fn legacy_1047_is_normalized_to_1049_in_the_prefix() {
    let mut a = actor();
    a.write(b"\x1b[?1047h\x1b[Halt-1047");
    let screen = a.serialize(SerializeOpts::ATTACH);
    assert!(screen.starts_with("\x1b[?1049h"), "{screen:?}");
    let mut b = actor();
    b.write(b"\x1b[?47h\x1b[Halt-47");
    assert!(b.serialize(SerializeOpts::ATTACH).starts_with("\x1b[?1049h"));
}

/// node: src/server.ts:1065-1072 — PEEK never carries Node's alt-screen
/// prefix (the serializer body may still start with `?1049h` on its own, as
/// xterm's addon does; tests/screen-replay-altscreen.test.ts:119-125).
#[test]
fn peek_never_prefixes_1049h() {
    let mut a = actor();
    a.write(b"\x1b[?1049h\x1b[Halt-marker");
    for opts in [SerializeOpts::PEEK, SerializeOpts::PEEK_FULL] {
        let screen = a.serialize(opts);
        let normal = a.normal_replay().unwrap_or("");
        let body = pty_terminal::serialize::vt(a.terminal(), opts.scrollback, a.cell_size());
        // The normal screen comes first and the alternate one after it, but
        // no mode prefix is added on top.
        assert_eq!(screen, format!("{normal}{body}"), "a prefix was added");
        assert!(screen.contains("alt-marker"));
    }
    assert_eq!(pty_terminal::serialize::mode_prefix(&a.modes(), false), "");
}

/// A replay taken while the child is on the alternate screen carries the
/// NORMAL screen too, ahead of the switch. Node gets this from xterm's
/// serialize addon, which walks both buffers.
///
/// Measured against the Node binary 0.12.0 on 2026-09-02: its ATTACH replay
/// for a session that printed lines and then entered the alternate screen
/// reads `ESC[?1049h`, the normal lines, `ESC[?1049h`, then the alternate
/// lines. Without the normal half a client that reconnects has a blank
/// normal screen the moment the full-screen program exits.
#[test]
fn a_replay_from_the_alt_screen_carries_the_normal_screen_first() {
    let mut a = actor();
    a.write(b"normal-line\r\n");
    a.write(b"\x1b[?1049h\x1b[Halt-marker");

    let screen = a.serialize(SerializeOpts::ATTACH);
    assert!(screen.contains("normal-line"), "{screen:?}");
    assert!(screen.contains("alt-marker"), "{screen:?}");
    // The last switch into the alternate screen separates the two halves.
    let switch = screen.rfind("\x1b[?1049h").expect("alt-screen switch");
    assert!(
        !screen[switch..].contains("normal-line"),
        "the normal screen was replayed after the switch: {screen:?}"
    );
    assert!(screen[switch..].contains("alt-marker"), "{screen:?}");

    // Leaving the alternate screen makes the live normal screen the answer
    // again, so the copy is dropped.
    a.write(b"\x1b[?1049l");
    assert_eq!(a.normal_replay(), None);
}

// ── modes that must reach a late attacher (tests/integration.test.ts:1617-1671) ──

/// node: tests/integration.test.ts:1642-1671
#[test]
fn kitty_sgr_mouse_and_hidden_cursor_reach_a_late_attacher() {
    for (enable, name) in [
        (&b"\x1b[>1u"[..], "kitty keyboard"),
        (b"\x1b[?1006h", "SGR mouse"),
        (b"\x1b[?25l", "cursor hidden"),
    ] {
        let mut a = actor();
        a.write(enable);
        a.write(b"ready\r\n");
        let screen = a.serialize(SerializeOpts::ATTACH);
        let enable = std::str::from_utf8(enable).unwrap();
        assert!(screen.contains(enable), "{name}: {screen:?}");
        assert!(screen.contains("ready"));
    }
}

/// node: tests/integration.test.ts:1617-1620 — modes xterm's serializer
/// already carries: bracketed paste, mouse tracking, focus reporting,
/// application cursor keys, alternate screen.
#[test]
fn serializer_carries_the_modes_xterm_serializes() {
    let mut a = actor();
    a.write(b"\x1b[?2004h\x1b[?1000h\x1b[?1004h\x1b[?1h\x1b[?1049h\x1b[Hx");
    let screen = a.serialize(SerializeOpts::ATTACH);
    for m in ["\x1b[?2004h", "\x1b[?1000h", "\x1b[?1004h", "\x1b[?1h", "\x1b[?1049h"] {
        assert!(screen.contains(m), "{m:?} missing from {screen:?}");
    }
}

/// node: tests/integration.test.ts:2240-2259 (`?1006h` → sgrMouse, `?25l` →
/// cursorHidden) and src/server.ts:343-376.
#[test]
fn mode_flags_follow_the_childs_sequences() {
    let mut a = actor();
    assert_eq!(a.modes(), Modes::default());
    a.write(b"\x1b[?1006h\x1b[?25l\x1b[?1002h\x1b[?2004h");
    let m = a.modes();
    assert!(m.sgr_mouse && m.cursor_hidden && m.mouse_1002 && m.bracketed_paste);
    assert!(m.mouse_tracking());
    a.write(b"\x1b[?1006l\x1b[?25h\x1b[?1002l\x1b[?1000;1003h");
    let m = a.modes();
    assert!(!m.sgr_mouse && !m.cursor_hidden && !m.mouse_1002);
    assert!(m.mouse_1000 && m.mouse_1003);
    assert_eq!(a.take_events(), vec![TerminalEvent::CursorVisible]);
    assert_eq!(
        a.serialize(SerializeOpts::PEEK).find("\x1b[?1000h\x1b[?1003h"),
        Some(0),
        "prefix leads with the tracked mouse modes"
    );
}

/// node: tests/ratatui-compat.test.ts:452-700 (kitty stack push/pop, empty
/// pop, combination with `?1006h` and `?25l`, mouse 1000/1002/1003).
#[test]
fn kitty_stack_and_mouse_modes_in_the_prefix() {
    let mut a = actor();
    a.write(b"\x1b[>7u\x1b[>3u\x1b[<u");
    assert_eq!(a.modes().kitty_stack, vec![7]);
    let screen = a.serialize(SerializeOpts::ATTACH);
    assert!(screen.contains("\x1b[>7u"));
    assert!(!screen.contains("\x1b[>3u"));

    let mut b = actor();
    b.write(b"\x1b[<u");
    b.write(b"KITTY-EMPTY-POP-OK\r\n");
    assert!(b.modes().kitty_stack.is_empty());
    let screen = b.serialize(SerializeOpts::ATTACH);
    assert!(!screen.contains("\x1b[>"), "{screen:?}");

    let mut c = actor();
    c.write(b"\x1b[?1006h\x1b[?25l\x1b[>7u\x1b[?1002h");
    let screen = c.serialize(SerializeOpts::ATTACH);
    assert!(screen.starts_with("\x1b[?1002h\x1b[?1006h\x1b[?25l\x1b[>7u"), "{screen:?}");

    let mut d = actor();
    d.write(b"\x1b[?1003h\x1b[?1003l");
    assert!(!d.serialize(SerializeOpts::ATTACH).contains("\x1b[?1003h"));
}

// ── plain-text semantics (src/server.ts:1269-1293) ──

#[test]
fn plain_viewport_is_the_active_area_and_full_is_everything() {
    let mut a = TerminalActor::new(4, 10, 100);
    for i in 0..10 {
        a.write(format!("line{i}\r\n").as_bytes());
    }
    assert_eq!(a.plain(Range::Viewport), "line7\nline8\nline9");
    assert_eq!(
        a.plain(Range::Full),
        (0..10).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n")
    );
    assert_eq!(a.base_y(), 7);
    assert_eq!(a.buffer_length(), 11);
    assert_eq!(a.scrollback_used(), 11);
    assert_eq!(a.scrollback_capacity(), 104);
}

#[test]
fn plain_keeps_written_spaces_drops_erased_and_never_written_cells() {
    let mut a = TerminalActor::new(6, 20, 0);
    a.write(b"AB   \r\n\x1b[44m\x1b[K\r\nCD\x1b[41m   \x1b[0m\r\n                    \r\n\x1b[42m                    \x1b[0m");
    assert_eq!(
        a.plain(Range::Viewport),
        "AB   \n\nCD   \n                    \n                    "
    );
    let mut b = TerminalActor::new(4, 10, 0);
    b.write(b"aaaaaaaaaaaaaaaaaaaaaaaaa\r\nshort");
    assert_eq!(b.plain(Range::Viewport), "aaaaaaaaaa\naaaaaaaaaa\naaaaa\nshort");
    let s = b.snapshot(0);
    assert_eq!(s.wrapped, vec![false, true, true, false]);
}

#[test]
fn reset_clears_screen_modes_and_partial_sequences() {
    let mut a = actor();
    a.write(b"\x1b[?25l\x1b[>7u\x1b]0;t\x07hello\x1b[");
    a.reset();
    assert_eq!(a.modes(), Modes::default());
    assert_eq!(a.plain(Range::Full), "");
    assert_eq!(a.title(), "");
    let data = a.write(b"c");
    assert_eq!(data, b"c", "the pending ESC [ is forgotten");
    assert_eq!(a.take_pty_replies(), b"");
}

#[test]
fn child_ris_clears_the_modes_replayed_to_late_clients() {
    let mut a = actor();
    a.write(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b[?25l\x1b[>1uFULL");
    assert!(a.modes().alt_screen);
    assert!(a.normal_replay().is_some());
    a.write(b"\x1b");
    a.write(b"cafter-reset");
    assert_eq!(a.modes(), Modes::default());
    assert!(a.normal_replay().is_none());
    assert_eq!(a.plain(Range::Full), "after-reset");
    let replay = a.serialize(SerializeOpts::ATTACH);
    assert!(!replay.contains("\x1b[?1000h"), "{replay:?}");
    assert!(!replay.contains("\x1b[>1u"), "{replay:?}");
}

// ── events (src/server.ts:409-454) ──

#[test]
fn bell_title_and_notification_events() {
    let mut a = actor();
    a.write(b"\x07\x1b]0;first\x07\x1b]2;first\x07\x1b]0;second\x07");
    assert_eq!(
        a.take_events(),
        vec![
            TerminalEvent::Bell,
            TerminalEvent::TitleChange("first".into()),
            TerminalEvent::TitleChange("second".into()),
        ]
    );
    assert_eq!(a.title(), "second");
    a.write(b"\x1b]9;hello there\x07\x1b]99;i=1;title=T;b=B\x1b\\\x1b]777;notify;Ti;bo;dy\x07\x1b]777;other;x\x07\x1b[?1004h\x1b[?1004h");
    assert_eq!(
        a.take_events(),
        vec![
            TerminalEvent::Notification(Notification {
                title: None,
                body: Some("hello there".into()),
                source: "osc9"
            }),
            TerminalEvent::Notification(Notification {
                title: Some("T".into()),
                body: Some("B".into()),
                source: "osc99"
            }),
            TerminalEvent::Notification(Notification {
                title: Some("Ti".into()),
                body: Some("bo;dy".into()),
                source: "osc777"
            }),
            TerminalEvent::FocusRequest,
            TerminalEvent::FocusRequest,
        ]
    );
}

// ── ratatui-style replay round trip (tests/ratatui-compat.test.ts sections 1-3) ──

/// Serialize `a`, parse the payload in a fresh actor, and compare the picture:
/// text, per-cell colours and attributes, wrapped flags, and the tracked modes.
fn round_trip(a: &TerminalActor) -> TerminalActor {
    let payload = a.serialize(SerializeOpts::ATTACH);
    let mut b = TerminalActor::new(a.rows(), a.cols(), a.scrollback());
    b.write(payload.as_bytes());
    // Cells erased with a background come back as written spaces with that
    // background (docs/decisions/0002-ansi-serialization.md), so the plain
    // text is compared right-trimmed; the cell comparison below is exact.
    let trimmed = |s: String| s.lines().map(str::trim_end).collect::<Vec<_>>().join("\n");
    assert_eq!(trimmed(b.plain(Range::Full)), trimmed(a.plain(Range::Full)), "plain text");
    assert_eq!((b.base_y(), b.buffer_length()), (a.base_y(), a.buffer_length()), "buffer shape");
    // A pending wrap (cursor past the last column) cannot be expressed by
    // CUP; xterm's serializer has the same limit.
    let clamp = |c: (u16, u16, bool)| (c.0.min(a.cols() - 1), c.1, c.2);
    assert_eq!(clamp(b.cursor()), clamp(a.cursor()), "cursor");
    let (sa, sb) = (a.snapshot(0), b.snapshot(0));
    assert_eq!(sb.wrapped, sa.wrapped, "wrapped flags");
    for (y, (ra, rb)) in sa.rows.iter().zip(&sb.rows).enumerate() {
        for (x, (ca, cb)) in ra.iter().zip(rb).enumerate() {
            assert_eq!(cb, ca, "cell ({x},{y}) after replay\npayload={payload:?}");
        }
    }
    assert_eq!(b.modes(), a.modes(), "tracked modes");
    b
}

/// node: tests/ratatui-compat.test.ts:138-177
#[test]
fn full_width_rgb_background_fill_survives_replay() {
    let mut a = actor();
    let line = format!("\x1b[48;2;71;76;86m{}\x1b[0m\r\nBG-FILL-DONE\r\n", " ".repeat(80));
    a.write(line.as_bytes());
    assert!(a.serialize(SerializeOpts::ATTACH).contains("48;2;71;76;86"));
    let b = round_trip(&a);
    let s = b.snapshot(0);
    assert!(s.rows[0].iter().all(|c| c.bg == ColorSnap::Rgb(71, 76, 86)));
    assert!(b.serialize(SerializeOpts::ATTACH).contains("48;2;71;76;86"));
}

/// node: tests/ratatui-compat.test.ts:179-212
#[test]
fn partial_background_fill_survives_replay() {
    let mut a = actor();
    let line = format!("\x1b[48;2;0;100;200m{}\x1b[0m{}\r\nPARTIAL-BG-DONE\r\n", " ".repeat(40), " ".repeat(40));
    a.write(line.as_bytes());
    let b = round_trip(&a);
    let s = b.snapshot(0);
    assert!(s.rows[0][..40].iter().all(|c| c.bg == ColorSnap::Rgb(0, 100, 200)));
    assert!(s.rows[0][40..].iter().all(|c| c.bg == ColorSnap::Default));
}

/// node: tests/ratatui-compat.test.ts:214-256
#[test]
fn text_with_background_survives_replay() {
    let mut a = actor();
    let text = "Hello World";
    let line = format!("\x1b[48;2;30;30;30m\x1b[38;2;255;255;255m{text}{}\x1b[0m\r\nTEXT-BG-DONE\r\n", " ".repeat(80 - text.len()));
    a.write(line.as_bytes());
    let b = round_trip(&a);
    let s = b.snapshot(0);
    assert_eq!(s.rows[0][0].fg, ColorSnap::Rgb(255, 255, 255));
    assert_eq!(s.rows[0][79].bg, ColorSnap::Rgb(30, 30, 30));
    assert!(b.plain(Range::Viewport).contains("Hello World"));
}

/// node: tests/ratatui-compat.test.ts:260-324 — alt screen, every row erased
/// with a background via EL, content on some rows.
#[test]
fn alt_screen_with_per_row_background_erase_survives_replay() {
    let mut a = actor();
    let mut script = String::from("\x1b[?1049h");
    for r in 1..=24 {
        script.push_str(&format!("\x1b[{r};1H\x1b[48;2;71;76;86m\x1b[K"));
    }
    script.push_str("\x1b[1;1H\x1b[48;2;71;76;86m\x1b[1m Title Bar \x1b[22m\x1b[0m");
    script.push_str("\x1b[3;1H\x1b[48;2;71;76;86m Content line here\x1b[0m");
    script.push_str("\x1b[24;1H\x1b[48;2;71;76;86m Status: RATATUI-SCREEN-OK\x1b[0m");
    a.write(script.as_bytes());
    let b = round_trip(&a);
    let s = b.snapshot(0);
    assert!(s.rows[1][0].text == " " && s.rows[1][79].bg == ColorSnap::Rgb(71, 76, 86), "erased-with-bg row keeps its background");
    assert!(s.rows[0][1].bold);
    assert!(b.plain(Range::Viewport).contains("RATATUI-SCREEN-OK"));
    assert!(b.alt_screen_active());
}

/// node: tests/ratatui-compat.test.ts:326-377
#[test]
fn cursor_addressed_multi_color_rows_survive_replay() {
    let mut a = actor();
    let script = format!(
        "\x1b[?1049h\x1b[H\x1b[2J\x1b[1;1H\x1b[48;2;180;0;0m{}\x1b[48;2;0;0;180m{}\x1b[0m\x1b[2;1H\x1b[48;2;30;30;30m\x1b[38;2;0;200;0mMULTI-COLOR-OK{}\x1b[0m",
        " ".repeat(40),
        " ".repeat(40),
        " ".repeat(80 - 14)
    );
    a.write(script.as_bytes());
    let b = round_trip(&a);
    let s = b.snapshot(0);
    assert_eq!(s.rows[0][0].bg, ColorSnap::Rgb(180, 0, 0));
    assert_eq!(s.rows[0][40].bg, ColorSnap::Rgb(0, 0, 180));
    assert_eq!(s.rows[1][0].fg, ColorSnap::Rgb(0, 200, 0));
}

/// node: tests/ratatui-compat.test.ts:379-449 — background-only rows keep
/// their background after the round trip (xterm's known weak spot).
#[test]
fn full_screen_el_background_is_kept_on_every_row() {
    let mut a = TerminalActor::new(10, 40, 0);
    let mut script = String::from("\x1b[?1049h");
    for r in 1..=10 {
        script.push_str(&format!("\x1b[{r};1H\x1b[48;2;128;0;128m\x1b[K"));
    }
    script.push_str("\x1b[1;1H\x1b[48;2;128;0;128m\x1b[38;2;255;255;255mFULL-BG-EL-OK\x1b[0m");
    a.write(script.as_bytes());
    let b = round_trip(&a);
    let s = b.snapshot(0);
    for (y, row) in s.rows.iter().enumerate() {
        assert!(row.iter().all(|c| c.bg == ColorSnap::Rgb(128, 0, 128)), "row {y} lost its background");
    }
}

/// node: tests/ratatui-compat.test.ts:452-490 — kitty push replayed;
/// scrollback content and styles survive too.
#[test]
fn kitty_push_and_scrollback_survive_replay() {
    let mut a = TerminalActor::new(5, 20, 100);
    a.write(b"\x1b[>7u");
    for i in 0..12 {
        a.write(format!("\x1b[3{}mline {i}\x1b[0m\r\n", i % 8).as_bytes());
    }
    let b = round_trip(&a);
    assert_eq!(b.modes().kitty_stack, vec![7]);
    assert_eq!(b.kitty_flags(), 7);
    assert_eq!(b.base_y(), a.base_y());
    let (ha, hb) = (a.snapshot(a.base_y()), b.snapshot(b.base_y()));
    assert_eq!(hb.rows, ha.rows, "oldest history rows");
    assert_eq!(hb.rows[0][0].fg, ColorSnap::Indexed(0));
    assert_eq!(hb.rows[1][0].fg, ColorSnap::Indexed(1));
}
