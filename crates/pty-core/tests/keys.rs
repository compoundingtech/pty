//! Port of the pty project's `tests/keys.test.ts`.

use pty_core::keys::{parse_seq_value, resolve_key};

fn r(spec: &str) -> String {
    resolve_key(spec).expect("resolve_key")
}

#[test]
fn resolves_named_keys() {
    assert_eq!(r("return"), "\r");
    assert_eq!(r("enter"), "\r");
    assert_eq!(r("tab"), "\t");
    assert_eq!(r("escape"), "\x1b");
    assert_eq!(r("esc"), "\x1b");
    assert_eq!(r("space"), " ");
    assert_eq!(r("backspace"), "\x7f");
    assert_eq!(r("delete"), "\x1b[3~");
}

#[test]
fn resolves_function_keys_and_control_slash() {
    for (name, sequence) in [
        ("f1", "\x1bOP"),
        ("f2", "\x1bOQ"),
        ("f3", "\x1bOR"),
        ("f4", "\x1bOS"),
        ("f5", "\x1b[15~"),
        ("f6", "\x1b[17~"),
        ("f7", "\x1b[18~"),
        ("f8", "\x1b[19~"),
        ("f9", "\x1b[20~"),
        ("f10", "\x1b[21~"),
        ("f11", "\x1b[23~"),
        ("f12", "\x1b[24~"),
    ] {
        assert_eq!(r(name), sequence, "{name}");
    }
    assert_eq!(r("ctrl+f1"), "\x1b[1;5P");
    assert_eq!(r("ctrl+/"), "\x1f");
    assert_eq!(r("alt+/"), "\x1b/");
    assert_eq!(r("alt+ctrl+/"), "\x1b\x1f");
    assert!(resolve_key("shift+/").is_err());
    assert!(resolve_key("ctrl+shift+/").is_err());
}

#[test]
fn resolves_arrow_keys() {
    assert_eq!(r("up"), "\x1b[A");
    assert_eq!(r("down"), "\x1b[B");
    assert_eq!(r("right"), "\x1b[C");
    assert_eq!(r("left"), "\x1b[D");
}

#[test]
fn resolves_navigation_keys() {
    assert_eq!(r("home"), "\x1b[H");
    assert_eq!(r("end"), "\x1b[F");
    assert_eq!(r("pageup"), "\x1b[5~");
    assert_eq!(r("pagedown"), "\x1b[6~");
}

#[test]
fn resolves_ctrl_chords() {
    assert_eq!(r("ctrl+c"), "\x03");
    assert_eq!(r("ctrl+a"), "\x01");
    assert_eq!(r("ctrl+z"), "\x1a");
    assert_eq!(r("ctrl+d"), "\x04");
}

#[test]
fn resolves_alt_chords() {
    assert_eq!(r("alt+x"), "\x1bx");
    assert_eq!(r("alt+a"), "\x1ba");
}

#[test]
fn resolves_shift_chords_for_letters() {
    assert_eq!(r("shift+a"), "A");
    assert_eq!(r("shift+z"), "Z");
}

#[test]
fn resolves_shift_return_via_csi_u() {
    assert_eq!(r("shift+return"), "\x1b[13;2u");
    assert_eq!(r("shift+enter"), "\x1b[13;2u");
}

#[test]
fn resolves_shift_tab_as_legacy_backtab() {
    assert_eq!(r("shift+tab"), "\x1b[Z");
}

#[test]
fn resolves_shift_escape_space_backspace_via_csi_u() {
    assert_eq!(r("shift+escape"), "\x1b[27;2u");
    assert_eq!(r("shift+space"), "\x1b[32;2u");
    assert_eq!(r("shift+backspace"), "\x1b[127;2u");
}

#[test]
fn resolves_shift_arrow_keys() {
    assert_eq!(r("shift+up"), "\x1b[1;2A");
    assert_eq!(r("shift+down"), "\x1b[1;2B");
    assert_eq!(r("shift+right"), "\x1b[1;2C");
    assert_eq!(r("shift+left"), "\x1b[1;2D");
}

#[test]
fn resolves_shift_navigation_keys() {
    assert_eq!(r("shift+home"), "\x1b[1;2H");
    assert_eq!(r("shift+end"), "\x1b[1;2F");
    assert_eq!(r("shift+pageup"), "\x1b[5;2~");
    assert_eq!(r("shift+pagedown"), "\x1b[6;2~");
    assert_eq!(r("shift+delete"), "\x1b[3;2~");
}

#[test]
fn resolves_ctrl_shift_combinations() {
    assert_eq!(r("ctrl+shift+up"), "\x1b[1;6A");
    assert_eq!(r("ctrl+shift+return"), "\x1b[13;6u");
}

#[test]
fn resolves_alt_shift_combinations() {
    assert_eq!(r("alt+shift+up"), "\x1b[1;4A");
    assert_eq!(r("alt+shift+return"), "\x1b[13;4u");
}

#[test]
fn resolves_ctrl_alt_on_named_keys() {
    assert_eq!(r("ctrl+alt+up"), "\x1b[1;7A");
    assert_eq!(r("ctrl+alt+delete"), "\x1b[3;7~");
}

#[test]
fn resolves_all_three_modifiers_combined() {
    assert_eq!(r("ctrl+alt+shift+up"), "\x1b[1;8A");
    assert_eq!(r("ctrl+alt+shift+return"), "\x1b[13;8u");
}

#[test]
fn resolves_composed_modifiers() {
    assert_eq!(r("ctrl+alt+c"), "\x1b\x03");
    assert_eq!(r("alt+ctrl+c"), "\x1b\x03");
}

#[test]
fn is_case_insensitive() {
    assert_eq!(r("Ctrl+C"), "\x03");
    assert_eq!(r("RETURN"), "\r");
    assert_eq!(r("Alt+X"), "\x1bx");
}

#[test]
fn throws_on_unknown_key() {
    assert!(resolve_key("f99").unwrap_err().0.contains("Unknown key"));
    assert!(resolve_key("nonexistent")
        .unwrap_err()
        .0
        .contains("Unknown key"));
}

#[test]
fn throws_on_unknown_modifier() {
    assert!(resolve_key("super+c")
        .unwrap_err()
        .0
        .contains("Unknown modifier"));
    assert!(resolve_key("meta+x")
        .unwrap_err()
        .0
        .contains("Unknown modifier"));
}

#[test]
fn parse_seq_value_resolves_key_prefixed() {
    assert_eq!(parse_seq_value("key:return").unwrap(), "\r");
    assert_eq!(parse_seq_value("key:ctrl+c").unwrap(), "\x03");
    assert_eq!(parse_seq_value("key:tab").unwrap(), "\t");
}

#[test]
fn parse_seq_value_passes_through_literals() {
    assert_eq!(parse_seq_value("hello").unwrap(), "hello");
    assert_eq!(parse_seq_value("git status").unwrap(), "git status");
    assert_eq!(parse_seq_value("").unwrap(), "");
}
