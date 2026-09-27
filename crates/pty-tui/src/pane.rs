//! The live pty pane, ported from `src/tui/widgets/pty-pane.ts` and the
//! `ptyView` node (`screen.ts:705-739`): renders a [`CellGrid`] read from a
//! [`LiveTerminal`] into a ratatui buffer with palette indices preserved,
//! draws focus-coloured chrome with a title, highlights a content-anchored
//! selection, and reports the cursor only when the pane is focused and the
//! cursor is on screen.
//!
//! Widgets are pure over a grid; [`PtyPane::render_handle`] is the
//! convenience that resizes the handle to the inner rect, reads the grid
//! (through a per-handle cache keyed by revision) and renders it. A handle is
//! anything that implements [`LiveTerminal`], such as the `pty` crate's
//! `TerminalHandle`; this crate never spawns or connects to what feeds it.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use pty_terminal::{CellGrid, CellSnap, ColorSnap, Wide};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color as RColor, Modifier, Style};
use ratatui::widgets::Widget;

use crate::text::text_width;
use crate::theme::{BoxStyle, Rgb, Theme, to_ratatui};

/// A live terminal a pane can draw: a revision that bumps on every change,
/// a size, and the grid at a scroll offset.
///
/// The `pty` crate's `TerminalHandle` is one. Keeping the pane on this trait
/// is what lets this crate render a live session without depending on the
/// child process or daemon socket behind it.
pub trait LiveTerminal {
    /// The current revision; bumps on every change.
    fn rev(&self) -> u64;
    /// Current width.
    fn cols(&self) -> u16;
    /// Current height.
    fn rows(&self) -> u16;
    /// The cell grid `scroll_offset` rows back into history (0 = live).
    fn snapshot(&self, scroll_offset: usize) -> CellGrid;
    /// Resize to `cols` x `rows`. A no-op when the size is unchanged or the
    /// terminal cannot be resized from here.
    fn resize(&self, cols: u16, rows: u16);
}

/// A selection in pane-inner cell coordinates captured at `scroll_offset`
/// (`PtyPaneSelection`, `pty-pane.ts:33-41`). The highlight follows the
/// content, not the screen position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtyPaneSelection {
    pub start_row: i32,
    pub start_col: i32,
    pub end_row: i32,
    pub end_col: i32,
    pub scroll_offset: i32,
}

impl PtyPaneSelection {
    /// Has the selection any extent (`hasDragDistance`)?
    pub fn has_extent(&self) -> bool {
        self.start_row != self.end_row || self.start_col != self.end_col
    }

    /// Does inner cell `(row, col)` fall inside, viewed at
    /// `current_scroll_offset` (`isSelectedInPane`, `pty-pane.ts:125-143`)?
    pub fn contains(&self, row: i32, col: i32, current_scroll_offset: i32) -> bool {
        let delta = current_scroll_offset - self.scroll_offset;
        let r = row - delta;
        let (mut r1, mut c1, mut r2, mut c2) =
            (self.start_row, self.start_col, self.end_row, self.end_col);
        if r1 > r2 || (r1 == r2 && c1 > c2) {
            std::mem::swap(&mut r1, &mut r2);
            std::mem::swap(&mut c1, &mut c2);
        }
        if r < r1 || r > r2 {
            return false;
        }
        if r == r1 && r == r2 {
            return col >= c1 && col <= c2;
        }
        if r == r1 {
            return col >= c1;
        }
        if r == r2 {
            return col <= c2;
        }
        true
    }
}

/// What a render reports (`PtyPaneResult`, `pty-pane.ts:69-76`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtyPaneResult {
    /// The child cursor's screen position (0-based, in buffer coordinates —
    /// pass to `Frame::set_cursor_position`), or `None` when the pane is
    /// unfocused or the cursor is scrolled off screen.
    pub cursor: Option<Position>,
    /// The inner content rect the cells were drawn into.
    pub inner: Rect,
}

/// The inner content rect (`ptyPaneInnerRect`, `pty-pane.ts:98-106`).
pub fn inner_rect(rect: Rect, chrome: bool) -> Rect {
    if !chrome {
        return rect;
    }
    Rect {
        x: rect.x + 1,
        y: rect.y + 1,
        width: rect.width.saturating_sub(2),
        height: rect.height.saturating_sub(2),
    }
}

/// The cursor's effective inner row at `scroll_offset`, or `None` when it
/// is pushed off the bottom (`effectiveCursorRow`, `pty-pane.ts:112-120`).
pub fn effective_cursor_row(cursor_row: i32, scroll_offset: i32, inner_height: i32) -> Option<i32> {
    let effective = cursor_row + scroll_offset;
    (effective >= 0 && effective < inner_height).then_some(effective)
}

/// A snapshot colour as a ratatui colour, palette index preserved.
pub fn snap_color(c: ColorSnap) -> RColor {
    match c {
        ColorSnap::Default => RColor::Reset,
        ColorSnap::Indexed(i) => RColor::Indexed(i),
        ColorSnap::Rgb(r, g, b) => RColor::Rgb(r, g, b),
    }
}

/// The ratatui style of a cell.
pub fn cell_style(cell: &CellSnap) -> Style {
    let mut style = Style::default().fg(snap_color(cell.fg)).bg(snap_color(cell.bg));
    if cell.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.dim {
        style = style.add_modifier(Modifier::DIM);
    }
    if cell.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.inverse {
        style = style.add_modifier(Modifier::REVERSED);
    }
    if cell.strikethrough {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    style
}

/// Blit a grid into `buf` at `area` (the `ptyView` blit, `screen.ts:722-737`),
/// inverting cells inside `selection`.
pub fn blit_grid(
    buf: &mut Buffer,
    area: Rect,
    grid: &CellGrid,
    selection: Option<&PtyPaneSelection>,
    scroll_offset: usize,
) {
    let show_selection = selection.is_some_and(|s| s.has_extent());
    for (r, row) in grid.rows.iter().enumerate() {
        if r >= area.height as usize {
            break;
        }
        let y = area.y + r as u16;
        let mut c = 0usize;
        while c < row.len() && c < area.width as usize {
            let cell = &row[c];
            let x = area.x + c as u16;
            let Some(target) = buf.cell_mut(Position::new(x, y)) else {
                c += 1;
                continue;
            };
            let mut style = cell_style(cell);
            if show_selection
                && let Some(sel) = selection
                && sel.contains(r as i32, c as i32, scroll_offset as i32)
            {
                // Invert fg/bg (indices included) for the highlight.
                let fg = match cell.bg {
                    ColorSnap::Default => RColor::Rgb(0, 0, 0),
                    other => snap_color(other),
                };
                let bg = match cell.fg {
                    ColorSnap::Default => RColor::Rgb(200, 200, 200),
                    other => snap_color(other),
                };
                style = style.fg(fg).bg(bg);
            }
            match cell.wide {
                Wide::Spacer => {
                    // Second half of a wide char: an empty symbol so ratatui
                    // skips it and the wide glyph before it spans two cells.
                    target.set_symbol("");
                    target.set_style(style);
                }
                _ => {
                    let text = if cell.text.is_empty() { " " } else { cell.text.as_str() };
                    target.set_symbol(text);
                    target.set_style(style);
                }
            }
            c += 1;
        }
    }
}

/// A bordered, titled, focus-aware pane over a grid (`renderPtyPane`,
/// `pty-pane.ts:154-255`).
#[derive(Debug, Clone)]
pub struct PtyPane<'a> {
    pub grid: &'a CellGrid,
    pub theme: Theme,
    pub title: Option<String>,
    pub focused: bool,
    /// Draw border + title. Default true.
    pub chrome: bool,
    pub box_style: BoxStyle,
    pub scroll_offset: usize,
    pub selection: Option<PtyPaneSelection>,
    /// Overrides the focused border colour (default `theme.fg_ac`).
    pub border_color: Option<Rgb>,
    /// Overrides the unfocused border colour (default `theme.border` then
    /// `theme.fg_mu`).
    pub muted_border_color: Option<Rgb>,
}

impl<'a> PtyPane<'a> {
    pub fn new(grid: &'a CellGrid, theme: Theme) -> Self {
        PtyPane {
            grid,
            theme,
            title: None,
            focused: false,
            chrome: true,
            box_style: BoxStyle::Rounded,
            scroll_offset: 0,
            selection: None,
            border_color: None,
            muted_border_color: None,
        }
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    pub fn chrome(mut self, chrome: bool) -> Self {
        self.chrome = chrome;
        self
    }

    pub fn box_style(mut self, style: BoxStyle) -> Self {
        self.box_style = style;
        self
    }

    pub fn scroll_offset(mut self, offset: usize) -> Self {
        self.scroll_offset = offset;
        self
    }

    pub fn selection(mut self, sel: Option<PtyPaneSelection>) -> Self {
        self.selection = sel;
        self
    }

    /// The border colour for the focus state.
    fn border_rgb(&self) -> Rgb {
        if self.focused {
            self.border_color
                .or(self.theme.fg_ac)
                .unwrap_or((80, 200, 120))
        } else {
            self.muted_border_color
                .or(self.theme.border)
                .or(self.theme.fg_mu)
                .unwrap_or((100, 100, 100))
        }
    }

    /// Render and report the cursor.
    pub fn render_with_result(&self, area: Rect, buf: &mut Buffer) -> PtyPaneResult {
        let inner = inner_rect(area, self.chrome);
        if self.chrome && area.width >= 2 && area.height >= 2 {
            draw_box(buf, area, self.box_style, self.title.as_deref(), self.border_rgb());
        }
        let mut cursor = None;
        if inner.width > 0 && inner.height > 0 {
            blit_grid(buf, inner, self.grid, self.selection.as_ref(), self.scroll_offset);
            if self.focused {
                let (row, col, _visible) = self.grid.cursor;
                if let Some(eff) = effective_cursor_row(
                    row as i32,
                    self.scroll_offset as i32,
                    inner.height as i32,
                ) && (col as i32) < inner.width as i32
                {
                    cursor = Some(Position::new(inner.x + col, inner.y + eff as u16));
                }
            }
        }
        PtyPaneResult { cursor, inner }
    }

    /// Resize `handle` to the inner rect, read its grid through the cache
    /// and render it. The cache is keyed by the handle's revision, size and
    /// scroll offset, so a clean pane costs no snapshot.
    pub fn render_handle<H: LiveTerminal + ?Sized>(
        area: Rect,
        buf: &mut Buffer,
        handle: &H,
        theme: Theme,
        configure: impl FnOnce(PtyPane<'_>) -> PtyPane<'_>,
    ) -> PtyPaneResult {
        // Configure once against an empty grid to learn chrome/offset.
        let probe = CellGrid::default();
        let probe_pane = configure(PtyPane::new(&probe, theme));
        let chrome = probe_pane.chrome;
        let scroll_offset = probe_pane.scroll_offset;
        let inner = inner_rect(area, chrome);
        if inner.width > 0 && inner.height > 0 {
            handle.resize(inner.width, inner.height);
        }
        let grid = cached_grid(handle, scroll_offset);
        let mut pane = PtyPane::new(&grid, theme);
        pane.title = probe_pane.title;
        pane.focused = probe_pane.focused;
        pane.chrome = chrome;
        pane.box_style = probe_pane.box_style;
        pane.scroll_offset = scroll_offset;
        pane.selection = probe_pane.selection;
        pane.border_color = probe_pane.border_color;
        pane.muted_border_color = probe_pane.muted_border_color;
        pane.render_with_result(area, buf)
    }
}

impl Widget for PtyPane<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        self.render_with_result(area, buf);
    }
}

/// The base `ptyView` node: a grid without chrome that fills its area.
#[derive(Debug, Clone)]
pub struct PtyView<'a> {
    pub grid: &'a CellGrid,
}

impl<'a> PtyView<'a> {
    pub fn new(grid: &'a CellGrid) -> Self {
        PtyView { grid }
    }
}

impl Widget for PtyView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        blit_grid(buf, area, self.grid, None, 0);
    }
}

/// Draw a box with an optional title the way `drawBox` does
/// (`colors.ts:229-257`): `╭─ title ───╮` when the title fits.
pub fn draw_box(buf: &mut Buffer, area: Rect, style: BoxStyle, title: Option<&str>, color: Rgb) {
    let set = style.border_set();
    let fg = Style::default().fg(to_ratatui(Some(color)));
    let w = area.width as usize;
    let h = area.height as usize;
    if w < 2 || h < 2 {
        return;
    }
    let mut top = set.horizontal_top.repeat(w - 2);
    if let Some(title) = title {
        let t_len = text_width(title) + 2;
        if t_len < w - 4 {
            let rest = w - 2 - t_len - 1;
            top = format!(
                "{} {} {}",
                set.horizontal_top,
                title,
                set.horizontal_top.repeat(rest)
            );
        }
    }
    let top_line = format!("{}{}{}", set.top_left, top, set.top_right);
    buf.set_stringn(area.x, area.y, &top_line, w, fg);
    for r in 1..h - 1 {
        let y = area.y + r as u16;
        buf.set_stringn(area.x, y, set.vertical_left, 1, fg);
        buf.set_stringn(area.x + (w - 1) as u16, y, set.vertical_right, 1, fg);
    }
    let bottom = format!(
        "{}{}{}",
        set.bottom_left,
        set.horizontal_bottom.repeat(w - 2),
        set.bottom_right
    );
    buf.set_stringn(area.x, area.y + (h - 1) as u16, &bottom, w, fg);
}

struct CacheEntry {
    rev: u64,
    offset: usize,
    cols: u16,
    rows: u16,
    grid: CellGrid,
}

fn cache() -> &'static Mutex<HashMap<usize, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<usize, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn handle_key<H: ?Sized>(handle: &H) -> usize {
    (handle as *const H).cast::<()>() as usize
}

/// The grid for `handle` at `offset`, reused while the handle's revision,
/// size and offset are unchanged (the `WeakMap` cache, `pty-pane.ts:82-94`).
pub fn cached_grid<H: LiveTerminal + ?Sized>(handle: &H, offset: usize) -> CellGrid {
    let key = handle_key(handle);
    let rev = handle.rev();
    let (cols, rows) = (handle.cols(), handle.rows());
    if let Ok(c) = cache().lock()
        && let Some(e) = c.get(&key)
        && e.rev == rev
        && e.offset == offset
        && e.cols == cols
        && e.rows == rows
    {
        return e.grid.clone();
    }
    let grid = handle.snapshot(offset);
    if let Ok(mut c) = cache().lock() {
        c.insert(
            key,
            CacheEntry {
                rev,
                offset,
                cols,
                rows,
                grid: grid.clone(),
            },
        );
    }
    grid
}

/// Drop a handle's cached grid (`clearPtyPaneCache`). Call when the handle
/// is closed so a later handle at the same address starts clean.
pub fn clear_pane_cache<H: ?Sized>(handle: &H) {
    if let Ok(mut c) = cache().lock() {
        c.remove(&handle_key(handle));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(rows: &[&str]) -> CellGrid {
        let cols = rows.iter().map(|r| r.chars().count()).max().unwrap_or(0) as u16;
        let mut g = CellGrid {
            cols,
            rows_n: rows.len() as u16,
            ..Default::default()
        };
        for r in rows {
            let mut row: Vec<CellSnap> = r
                .chars()
                .map(|c| CellSnap {
                    text: c.to_string(),
                    ..Default::default()
                })
                .collect();
            row.resize(cols as usize, CellSnap::default());
            g.rows.push(row);
            g.wrapped.push(false);
        }
        g
    }

    fn find(buf: &Buffer, ch: &str) -> Option<(u16, u16)> {
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf[(x, y)].symbol() == ch {
                    return Some((y, x));
                }
            }
        }
        None
    }

    fn row_text(buf: &Buffer, y: u16, width: u16) -> String {
        (0..width).map(|x| buf[(x, y)].symbol().to_string()).collect()
    }

    /// node: tests/pty-pane.test.ts:73-88
    #[test]
    fn inner_rect_insets() {
        assert_eq!(inner_rect(Rect::new(0, 0, 20, 6), true), Rect::new(1, 1, 18, 4));
        assert_eq!(inner_rect(Rect::new(3, 2, 10, 5), false), Rect::new(3, 2, 10, 5));
        assert_eq!(inner_rect(Rect::new(0, 0, 1, 1), true), Rect::new(1, 1, 0, 0));
    }

    /// node: tests/pty-pane.test.ts:90-111
    #[test]
    fn selection_contains() {
        let sel = PtyPaneSelection {
            start_row: 1,
            start_col: 2,
            end_row: 3,
            end_col: 4,
            scroll_offset: 0,
        };
        assert!(sel.contains(2, 0, 0));
        assert!(!sel.contains(1, 1, 0));
        assert!(sel.contains(1, 2, 0));
        assert!(sel.contains(3, 4, 0));
        assert!(!sel.contains(3, 5, 0));
        assert!(sel.contains(3, 0, 1));
        assert!(!sel.contains(2, 0, 1));
    }

    /// node: tests/pty-pane.test.ts:114-129
    #[test]
    fn draws_border_with_title_and_content() {
        let mut g = grid(&["HELLO"]);
        g.cursor = (0, 5, true);
        let mut buf = Buffer::empty(Rect::new(0, 0, 30, 10));
        let res = PtyPane::new(&g, crate::theme::COOL_BLUE)
            .title("term")
            .focused(true)
            .render_with_result(Rect::new(0, 0, 20, 6), &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "╭");
        assert!(row_text(&buf, 0, 20).contains("term"));
        assert_eq!(row_text(&buf, 0, 20), "╭─ term ───────────╮");
        assert_eq!(find(&buf, "H"), Some((1, 1)));
        assert_eq!(res.inner, Rect::new(1, 1, 18, 4));
        // Focused border uses the accent colour.
        assert_eq!(buf[(0, 0)].fg, RColor::Rgb(100, 160, 255));
    }

    /// node: tests/pty-pane.test.ts:131-140
    #[test]
    fn no_chrome_blits_at_origin() {
        let g = grid(&["HELLO"]);
        let mut buf = Buffer::empty(Rect::new(0, 0, 30, 10));
        PtyPane::new(&g, crate::theme::COOL_BLUE)
            .chrome(false)
            .render(Rect::new(0, 0, 20, 5), &mut buf);
        assert_ne!(buf[(0, 0)].symbol(), "╭");
        assert_eq!(find(&buf, "H"), Some((0, 0)));
    }

    /// node: tests/pty-pane.test.ts:142-155
    #[test]
    fn preserves_palette_index() {
        let mut g = grid(&["B"]);
        g.rows[0][0].fg = ColorSnap::Indexed(4);
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 6));
        PtyView::new(&g).render(Rect::new(0, 0, 10, 3), &mut buf);
        assert_eq!(buf[(0, 0)].fg, RColor::Indexed(4));
    }

    /// node: tests/pty-pane.test.ts:157-197
    #[test]
    fn cursor_reporting() {
        let mut g = grid(&["hi"]);
        g.cursor = (0, 2, true);
        let mut buf = Buffer::empty(Rect::new(0, 0, 30, 10));
        let res = PtyPane::new(&g, crate::theme::COOL_BLUE)
            .focused(true)
            .render_with_result(Rect::new(0, 0, 20, 6), &mut buf);
        assert_eq!(res.cursor, Some(Position::new(3, 1)));
        let res = PtyPane::new(&g, crate::theme::COOL_BLUE)
            .focused(false)
            .render_with_result(Rect::new(0, 0, 20, 6), &mut buf);
        assert_eq!(res.cursor, None);
        let res = PtyPane::new(&g, crate::theme::COOL_BLUE)
            .focused(true)
            .scroll_offset(10)
            .render_with_result(Rect::new(0, 0, 20, 6), &mut buf);
        assert_eq!(res.cursor, None);
    }

    /// node: tests/pty-pane.test.ts:199-213
    #[test]
    fn inverts_selected_cells() {
        let mut g = grid(&["R"]);
        g.rows[0][0].fg = ColorSnap::Indexed(1);
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 6));
        PtyPane::new(&g, crate::theme::COOL_BLUE)
            .chrome(false)
            .selection(Some(PtyPaneSelection {
                start_row: 0,
                start_col: 0,
                end_row: 0,
                end_col: 3,
                scroll_offset: 0,
            }))
            .render(Rect::new(0, 0, 10, 3), &mut buf);
        assert_eq!(buf[(0, 0)].bg, RColor::Indexed(1));
        assert_eq!(buf[(0, 0)].fg, RColor::Rgb(0, 0, 0));
    }

    #[test]
    fn wide_cells_keep_their_spacer() {
        let mut g = grid(&["日 x"]);
        g.rows[0][0].wide = Wide::Wide;
        g.rows[0][1] = CellSnap {
            text: String::new(),
            wide: Wide::Spacer,
            ..Default::default()
        };
        let mut buf = Buffer::empty(Rect::new(0, 0, 5, 1));
        PtyView::new(&g).render(Rect::new(0, 0, 5, 1), &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "日");
        assert_eq!(buf[(1, 0)].symbol(), "");
        assert_eq!(buf[(2, 0)].symbol(), "x");
    }
}
