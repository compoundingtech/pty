use pty_client::tty::{DETACH_KEY, normalize_detach_key};

#[test]
fn ctrl_backslash_with_lock_modifiers_still_detaches() {
    for sequence in ["\x1b[92;5u", "\x1b[92;69u", "\x1b[92;133u", "\x1b[92;197u"] {
        assert_eq!(normalize_detach_key(sequence.as_bytes()), [DETACH_KEY], "{sequence:?}");
    }
    assert_eq!(normalize_detach_key(b"\x1b[92;6u"), b"\x1b[92;6u");
}

/// What iTerm2 sends for Ctrl+\ in each keyboard mode a program can request,
/// from its key mappers (sources/Keyboard/ in gnachman/iTerm2). Each form
/// must detach. Only modifyOtherKeys 2 is not the legacy byte or the kitty
/// form. iTerm2 sends it for Ctrl plus any key, Caps Lock or not, and
/// `CSI > 4 ; 2 m` also clears iTerm2's kitty flags.
#[test]
fn every_iterm2_encoding_of_ctrl_backslash_detaches() {
    let modes: [(&str, &[u8]); 6] = [
        ("legacy", b"\x1c"),
        ("profile: report keys using CSI u", b"\x1b[92;5u"),
        ("modifyOtherKeys 1", b"\x1c"),
        ("modifyOtherKeys 2", b"\x1b[27;5;92~"),
        ("kitty flags", b"\x1b[92;5u"),
        ("kitty flags, Caps Lock", b"\x1b[92;69u"),
    ];
    for (mode, press) in modes {
        assert_eq!(normalize_detach_key(press), [DETACH_KEY], "{mode}");
    }
}

/// The key arrives in the same read as other input, and only the key changes.
#[test]
fn the_modify_other_keys_form_is_replaced_in_place() {
    assert_eq!(
        normalize_detach_key(b"a\x1b[27;5;92~b"),
        [b'a', DETACH_KEY, b'b']
    );
    // Ctrl+Shift+\ is a different key; it reaches the program untouched.
    assert_eq!(normalize_detach_key(b"\x1b[27;6;92~"), b"\x1b[27;6;92~");
}
