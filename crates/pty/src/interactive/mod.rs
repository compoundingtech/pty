//! The interactive session manager: `pty` with no arguments.
//!
//! A list of this machine's sessions with a fuzzy filter over it. Return
//! attaches to the one you picked, or restarts it first if it has stopped,
//! or creates a new one from the last row. Detaching brings you back to the
//! list rather than to the shell.
//!
//! Relay host groups are not here yet. Everything the picker does is local,
//! so the section headers Node draws for remote hosts have nothing to show
//! and are left out until the relay work lands.
//!
//! node: src/tui/session-manager.ts; docs/parity.md §9

mod row;

use std::time::{Duration, Instant};

use pty_core::registry::{self, SessionInfo, TagMap};
use pty_tui::app::RenderCtx;
use pty_tui::input::KeyEvent;
use pty_tui::line_edit::{TextFieldState, apply_text_key, render_field_spans};
use pty_tui::ratatui::Frame;
use pty_tui::ratatui::layout::{Constraint, Direction, Layout, Rect};
use pty_tui::ratatui::style::{Modifier, Style};
use pty_tui::ratatui::text::{Line, Span};
use pty_tui::ratatui::widgets::{Block, Paragraph};
use pty_tui::theme::to_ratatui;
use pty_tui::{App, AppConfig, AppCtl, Screen, ScrollRegion, Theme, theme_by_name, theme_names};

use crate::cli::InteractiveOptions;

pub use row::Row;

/// How often the list re-reads the registry. Paused while attached.
const REFRESH: Duration = Duration::from_secs(1);

/// `pty` / `pty i` / `pty interactive`, past the nesting guard.
pub fn run(opts: InteractiveOptions) -> i32 {
    let (theme, theme_name) = load_theme();
    let mut picker = Picker::new(opts, theme, theme_name);
    let config = AppConfig {
        theme,
        tick: Some(REFRESH),
        ..AppConfig::default()
    };
    match App::run::<(), _>(config, &mut picker) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("pty: {e}");
            1
        }
    }
}

/// The picked theme lives in `<root>/theme` so it survives a restart.
///
/// node: the session manager's theme file
fn theme_path() -> std::path::PathBuf {
    registry::session_dir().join("theme")
}

/// The saved theme, or the default when there is none or the name is gone.
fn load_theme() -> (Theme, String) {
    let saved = std::fs::read_to_string(theme_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    match saved.as_deref().and_then(|n| theme_by_name(n).map(|t| (t, n))) {
        Some((theme, name)) => (theme, name.to_string()),
        // **The default is the terminal's own theme, which paints nothing.**
        // Every slot in `TERMINAL` is `None`, so no colour sequence is emitted
        // and whatever the user has configured shows through. The Node tool
        // has defaulted this way deliberately since the picker was written
        // (`interactive.ts`: `return terminalIdx >= 0 ? terminalIdx : 0`), and
        // it is the specification here rather than a comparison.
        //
        // Defaulting to a painted theme means a dark palette on a light
        // terminal, which is the report that produced this line.
        None => (pty_tui::theme::TERMINAL, "terminal".to_string()),
    }
}

fn save_theme(name: &str) {
    let path = theme_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, format!("{name}\n"));
}

struct Picker {
    /// Every session, newest read from the registry.
    all: Vec<SessionInfo>,
    /// The rows the filter left, in the order they are drawn.
    shown: Vec<Row>,
    filter: TextFieldState,
    /// Tags every listed session must carry, from `--filter-tag`. A new
    /// session inherits them.
    filter_tags: TagMap,
    region: ScrollRegion,
    /// Select the create row on the first draw (`--preselect-new`).
    preselect_new: bool,
    theme: Theme,
    /// The theme's name, for the footer and the theme file. `Theme` is just
    /// the thirteen colours.
    theme_name: String,
    last_refresh: Instant,
    /// Reported under the list until the next keystroke.
    notice: Option<String>,
}

impl Picker {
    fn new(opts: InteractiveOptions, theme: Theme, theme_name: String) -> Self {
        let mut picker = Picker {
            all: Vec::new(),
            shown: Vec::new(),
            filter: TextFieldState::empty(),
            filter_tags: opts.filter_tags,
            region: ScrollRegion::new(0, 10),
            preselect_new: opts.preselect_new,
            theme,
            theme_name,
            last_refresh: Instant::now(),
            notice: None,
        };
        picker.refresh();
        if picker.preselect_new {
            picker.region.selected = picker.shown.len().saturating_sub(1);
            picker.region = picker.region.ensure_visible();
        }
        picker
    }

    /// Re-read the registry and rebuild the rows, keeping the selection on
    /// the same session where that session is still listed.
    fn refresh(&mut self) {
        let selected_name = self.selected_session_name();
        self.all = registry::list_sessions();
        if !self.filter_tags.is_empty() {
            self.all.retain(|s| {
                registry::matches_all_tags(
                    s.metadata.as_ref().and_then(|m| m.tags.as_ref()),
                    &self.filter_tags,
                )
            });
        }
        self.rebuild_rows();
        if let Some(name) = selected_name
            && let Some(at) = self
                .shown
                .iter()
                .position(|r| matches!(r, Row::Session(s) if s.name == name))
        {
            self.region.selected = at;
        }
        self.region = self.region.update(self.shown.len(), None).ensure_visible();
        self.last_refresh = Instant::now();
    }

    /// Apply the filter and sort. A running session outranks a stopped one,
    /// and a match on the name outranks a match on the command.
    ///
    /// node: the manager's fuzzy ranking
    fn rebuild_rows(&mut self) {
        let query = self.filter.text.trim();
        let mut scored: Vec<(i64, &SessionInfo)> = Vec::new();
        for s in &self.all {
            match row::score(s, query) {
                Some(score) => scored.push((score, s)),
                None => continue,
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
        self.shown = scored
            .into_iter()
            .map(|(_, s)| Row::Session(Box::new(s.clone())))
            .collect();
        self.shown.push(Row::Create);
    }

    fn selected_session_name(&self) -> Option<String> {
        match self.shown.get(self.region.selected) {
            Some(Row::Session(s)) => Some(s.name.clone()),
            _ => None,
        }
    }

    fn set_filter(&mut self, filter: TextFieldState) {
        let text_changed = filter.text != self.filter.text;
        self.filter = filter;
        if !text_changed {
            return;
        }
        self.rebuild_rows();
        self.region = self.region.update(self.shown.len(), None);
        self.region.selected = self.region.selected.min(self.shown.len().saturating_sub(1));
        self.region = self.region.ensure_visible();
    }
}

impl Screen<()> for Picker {
    fn render(&mut self, frame: &mut Frame<'_>, ctx: &RenderCtx) {
        let area = frame.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(area);
        self.render_list(frame, chunks[0], ctx);
        self.render_footer(frame, chunks[1], ctx);
    }

    fn handle_key(&mut self, key: &KeyEvent, app: &mut AppCtl<()>) {
        self.notice = None;
        match key.name.as_str() {
            "up" => self.region = self.region.scroll_up(),
            "down" => self.region = self.region.scroll_down(),
            "pageup" => self.region = self.region.page_up(),
            "pagedown" => self.region = self.region.page_down(),
            "return" | "enter" => self.activate(app),
            "escape" => {
                // Escape clears a filter first; a second one quits.
                if self.filter.is_empty() {
                    app.quit(0);
                } else {
                    self.set_filter(TextFieldState::empty());
                }
            }
            "c" if key.ctrl => app.quit(130),
            "g" if key.ctrl => self.cycle_theme(app),
            // `q` quits only when it would not be filter text.
            "q" if !key.ctrl && !key.alt && self.filter.is_empty() => app.quit(0),
            // Readline-style editing: ctrl+w, ctrl+u, ctrl+k, word motion, …
            _ => {
                if let Some(filter) = apply_text_key(&self.filter, key) {
                    self.set_filter(filter);
                }
            }
        }
        app.redraw();
    }

    fn on_tick(&mut self, app: &mut AppCtl<()>) {
        if app.is_paused() || self.last_refresh.elapsed() < REFRESH {
            return;
        }
        self.refresh();
        app.redraw();
    }
}

impl Picker {
    /// The panel: the filter line, then one line per row.
    fn render_list(&mut self, frame: &mut Frame<'_>, area: Rect, ctx: &RenderCtx) {
        let block = Block::bordered()
            .title("pty")
            .border_style(Style::default().fg(to_ratatui(ctx.theme.border)));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let viewport = inner.height.saturating_sub(2) as usize;
        self.region = self.region.update(self.shown.len(), Some(viewport.max(1)));
        self.region = self.region.ensure_visible();

        let mut lines: Vec<Line> = vec![self.filter_line(ctx), Line::from("")];
        let first = self.region.offset;
        for (offset, row) in self
            .shown
            .iter()
            .skip(first)
            .take(viewport)
            .enumerate()
        {
            let at = first + offset;
            let selected = at == self.region.selected;
            let marker = if selected { "▸ " } else { "  " };
            let (text, style) = match row {
                Row::Session(s) => (
                    row::describe(s),
                    Style::default().fg(if s.status == registry::SessionStatus::Running {
                        to_ratatui(ctx.theme.ok)
                    } else {
                        to_ratatui(ctx.theme.fg_mu)
                    }),
                ),
                Row::Create => (
                    "+ Create new session...".to_string(),
                    Style::default().fg(to_ratatui(ctx.theme.fg_ac)),
                ),
            };
            let style = if selected {
                style.add_modifier(Modifier::REVERSED)
            } else {
                style
            };
            lines.push(Line::from(vec![
                Span::styled(marker, Style::default().fg(to_ratatui(ctx.theme.fg_ac))),
                Span::styled(text, style),
            ]));
        }
        if let Some(notice) = &self.notice {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("  {notice}"),
                Style::default().fg(to_ratatui(ctx.theme.warn)),
            )));
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }

    /// `  Filter: <text>` with a block cursor, or the dim placeholder.
    fn filter_line(&self, ctx: &RenderCtx) -> Line<'static> {
        let mut spans = vec![Span::styled(
            "  Filter: ",
            Style::default().fg(to_ratatui(ctx.theme.fg_mu)),
        )];
        if self.filter.is_empty() {
            spans.push(Span::styled(
                "▏",
                Style::default().fg(to_ratatui(ctx.theme.fg_ac)),
            ));
            spans.push(Span::styled(
                "(type to filter)",
                Style::default()
                    .fg(to_ratatui(ctx.theme.fg_mu))
                    .add_modifier(Modifier::DIM),
            ));
        } else {
            spans.extend(render_field_spans(
                &self.filter.text,
                self.filter.cursor,
                true,
                Style::default().fg(to_ratatui(ctx.theme.fg1)),
            ));
        }
        Line::from(spans)
    }

    /// ctrl+g walks the built-in themes and remembers the choice.
    fn cycle_theme(&mut self, app: &mut AppCtl<()>) {
        let names = theme_names();
        let at = names.iter().position(|n| *n == self.theme_name).unwrap_or(0);
        let next = names[(at + 1) % names.len()];
        if let Some(theme) = theme_by_name(next) {
            self.theme = theme;
            self.theme_name = next.to_string();
            save_theme(next);
            app.set_theme(theme);
        }
    }

    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect, ctx: &RenderCtx) {
        let text = format!(
            "  ↑↓ select  ⏎ attach  ctrl+g theme ({})  q quit",
            self.theme_name
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                text,
                Style::default().fg(to_ratatui(ctx.theme.fg_mu)),
            ))),
            area,
        );
    }

    /// Return: attach to the selected session, restart it first if it has
    /// stopped, or create one from the last row.
    fn activate(&mut self, app: &mut AppCtl<()>) {
        match self.shown.get(self.region.selected) {
            Some(Row::Create) => self.create(app),
            Some(Row::Session(session)) => {
                let session = session.clone();
                if session.status != registry::SessionStatus::Running
                    && let Err(msg) = self.restart(&session)
                {
                    self.notice = Some(msg);
                    return;
                }
                self.attach(&session.name, app);
            }
            None => {}
        }
    }

    /// Start a stopped session again from what it was given the first time.
    fn restart(&self, session: &SessionInfo) -> Result<(), String> {
        let Some(meta) = session.metadata.clone() else {
            return Err(format!("{}: no metadata, cannot restart", session.name));
        };
        let _ = registry::cleanup_all(&session.name);
        let mut params = crate::cli::SpawnParams::new(&session.name, &meta.command, &meta.args);
        params.display_command = meta.display_command.clone();
        params.cwd = meta.cwd.clone();
        params.tags = registry::strip_gc_bookkeeping(meta.tags.as_ref()).unwrap_or_default();
        params.display_name = meta.display_name.clone();
        crate::cli::apply_persisted_launch_options(&mut params, &meta);
        params.scrub_env = crate::cli::RESTART_SCRUBBED_ENV
            .iter()
            .map(|s| s.to_string())
            .collect();
        crate::cli::spawn_daemon(&params).map_err(|e| format!("{}: {e}", session.name))
    }

    /// One keystroke makes a session: a random id, your shell, your home
    /// directory, and whatever `--filter-tag` asked the list to show — so
    /// the new session appears in the list that created it.
    fn create(&mut self, app: &mut AppCtl<()>) {
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/bash".to_string());
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
        let name = registry::generate_id();
        let mut params = crate::cli::SpawnParams::new(&name, &shell, &[]);
        params.cwd = home;
        params.tags = self.filter_tags.clone();
        if let Err(e) = crate::cli::spawn_daemon(&params) {
            self.notice = Some(format!("could not create a session: {e}"));
            return;
        }
        self.refresh();
        self.attach(&name, app);
    }

    /// Hand the terminal to the attach client, then take it back and show
    /// the list again with the filter and the selection where they were.
    fn attach(&mut self, name: &str, app: &mut AppCtl<()>) {
        app.pause();
        let code = crate::cli::attach::do_attach(name, None);
        // A session that ended while attached should not still be listed as
        // running when the list comes back.
        if code != 0 {
            std::thread::sleep(Duration::from_millis(200));
        }
        self.refresh();
        app.resume();
    }
}
