//! Session ids and display names: validation texts, random ids, the automatic
//! display name, and the two small presentation helpers `pty list` uses.
//!
//! node: src/sessions.ts:30-80; src/cli.ts:642-668, 971-973, 4114-4130

use std::path::Path;

use super::atomic::random_bytes;
use super::root::{SUN_PATH_MAX, socket_path};

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// Validate a stable session id. Errors carry Node's texts, checked in
/// Node's order.
///
/// node: src/sessions.ts:35-64
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Session name cannot be empty.".to_string());
    }
    if name == "." || name == ".." {
        return Err(format!(
            "Invalid session name \"{name}\". Names cannot be \".\" or \"..\"."
        ));
    }
    if name.encode_utf16().count() > 255 {
        return Err("Session name too long (max 255 characters).".to_string());
    }
    if !name.chars().all(is_name_char) {
        return Err(format!(
            "Invalid session name \"{name}\". Names may only contain letters, numbers, dots, hyphens, and underscores."
        ));
    }
    // NAME_MAX is 255 on supported filesystems. Reserve 14 bytes for
    // .revision.json and 32 for .tmp.<10-digit-pid>.<16-hex> publication.
    if name.len() > 209 {
        return Err("Session name too long for registry sidecars (max 209 characters).".to_string());
    }
    let byte_len = socket_path(name).as_os_str().len();
    if byte_len > SUN_PATH_MAX {
        let overflow = byte_len - SUN_PATH_MAX;
        return Err(format!(
            "Session name \"{name}\" produces a socket path of {byte_len} bytes, which exceeds the {SUN_PATH_MAX}-byte kernel limit by {overflow}. Shorten the name or set PTY_SESSION_DIR to a shorter path."
        ));
    }
    Ok(())
}

/// Validate a mutable display name: non-empty, trimmed, at most 160 Unicode
/// scalars, no control characters (`\p{Cc}`) and no U+2028/U+2029.
///
/// node: src/sessions.ts:67-80
pub fn validate_display_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Display name cannot be empty.".to_string());
    }
    if name != js_trim(name) {
        return Err("Display name must be trimmed.".to_string());
    }
    if name.chars().count() > 160 {
        return Err("Display name too long (max 160 Unicode scalars).".to_string());
    }
    if name
        .chars()
        .any(|c| c.is_control() || c == '\u{2028}' || c == '\u{2029}')
    {
        return Err(
            "Display name must be single-line and contain no control characters.".to_string(),
        );
    }
    Ok(())
}

/// JavaScript's `String.prototype.trim` character set (WhiteSpace and
/// LineTerminator), which is not quite Rust's `char::is_whitespace`.
pub(crate) fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// `s.trim()` with JavaScript's whitespace set.
pub(crate) fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

/// The alphabet random ids draw from: no `0 1 o i l`.
pub const SESSION_ID_ALPHABET: &[u8] = b"23456789abcdefghjkmnpqrstuvwxyz";

/// The number of collision-avoidance attempts `pty run` makes.
pub const SESSION_ID_ATTEMPTS: usize = 8;

/// An 8-character random session id: each character is `byte % 31` into
/// [`SESSION_ID_ALPHABET`], exactly as Node draws it.
///
/// node: src/cli.ts:642-648
pub fn random_session_name() -> String {
    random_bytes(8)
        .into_iter()
        .map(|b| SESSION_ID_ALPHABET[(b as usize) % SESSION_ID_ALPHABET.len()] as char)
        .collect()
}

/// Alias kept for existing callers: a fresh random session id.
pub fn generate_id() -> String {
    random_session_name()
}

/// The error `pty run` prints when eight random ids all collide.
pub fn unique_id_failure_message() -> String {
    format!("Could not generate a unique session id after {SESSION_ID_ATTEMPTS} attempts.")
}

/// `path.basename`: the last non-empty component, `""` for `/` or `""`.
fn basename(p: &str) -> &str {
    let trimmed = p.trim_end_matches('/');
    if trimmed.is_empty() {
        return "";
    }
    trimmed.rsplit('/').next().unwrap_or("")
}

/// The automatic display name for `pty run` without `--name`:
/// `<basename(cwd)>-<basename(cmd)>[-<first arg base>]`, then sanitized to
/// `[a-zA-Z0-9._-]` with dash runs collapsed and edge dashes stripped.
///
/// node: src/cli.ts:651-668, 971-973
pub fn auto_display_name(cwd: &Path, cmd: &str, args: &[String]) -> String {
    let dir_part = basename(&cwd.to_string_lossy()).to_string();
    let cmd_base = basename(cmd).to_string();
    let first_arg = args
        .iter()
        .find(|a| !a.starts_with('-') && a.encode_utf16().count() < 30);
    let mut cmd_part = cmd_base.clone();
    if let Some(first_arg) = first_arg {
        let base = basename(first_arg);
        // `.replace(/\.[^.]+$/, "")`: drop a final `.ext` with at least one
        // character after the dot (`.bashrc` -> ``, `file.` -> `file.`).
        let arg_base = match base.rfind('.') {
            Some(idx) if idx + 1 < base.len() => &base[..idx],
            _ => base,
        };
        if !arg_base.is_empty() && arg_base.chars().all(is_name_char) {
            cmd_part = format!("{cmd_base}-{arg_base}");
        }
    }
    sanitize_display_name(&format!("{dir_part}-{cmd_part}"))
}

/// The sanitize step of the automatic display name.
///
/// node: src/cli.ts:971-973
pub fn sanitize_display_name(candidate: &str) -> String {
    let mut out = String::with_capacity(candidate.len());
    for c in candidate.chars() {
        let c = if is_name_char(c) { c } else { '-' };
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    out.trim_matches('-').to_string()
}

/// Replace a leading `$HOME` with `~`.
///
/// node: src/cli.ts:4114-4119
pub fn short_path(p: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return p.to_string();
    }
    if p == home {
        return "~".to_string();
    }
    if let Some(rest) = p.strip_prefix(&format!("{home}/")) {
        return format!("~/{rest}");
    }
    p.to_string()
}

/// `Ns ago` / `Nm ago` / `Nh ago` / `Nd ago` for an age in seconds.
///
/// node: src/cli.ts:4121-4130
pub fn time_ago_from_seconds(seconds: i64) -> String {
    if seconds < 60 {
        return format!("{seconds}s ago");
    }
    let minutes = seconds.div_euclid(60);
    if minutes < 60 {
        return format!("{minutes}m ago");
    }
    let hours = minutes.div_euclid(60);
    if hours < 24 {
        return format!("{hours}h ago");
    }
    format!("{}d ago", hours.div_euclid(24))
}

/// `timeAgo(new Date(iso))` against now; an unparseable timestamp yields
/// `NaN`-style output in Node, here `"? ago"`.
pub fn time_ago(iso: &str) -> String {
    match super::time::parse_iso8601_ms(iso) {
        Some(then) => {
            let seconds = (super::time::now_epoch_ms() - then).div_euclid(1000);
            time_ago_from_seconds(seconds)
        }
        None => "NaNs ago".to_string(),
    }
}
