//! Screen serialization with Node's shapes.
//!
//! - [`serialize_for_replay`]: the SCREEN payload — Node's mode prefix
//!   (`src/server.ts:1065-1082`) followed by the VT serialization of the
//!   screen. The prefix is generated from the actor's own tracked flags, so a
//!   Node client sees the same leading bytes it would from a Node daemon
//!   (`ESC[?1049h` at byte 0 when the child is in the alternate screen, and
//!   so on). The VT body is libghostty's `Format::Vt` with cursor, modes and
//!   kitty keyboard state; its bytes differ from xterm's serialize addon but
//!   restore the same picture (docs/decisions/0002-ansi-serialization.md).
//! - [`plain_viewport`] / [`plain_full`]: Node's plain peeks
//!   (`src/server.ts:1269-1293`): rows `baseY..length` or all rows, each
//!   right-trimmed of never-written cells, trailing empty rows dropped,
//!   joined by `\n`.

use libghostty_vt::fmt::{Format, Formatter, FormatterOptions};
use libghostty_vt::screen::CellContentTag;
use libghostty_vt::selection::Selection;
use libghostty_vt::style::RgbColor;
use libghostty_vt::terminal::{Point, PointCoordinate, Terminal};

use crate::actor::{Modes, TerminalActor};
use crate::graphics::{self, CellSize};

/// What a replay should carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerializeOpts {
    /// Prefix `ESC[?1049h` when the child is in the alternate screen. Node
    /// does this for ATTACH only, never for PEEK (a one-shot peek printed to
    /// the caller's shell must not switch buffers).
    pub include_alt_screen_prefix: bool,
    /// Include scrollback rows (ATTACH and `peek --full`) or only the
    /// viewport (plain `peek`, `serialize({scrollback: 0})`).
    pub scrollback: bool,
}

impl SerializeOpts {
    /// The ATTACH replay: alt-screen prefix, full scrollback.
    pub const ATTACH: SerializeOpts = SerializeOpts {
        include_alt_screen_prefix: true,
        scrollback: true,
    };
    /// `peek` (ANSI): no alt-screen prefix, viewport only.
    pub const PEEK: SerializeOpts = SerializeOpts {
        include_alt_screen_prefix: false,
        scrollback: false,
    };
    /// `peek --full` (ANSI): no alt-screen prefix, full scrollback.
    pub const PEEK_FULL: SerializeOpts = SerializeOpts {
        include_alt_screen_prefix: false,
        scrollback: true,
    };
}

/// Node's `getModePrefix(includeAltScreen)` (`src/server.ts:1065-1082`), in
/// Node's order: `?1049h` (if asked and active), `?1000h`, `?1002h`,
/// `?1003h`, `?1006h`, `?25l`, then one `CSI > flags u` per kitty stack entry.
pub fn mode_prefix(modes: &Modes, include_alt_screen: bool) -> String {
    let mut prefix = String::new();
    if include_alt_screen && modes.alt_screen {
        prefix.push_str("\x1b[?1049h");
    }
    if modes.mouse_1000 {
        prefix.push_str("\x1b[?1000h");
    }
    if modes.mouse_1002 {
        prefix.push_str("\x1b[?1002h");
    }
    if modes.mouse_1003 {
        prefix.push_str("\x1b[?1003h");
    }
    if modes.sgr_mouse {
        prefix.push_str("\x1b[?1006h");
    }
    if modes.cursor_hidden {
        prefix.push_str("\x1b[?25l");
    }
    for flags in &modes.kitty_stack {
        prefix.push_str(&format!("\x1b[>{flags}u"));
    }
    prefix
}

/// The replay payload: [`mode_prefix`] + the normal screen (when the child
/// is on the alternate one) + [`vt`].
///
/// A replay taken while a full-screen program runs carries BOTH screens, in
/// Node's order: the normal buffer, then `ESC[?1049h`, then the alternate
/// buffer. `vt` supplies the switch and the alternate half; the normal half
/// is the copy the actor took when the child left it. Without it a client
/// that reconnects gets a blank normal screen the moment the program exits.
///
/// The prefix comes first because its position is the contract (`ESC[?1049h`
/// at byte 0), which leaves the client on its alternate screen just as the
/// normal half arrives. For text that was invisible — the alternate screen is
/// overwritten immediately afterwards — but kitty image storage is per
/// screen, so the normal screen's images landed in the client's alternate
/// storage and were lost the moment the program exited. One `ESC[?1049l`
/// ahead of the normal half puts it where it belongs; the switch back is
/// [`vt`]'s own `ESC[?1049h`, which also clears and homes the alternate
/// screen the way the body that follows it expects.
///
/// node: src/server.ts:962 and 1017 (`serialize.serialize()`, whose addon
/// walks both buffers).
pub fn serialize_for_replay(actor: &TerminalActor, opts: SerializeOpts) -> String {
    let modes = actor.modes();
    let mut out = mode_prefix(&modes, opts.include_alt_screen_prefix);
    out.push_str(&dynamic_color_prefix(actor.terminal()));
    if let Some(normal) = actor.normal_replay() {
        if opts.include_alt_screen_prefix && modes.alt_screen {
            // Back to the normal screen for its own half, then to the
            // alternate one again with the cursor homed — Node's
            // `ESC[?1049h ESC[H` — because the normal half leaves the cursor
            // wherever its own trailing CUP put it and the alternate body
            // that follows is written from wherever the cursor is.
            out.push_str("\x1b[?1049l");
            out.push_str(normal);
            out.push_str("\x1b[?1049h\x1b[H");
        } else {
            out.push_str(normal);
        }
    }
    out.push_str(&vt(actor.terminal(), opts.scrollback, actor.cell_size()));
    out
}

/// Restore only colors the child changed from the terminal defaults. This
/// leaves an attached host's own theme alone for untouched entries.
fn dynamic_color_prefix(term: &Terminal<'_, '_>) -> String {
    let mut out = String::new();
    for (id, current, default) in [
        (10, term.fg_color(), term.default_fg_color()),
        (11, term.bg_color(), term.default_bg_color()),
        (12, term.cursor_color(), term.default_cursor_color()),
    ] {
        if let (Ok(Some(color)), Ok(base)) = (current, default)
            && Some(color) != base
        {
            out.push_str(&osc_rgb(id, color));
        }
    }
    if let (Ok(current), Ok(default)) = (term.color_palette(), term.default_color_palette()) {
        for (i, color) in current.0.iter().enumerate() {
            if *color != default.0[i] {
                out.push_str(&format!("\x1b]4;{i};rgb:{:02x}/{:02x}/{:02x}\x1b\\", color.r, color.g, color.b));
            }
        }
    }
    out
}

fn osc_rgb(id: u8, color: RgbColor) -> String {
    format!("\x1b]{id};rgb:{:02x}/{:02x}/{:02x}\x1b\\", color.r, color.g, color.b)
}

fn format(term: &Terminal, opts: FormatterOptions) -> String {
    let mut f = match Formatter::new(term, opts) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    match f.format_alloc(None) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => String::new(),
    }
}

/// Format the active area only (rows `baseY..length`) by selecting it.
fn format_active(term: &Terminal, opts: FormatterOptions) -> String {
    let rows = term.rows().unwrap_or(1).max(1);
    let cols = term.cols().unwrap_or(1).max(1);
    let start = term.grid_ref(Point::Active(PointCoordinate { x: 0, y: 0 }));
    let end = term.grid_ref(Point::Active(PointCoordinate {
        x: cols - 1,
        y: (rows - 1) as u32,
    }));
    match (start, end) {
        (Ok(s), Ok(e)) => {
            let sel = Selection::new(s, e, false);
            format(term, opts.with_selection(&sel))
        }
        _ => format(term, opts),
    }
}

fn vt_opts<'t, 's>() -> FormatterOptions<'t, 's> {
    // `unwrap`: soft-wrapped rows are emitted as one run so the client wraps
    // them again and keeps the wrapped flags (xterm's addon does the same).
    // The cursor is positioned by `vt` itself, after the row padding.
    FormatterOptions::new()
        .with_format(Format::Vt)
        .with_unwrap(true)
        .with_trim(false)
        .with_cursor(false)
        .with_modes(true)
        .with_kitty_keyboard(true)
}

fn row_has_text(term: &Terminal, y: u32, cols: u16) -> bool {
    (0..cols).any(|x| {
        term.grid_ref(Point::Screen(PointCoordinate { x, y }))
            .ok()
            .and_then(|g| g.cell().ok())
            .and_then(|c| c.has_text().ok())
            .unwrap_or(false)
    })
}

/// What one walk over the active area learns.
struct ActiveScan {
    /// Active row index of the last row with text, if any.
    last_text_row: Option<usize>,
    /// Sequences restoring the background of rows that have no text.
    bg_fixups: String,
}

/// Walk the active area once: find the last row with text (libghostty's
/// formatter never emits the blank rows after it) and build the background
/// fix-ups for text-less rows.
///
/// libghostty's formatter drops rows without text even when their cells
/// carry a background (a TUI that clears rows with `SGR bg` + `EL`). xterm's
/// serializer keeps them as `SGR bg` + `ECH n`; this re-emits them the same
/// way, so those cells come back as background-only cells, not spaces.
fn scan_active(term: &Terminal) -> ActiveScan {
    let rows = term.rows().unwrap_or(0);
    let cols = term.cols().unwrap_or(0);
    let mut last_text_row = None;
    let mut bg_fixups = String::new();
    for y in 0..rows {
        // (start x, run length, SGR params) of background-only runs.
        let mut runs: Vec<(u16, u16, String)> = Vec::new();
        let mut has_text = false;
        for x in 0..cols {
            let cell = term
                .grid_ref(Point::Active(PointCoordinate { x, y: y as u32 }))
                .ok()
                .and_then(|g| g.cell().ok());
            let Some(cell) = cell else { continue };
            if cell.has_text().unwrap_or(false) {
                has_text = true;
                break;
            }
            let sgr = match cell.content_tag() {
                Ok(CellContentTag::BgColorPalette) => cell
                    .bg_color_palette()
                    .ok()
                    .map(|i| format!("48;5;{}", i.0)),
                Ok(CellContentTag::BgColorRgb) => cell
                    .bg_color_rgb()
                    .ok()
                    .map(|c| format!("48;2;{};{};{}", c.r, c.g, c.b)),
                _ => None,
            };
            if let Some(sgr) = sgr {
                match runs.last_mut() {
                    Some((sx, len, p)) if *sx + *len == x && *p == sgr => *len += 1,
                    _ => runs.push((x, 1, sgr)),
                }
            }
        }
        if has_text {
            last_text_row = Some(y as usize);
            continue;
        }
        for (sx, len, sgr) in runs {
            bg_fixups.push_str(&format!("\x1b[{};{}H\x1b[{sgr}m\x1b[{len}X", y + 1, sx + 1));
        }
    }
    if !bg_fixups.is_empty() {
        bg_fixups.push_str("\x1b[0m");
    }
    ActiveScan {
        last_text_row,
        bg_fixups,
    }
}

/// Buffer index of the last row with any text: the active area's answer, or
/// a scan back through history when the active area has none.
fn last_text_row(term: &Terminal, active: &ActiveScan) -> Option<usize> {
    let base = term.scrollback_rows().unwrap_or(0);
    if let Some(y) = active.last_text_row {
        return Some(base + y);
    }
    let cols = term.cols().unwrap_or(0);
    (0..base).rev().find(|&y| row_has_text(term, y as u32, cols))
}

fn plain_opts<'t, 's>() -> FormatterOptions<'t, 's> {
    // `trim(false)` keeps written trailing spaces (a prompt's "$ ") and still
    // drops never-written cells — exactly xterm's `translateToString(true)`.
    FormatterOptions::new()
        .with_format(Format::Plain)
        .with_unwrap(false)
        .with_trim(false)
}

/// The VT serialization: cells with styles, then cursor position, modes that
/// differ from their defaults, the kitty keyboard flags, and the kitty
/// graphics storage. With `scrollback` the history rows come first; without
/// it only the active area is emitted.
///
/// The graphics block comes last, after the cursor move, because it must not
/// change where the cursor ends up: libghostty's formatter keeps the
/// placeholder cells of a virtual placement (they are ordinary text) but
/// neither the images nor the placements, so without the block a client would
/// replay placeholders naming images it never received
/// (docs/decisions/0012-kitty-graphics-replay.md). It is empty for a terminal
/// with no graphics, so a session that never sent an image is byte-identical
/// to before.
pub fn vt(term: &Terminal, scrollback: bool, cell: CellSize) -> String {
    let mut out = if scrollback {
        format(term, vt_opts())
    } else {
        format_active(term, vt_opts())
    };
    let active = scan_active(term);
    if scrollback {
        // The formatter drops trailing blank rows, and the cursor position
        // is absolute within the viewport. With scrollback, a client that
        // parsed fewer rows than the source has would place its viewport
        // (and the cursor) one row too high for every dropped row, and the
        // next DATA would land on the wrong line. Pad the missing rows so
        // the client's buffer has as many rows as ours. (xterm's serializer
        // keeps those rows whenever the buffer exceeds the viewport.)
        let rows = term.rows().unwrap_or(0) as usize;
        let total = term.total_rows().unwrap_or(rows);
        let emitted = last_text_row(term, &active).map_or(0, |y| y + 1);
        for _ in emitted.max(rows)..total {
            out.push_str("\r\n");
        }
    }
    out.push_str(&active.bg_fixups);
    let cx = term.cursor_x().unwrap_or(0);
    let cy = term.cursor_y().unwrap_or(0);
    out.push_str(&format!("\x1b[{};{}H", cy + 1, cx + 1));
    out.push_str(&graphics::replay(term, cell));
    out
}

fn plain_lines(text: String) -> Vec<String> {
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    while lines.last().map(|l| l.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines
}

/// All rows (history + viewport) as trimmed lines, trailing empty rows
/// dropped. What [`plain_full`] joins.
pub fn plain_lines_full(term: &Terminal) -> Vec<String> {
    plain_lines(format(term, plain_opts()))
}

/// The viewport rows as trimmed lines, trailing empty rows dropped. What
/// [`plain_viewport`] joins.
pub fn plain_lines_viewport(term: &Terminal) -> Vec<String> {
    plain_lines(format_active(term, plain_opts()))
}

/// Node's `getPlainScreen()`: rows `baseY..length`.
pub fn plain_viewport(term: &Terminal) -> String {
    plain_lines_viewport(term).join("\n")
}

/// Node's `getFullPlainScreen()`: every row.
pub fn plain_full(term: &Terminal) -> String {
    plain_lines_full(term).join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_prefix_matches_node_order() {
        let m = Modes {
            sgr_mouse: true,
            mouse_1000: true,
            mouse_1002: true,
            mouse_1003: true,
            alt_screen: true,
            cursor_hidden: true,
            bracketed_paste: true,
            focus_events: true,
            kitty_stack: vec![7, 1],
            // Neither of these reaches the prefix: Node does not track them,
            // and a replay must not tell the client's terminal to turn a mode
            // on that the daemon's own prefix never carried.
            mouse_9: true,
            app_cursor: true,
        };
        assert_eq!(
            mode_prefix(&m, true),
            "\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[?25l\x1b[>7u\x1b[>1u"
        );
        assert_eq!(
            mode_prefix(&m, false),
            "\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[?25l\x1b[>7u\x1b[>1u"
        );
        assert_eq!(mode_prefix(&Modes::default(), true), "");
    }
}
