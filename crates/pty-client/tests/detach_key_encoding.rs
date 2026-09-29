use pty_client::tty::{DETACH_KEY, normalize_detach_key};

#[test]
fn ctrl_backslash_with_lock_modifiers_still_detaches() {
    for sequence in ["\x1b[92;5u", "\x1b[92;69u", "\x1b[92;133u", "\x1b[92;197u"] {
        assert_eq!(normalize_detach_key(sequence.as_bytes()), [DETACH_KEY], "{sequence:?}");
    }
    assert_eq!(normalize_detach_key(b"\x1b[92;6u"), b"\x1b[92;6u");
}
