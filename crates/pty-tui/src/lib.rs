//! TUI library for `pty` on ratatui + crossterm — the Rust successor of the
//! Node `@compoundingtech/pty/tui` framework (docs/parity.md §9).
//!
//! ratatui supplies the buffer, diffing, layout, blocks and the stock
//! widgets. This crate adds what the Node framework had and ratatui lacks:
//!
//! - [`theme`]: the 13-slot [`Theme`], nine semantic [`Color`] tokens, the
//!   eleven built-in themes and the 16-colour palette for embedded terminals;
//! - [`focus`]: the stack-based [`FocusStack`] router;
//! - [`fuzzy`]: fzf-style [`fuzzy_match`];
//! - [`input`]: Node-named [`KeyEvent`]/[`MouseEvent`] from raw bytes or
//!   crossterm events;
//! - [`line_edit`]: readline-style single-line editing;
//! - [`scroll`]: the [`ScrollRegion`] model and grouped list layout;
//! - [`app`]: the [`App`] runner with `pause`/`resume` for in-process attach;
//! - [`pane`]: the [`PtyPane`] widget over a `pty-terminal` [`CellGrid`],
//!   drawn live from any [`LiveTerminal`];
//! - [`widgets`]: the 28 Node widgets, state-first (you own the state; render
//!   and key dispatch are pure).

pub mod app;
pub mod focus;
pub mod fuzzy;
pub mod input;
pub mod line_edit;
pub mod pane;
pub mod scroll;
pub mod text;
pub mod theme;
pub mod widgets;

pub use app::{App, AppConfig, AppCtl, AppEvent, Screen};
pub use focus::{FocusGuard, FocusScope, FocusStack};
pub use fuzzy::{fuzzy_match, fuzzy_matches};
pub use input::{
    InputEvent, KeyEvent, MouseAction, MouseButton, MouseEvent, from_crossterm_key,
    from_crossterm_mouse, parse_input, parse_key,
};
pub use line_edit::{
    TextFieldState, apply_text_key, next_word_boundary, prev_word_boundary, render_field_spans,
    render_field_text,
};
pub use pane::{LiveTerminal, PtyPane, PtyPaneResult, PtyPaneSelection, PtyView};
pub use pty_terminal::{CellGrid, CellSnap, ColorSnap, Wide};
pub use scroll::{Group, GroupedLayout, GroupedRow, ScrollRegion};
pub use theme::{BoxStyle, Color, Rgb, THEMES, Theme, theme_by_name, theme_names};

pub use ratatui;
