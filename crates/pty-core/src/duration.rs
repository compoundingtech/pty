//! Duration parse/format helpers, ported from the pty project's `src/duration.ts`.
//!
//! `parse_duration` accepts strict compact `Ns|Nm|Nh|Nd` strings and returns
//! milliseconds. `format_duration` renders a millisecond value back to a
//! compact string (`45s`, `2h12m`, `3d2h`).

/// Parse a compact single-unit duration (`30s`, `5m`, `2h`, `7d`) into
/// milliseconds. Case-insensitive on the unit; tolerates surrounding and a
/// single internal run of whitespace. Returns `None` for compound forms,
/// missing unit/number, unknown units, negatives, or non-integers.
pub fn parse_duration(input: &str) -> Option<i64> {
    let s = input.trim();
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    // Leading digits.
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 {
        return None; // no number
    }
    let digits = &s[..i];
    // Optional internal whitespace.
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    // Exactly one unit char must remain.
    if i != bytes.len() - 1 {
        return None;
    }
    let unit = (bytes[i] as char).to_ascii_lowercase();
    let unit_ms: i64 = match unit {
        's' => 1000,
        'm' => 60 * 1000,
        'h' => 60 * 60 * 1000,
        'd' => 24 * 60 * 60 * 1000,
        _ => return None,
    };
    let n: i64 = digits.parse().ok()?;
    if n < 0 {
        return None;
    }
    n.checked_mul(unit_ms)
}

/// Render a millisecond value into a compact duration string. Negative values
/// are treated as 0.
pub fn format_duration(ms: i64) -> String {
    let seconds = (ms / 1000).max(0);
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        let s = seconds % 60;
        return if s == 0 {
            format!("{minutes}m")
        } else {
            format!("{minutes}m{s}s")
        };
    }
    let hours = minutes / 60;
    if hours < 24 {
        let m = minutes % 60;
        return if m == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h{m}m")
        };
    }
    let days = hours / 24;
    let h = hours % 24;
    if h == 0 {
        format!("{days}d")
    } else {
        format!("{days}d{h}h")
    }
}
