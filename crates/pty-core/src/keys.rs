//! Named-key resolution, ported from the pty project's `src/keys.ts`.
//!
//! Turns key specs like `ctrl+c`, `return`, `alt+x`, `shift+tab` into the byte
//! sequence a real terminal would send to the PTY. Modifier combinations use
//! the xterm modifier-parameter encoding; control chars use CSI-u.

use std::collections::HashSet;

/// Error returned when a key spec references an unknown key or modifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyError(pub String);

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for KeyError {}

/// Base named keys → their unmodified byte sequence.
fn key_map(name: &str) -> Option<&'static str> {
    Some(match name {
        "return" | "enter" => "\r",
        "tab" => "\t",
        "escape" | "esc" => "\x1b",
        "space" => " ",
        "backspace" => "\x7f",
        "delete" => "\x1b[3~",
        "up" => "\x1b[A",
        "down" => "\x1b[B",
        "right" => "\x1b[C",
        "left" => "\x1b[D",
        "home" => "\x1b[H",
        "end" => "\x1b[F",
        "pageup" => "\x1b[5~",
        "pagedown" => "\x1b[6~",
        "f1" => "\x1bOP",
        "f2" => "\x1bOQ",
        "f3" => "\x1bOR",
        "f4" => "\x1bOS",
        "f5" => "\x1b[15~",
        "f6" => "\x1b[17~",
        "f7" => "\x1b[18~",
        "f8" => "\x1b[19~",
        "f9" => "\x1b[20~",
        "f10" => "\x1b[21~",
        "f11" => "\x1b[23~",
        "f12" => "\x1b[24~",
        "/" => "/",
        _ => return None,
    })
}

/// CSI-u keycodes for control-char keys under modifiers.
fn csi_u_keycode(name: &str) -> Option<u32> {
    Some(match name {
        "return" | "enter" => 13,
        "tab" => 9,
        "escape" | "esc" => 27,
        "space" => 32,
        "backspace" => 127,
        _ => return None,
    })
}

fn is_modifier(m: &str) -> bool {
    matches!(m, "ctrl" | "alt" | "shift")
}

/// The named keys, sorted, as the help text lists them.
fn named_keys() -> String {
    let mut names = [
        "return", "enter", "tab", "escape", "esc", "space", "backspace", "delete", "up", "down",
        "right", "left", "home", "end", "pageup", "pagedown", "f1", "f2", "f3", "f4",
        "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12", "/",
    ];
    names.sort_unstable();
    names.join(", ")
}

/// The sentence every key-spec error ends with.
///
/// node: src/keys.ts:22-25 (`KEY_SPEC_HELP`)
fn key_spec_help() -> String {
    format!(
        "Use ctrl+u, ctrl-u, ctrl_u, or C-u; supported modifiers are ctrl, alt, and shift; \
         supported keys are a-z, {}.",
        named_keys()
    )
}

/// `+`, `_` and `-` all separate a modifier from what follows it.
///
/// node: src/keys.ts:21 (`MODIFIER_SEPARATORS`)
fn is_separator(c: char) -> bool {
    c == '+' || c == '_' || c == '-'
}

/// `C-u` is the readline and tmux spelling of `ctrl+u`. The one-letter alias
/// is scoped to a leading `C-`, so `C+u` keeps no surprise meaning.
///
/// node: src/keys.ts:47-53 (`normalizeModifier`)
fn normalize_modifier(m: &str, index: usize, spec: &str) -> String {
    let leading_c_dash = {
        let mut chars = spec.chars();
        matches!(chars.next(), Some(c) if c.eq_ignore_ascii_case(&'c'))
            && matches!(chars.next(), Some('-'))
    };
    if m == "c" && index == 0 && leading_c_dash {
        return "ctrl".to_string();
    }
    m.to_string()
}

fn is_supported_base(base: &str) -> bool {
    key_map(base).is_some() || (base.len() == 1 && base.as_bytes()[0].is_ascii_lowercase())
}

/// xterm modifier parameter: `1 + bitmask(shift=1, alt=2, ctrl=4)`.
fn modifier_param(mods: &HashSet<String>) -> u32 {
    1 + if mods.contains("shift") { 1 } else { 0 }
        + if mods.contains("alt") { 2 } else { 0 }
        + if mods.contains("ctrl") { 4 } else { 0 }
}

/// Parse a key spec like `ctrl+c`, `return`, `alt+x` into the bytes a terminal
/// would send. Returns `Err` for unknown modifiers or keys.
pub fn resolve_key(spec: &str) -> Result<String, KeyError> {
    let lower = spec.to_lowercase();
    let has_separator = lower.chars().any(is_separator);
    let raw_parts: Vec<&str> = if has_separator {
        lower.split(is_separator).collect()
    } else {
        vec![lower.as_str()]
    };
    let raw_base = *raw_parts.last().unwrap_or(&"");
    let raw_mods: Vec<String> = raw_parts[..raw_parts.len().saturating_sub(1)]
        .iter()
        .enumerate()
        .map(|(i, m)| normalize_modifier(m, i, spec))
        .collect();

    // A separator-bearing name could be both a named key and a modifier
    // chord. Refuse the collision rather than silently pick one.
    //
    // node: src/keys.ts:73-82
    let is_valid_chord = !raw_base.is_empty()
        && !raw_mods.is_empty()
        && raw_mods.iter().all(|m| !m.is_empty() && is_modifier(m))
        && is_supported_base(raw_base);
    if has_separator && key_map(&lower).is_some() && is_valid_chord {
        return Err(KeyError(format!(
            "Ambiguous key spec \"{spec}\": it is both a named key and a modifier chord. {}",
            key_spec_help()
        )));
    }
    if let Some(mapped) = key_map(&lower)
        && !is_valid_chord
    {
        return Ok(mapped.to_string());
    }

    let mut parts = raw_parts;
    let base = parts.pop().unwrap_or("").to_string();
    if base.is_empty() || parts.iter().any(|p| p.is_empty()) {
        return Err(KeyError(format!(
            "Incomplete key spec \"{spec}\". {}",
            key_spec_help()
        )));
    }
    let mods: HashSet<String> = parts
        .iter()
        .enumerate()
        .map(|(i, m)| normalize_modifier(m, i, spec))
        .collect();

    for m in &mods {
        if !is_modifier(m) {
            return Err(KeyError(format!(
                "Unknown modifier: \"{m}\" in key spec \"{spec}\". {}",
                key_spec_help()
            )));
        }
    }

    let is_letter = base.len() == 1 && base.as_bytes()[0].is_ascii_lowercase();
    let has_modifiers = !mods.is_empty();
    let mapped = key_map(&base);

    if mapped.is_none() && !is_letter {
        return Err(KeyError(format!(
            "Unknown key: \"{base}\" in key spec \"{spec}\". {}",
            key_spec_help()
        )));
    }

    // Single-letter keys.
    if is_letter {
        let mut result = base.clone();
        if mods.contains("shift") {
            result = result.to_uppercase();
        }
        if mods.contains("ctrl") {
            let code = result.to_lowercase().as_bytes()[0] as u32;
            result = char::from_u32(code - 96).unwrap().to_string();
        }
        if mods.contains("alt") {
            result = format!("\x1b{result}");
        }
        return Ok(result);
    }

    let mapped = mapped.unwrap();

    // The legacy control spelling for slash is the unit separator byte.
    if base == "/" && mods.contains("ctrl") {
        return Ok(if mods.contains("alt") {
            "\x1b\x1f".to_string()
        } else {
            "\x1f".to_string()
        });
    }

    // Named keys without modifiers: return the mapped value directly.
    if !has_modifiers {
        return Ok(mapped.to_string());
    }

    let modp = modifier_param(&mods);

    // Special case: shift+tab produces the legacy backtab sequence.
    if base == "tab" && modp == 2 {
        return Ok("\x1b[Z".to_string());
    }

    // CSI sequences of form ESC [ N ~  (e.g. delete, pageup).
    if let Some(digits) = mapped
        .strip_prefix("\x1b[")
        .and_then(|s| s.strip_suffix('~'))
        && !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            return Ok(format!("\x1b[{digits};{modp}~"));
        }

    // CSI sequences of form ESC [ X  (single uppercase letter: arrows, home, end).
    if let Some(letter) = mapped.strip_prefix("\x1b[")
        && letter.len() == 1 && letter.as_bytes()[0].is_ascii_uppercase() {
        return Ok(format!("\x1b[1;{modp}{letter}"));
    }

    // F1–F4 use SS3 without modifiers and CSI with modifiers.
    if let Some(letter) = mapped.strip_prefix("\x1bO")
        && letter.len() == 1 && letter.as_bytes()[0].is_ascii_uppercase() {
            return Ok(format!("\x1b[1;{modp}{letter}"));
        }

    // Control-char keys (return, tab, escape, space, backspace): CSI-u encoding.
    if let Some(keycode) = csi_u_keycode(&base) {
        return Ok(format!("\x1b[{keycode};{modp}u"));
    }

    Ok(mapped.to_string())
}

/// If `value` starts with `key:`, resolve the key name; otherwise return the
/// literal string.
pub fn parse_seq_value(value: &str) -> Result<String, KeyError> {
    if let Some(rest) = value.strip_prefix("key:") {
        resolve_key(rest)
    } else {
        Ok(value.to_string())
    }
}
