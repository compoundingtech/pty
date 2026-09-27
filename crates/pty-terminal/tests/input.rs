//! The child input encoder: the bytes a key, a mouse event, a focus change,
//! or a paste turns into depend on modes the *child* set, so every case here
//! sets that mode the way a child would and then encodes.

use pty_terminal::input::{self, Key, KeyAction, KeyEvent, Mods, MouseAction, MouseButton, MouseEvent};
use pty_terminal::TerminalActor;

fn actor() -> TerminalActor {
    TerminalActor::new(10, 20, 0)
}

// ── keys ──

#[test]
fn decckm_decides_whether_an_arrow_is_csi_or_ss3() {
    let mut a = actor();
    assert!(!a.modes().app_cursor);
    assert_eq!(a.encode_key(&KeyEvent::press(Key::ArrowUp)), b"\x1b[A");

    a.write(b"\x1b[?1h");
    assert!(a.modes().app_cursor, "?1 is tracked");
    assert_eq!(
        a.encode_key(&KeyEvent::press(Key::ArrowUp)),
        b"\x1bOA",
        "application cursor keys"
    );

    a.write(b"\x1b[?1l");
    assert!(!a.modes().app_cursor);
    assert_eq!(a.encode_key(&KeyEvent::press(Key::ArrowUp)), b"\x1b[A");
}

#[test]
fn a_plain_character_is_itself() {
    let a = actor();
    assert_eq!(a.encode_key(&KeyEvent::typed(Key::A, "a", Some('a'))), b"a");
    assert_eq!(
        a.encode_key(&KeyEvent::press(Key::C).with_mods(Mods::CTRL)),
        b"\x03",
        "ctrl+c is a control byte until the child asks for more"
    );
}

/// The kitty keyboard protocol's associated text: the child asked for it
/// (`CSI > 17 u` = disambiguate + report associated), so the shifted text has
/// to reach it alongside the base key. A `KeyEvent` that folded shift into one
/// character could not express this.
#[test]
fn kitty_associated_text_carries_the_shifted_character() {
    let mut a = actor();
    a.write(b"\x1b[>17u");
    assert_eq!(a.kitty_flags(), 17);
    assert_eq!(a.modes().kitty_stack, vec![17]);

    let shift_a = KeyEvent::typed(Key::A, "A", Some('a')).with_mods(Mods::SHIFT);
    assert_eq!(
        a.encode_key(&shift_a),
        b"\x1b[97;2;65u",
        "base key 97 ('a'), shift modifier, associated text 65 ('A')"
    );
    assert_eq!(
        a.encode_key(&KeyEvent::typed(Key::A, "a", Some('a'))),
        b"a",
        "an unmodified key still goes through as text"
    );
}

/// The alternate-key form (`CSI > 5 u` = disambiguate + report alternates):
/// the shifted key travels in the key field itself, `base:shifted`.
#[test]
fn kitty_alternates_carry_the_shifted_key() {
    let mut a = actor();
    a.write(b"\x1b[>5u");
    let shift_a = KeyEvent::typed(Key::A, "A", Some('a')).with_mods(Mods::SHIFT);
    let out = String::from_utf8(a.encode_key(&shift_a)).expect("utf8");
    assert!(
        out.contains("97:65"),
        "expected base:shifted in {out:?} (flags {})",
        a.kitty_flags()
    );
}

#[test]
fn a_release_reaches_the_child_only_when_it_asked_for_events() {
    let mut a = actor();
    let release = KeyEvent {
        action: KeyAction::Release,
        ..KeyEvent::typed(Key::A, "a", Some('a'))
    };
    assert!(
        a.encode_key(&release).is_empty(),
        "no release reporting by default"
    );

    // `CSI > 3 u` = disambiguate + report events.
    a.write(b"\x1b[>3u");
    assert_eq!(
        a.encode_key(&release),
        b"\x1b[97;1:3u",
        "key 97, no modifiers, event type 3 = release"
    );

    // A character key with neither text nor an unshifted codepoint has no
    // identity the protocol can report, so it encodes to nothing. A
    // consumer that sees an empty encoding for a character key is missing
    // `KeyEvent::unshifted`.
    let anonymous = KeyEvent {
        action: KeyAction::Release,
        ..KeyEvent::press(Key::A)
    };
    assert!(a.encode_key(&anonymous).is_empty());
}

// ── mouse ──

fn press() -> MouseEvent {
    MouseEvent::press(MouseButton::Left, 3, 4)
}

#[test]
fn no_tracking_reports_nothing_so_the_surface_keeps_the_event() {
    let a = actor();
    assert_eq!(a.encode_mouse(&press()), None);
    assert_eq!(a.encode_mouse(&MouseEvent::wheel(true, 3, 4)), None);
    assert!(!a.modes().mouse_reporting());
}

/// X10 (`?9`) reports button presses and nothing else. A consumer that folded
/// it into "mouse tracking is on" would forward the wheel to a child that
/// never hears it — and lose its own scrolling.
#[test]
fn x10_reports_presses_but_never_the_wheel() {
    let mut a = actor();
    a.write(b"\x1b[?9h");
    assert!(a.modes().mouse_9);
    assert!(!a.modes().mouse_tracking(), "?9 is not wheel-reporting");
    assert!(a.modes().mouse_reporting());

    assert_eq!(a.encode_mouse(&press()), Some(b"\x1b[M \x24\x25".to_vec()));
    assert_eq!(
        a.encode_mouse(&MouseEvent::wheel(true, 3, 4)),
        None,
        "the surface keeps the wheel"
    );
    assert_eq!(
        a.encode_mouse(&MouseEvent {
            action: MouseAction::Release,
            ..press()
        }),
        None,
        "X10 has no release report"
    );
}

#[test]
fn normal_tracking_reports_the_wheel() {
    let mut a = actor();
    a.write(b"\x1b[?1000h");
    assert!(a.modes().mouse_tracking());
    assert!(a.encode_mouse(&press()).is_some());
    assert_eq!(
        a.encode_mouse(&MouseEvent::wheel(true, 3, 4)),
        Some(b"\x1b[M`\x24\x25".to_vec()),
        "button 64 = wheel up"
    );
}

#[test]
fn sgr_mode_reports_the_cell_the_event_names() {
    let mut a = actor();
    a.write(b"\x1b[?1000h\x1b[?1006h");
    assert_eq!(
        a.encode_mouse(&press()),
        Some(b"\x1b[<0;4;5M".to_vec()),
        "col 3 / row 4, 1-based in the report"
    );
}

// ── focus and paste ──

#[test]
fn focus_is_reported_only_under_1004() {
    let mut a = actor();
    assert_eq!(a.encode_focus(true), None);
    a.write(b"\x1b[?1004h");
    assert_eq!(a.encode_focus(true), Some(b"\x1b[I".to_vec()));
    assert_eq!(a.encode_focus(false), Some(b"\x1b[O".to_vec()));
    a.write(b"\x1b[?1004l");
    assert_eq!(a.encode_focus(false), None);
}

#[test]
fn paste_is_bracketed_only_under_2004() {
    let mut a = actor();
    assert_eq!(
        a.encode_paste("a\nb"),
        b"a\rb",
        "without bracketing a newline becomes a carriage return"
    );
    a.write(b"\x1b[?2004h");
    assert_eq!(a.encode_paste("a\nb"), b"\x1b[200~a\nb\x1b[201~");
}

#[test]
fn a_multi_line_paste_is_flagged_unsafe() {
    assert!(input::paste_is_safe("just text"));
    assert!(!input::paste_is_safe("rm -rf /\n"));
    assert!(
        !input::paste_is_safe("a\x1b[201~b"),
        "a forged bracketed-paste end escapes the brackets"
    );
}
