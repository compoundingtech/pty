//! The application runner, ported from `src/tui/app.ts`.
//!
//! [`App::run`] enters the terminal (alternate screen, raw mode, hidden
//! cursor, optional SGR mouse, bracketed paste and kitty disambiguation),
//! reads stdin on its own thread through [`parse_input`], and drives a
//! [`Screen`] from one channel of [`AppEvent`]s: keys, mouse, a 1 s tick,
//! `SIGWINCH`, `Dirty` from embedded handles and the host's own messages.
//! Every frame is drawn with ratatui inside DEC synchronized output.
//!
//! [`AppCtl::pause`] leaves the terminal completely and stops reading
//! stdin so an in-process `pty_core::client::attach` can own the tty;
//! [`AppCtl::resume`] re-enters and forces a full redraw. The default
//! `ctrl+c` ends the run with exit code 130 (`app.ts:208-211`) unless the
//! screen's [`Screen::global_key`] consumes it first.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    disable_raw_mode, enable_raw_mode,
};
use crossterm::{cursor, execute};
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::input::{InputEvent, KeyEvent, MOUSE_DISABLE_SGR, MOUSE_ENABLE_SGR, MouseEvent, parse_input};
use crate::theme::{BoxStyle, Theme};

/// Everything the loop reacts to. `M` is the host's own message type.
#[derive(Debug)]
pub enum AppEvent<M> {
    Key(KeyEvent),
    Mouse(MouseEvent),
    /// The periodic tick ([`AppConfig::tick`]).
    Tick,
    /// The terminal was resized (`cols`, `rows`).
    Resize(u16, u16),
    /// An embedded handle changed; redraw.
    Dirty,
    /// A host message.
    Message(M),
}

/// `AppConfig` (`app.ts:18-39`).
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// Default `coolBlue` (`app.ts:74`).
    pub theme: Theme,
    /// Default rounded (`app.ts:78`).
    pub box_style: BoxStyle,
    /// Enable SGR mouse reporting (`?1002h ?1006h`).
    pub mouse: bool,
    /// Enable bracketed paste (`?2004h`).
    pub bracketed_paste: bool,
    /// Push the kitty `DISAMBIGUATE_ESCAPE_CODES` flag so Esc and modified
    /// keys arrive as CSI-u.
    pub kitty: bool,
    /// The tick interval; `None` disables ticks. Default 1 s.
    pub tick: Option<Duration>,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            theme: crate::theme::COOL_BLUE,
            box_style: BoxStyle::Rounded,
            mouse: false,
            bracketed_paste: true,
            kitty: true,
            tick: Some(Duration::from_secs(1)),
        }
    }
}

/// What a frame is rendered with (`ScreenContext`, `types.ts:55-77`).
#[derive(Debug, Clone, Copy)]
pub struct RenderCtx {
    pub theme: Theme,
    pub box_style: BoxStyle,
    pub rows: u16,
    pub cols: u16,
}

/// A screen (`Screen`, `types.ts:80-90`): pure render plus event handlers.
pub trait Screen<M> {
    /// Draw the frame. Overlays are drawn here too, after the base screen,
    /// with [`ratatui::widgets::Clear`] (see [`crate::widgets::overlay`]).
    fn render(&mut self, frame: &mut Frame<'_>, ctx: &RenderCtx);

    /// A key, after [`Screen::global_key`] and the default `ctrl+c`.
    fn handle_key(&mut self, key: &KeyEvent, app: &mut AppCtl<M>);

    /// A mouse event (only with [`AppConfig::mouse`]).
    fn handle_mouse(&mut self, _event: &MouseEvent, _app: &mut AppCtl<M>) {}

    /// The global interceptor (`AppConfig.onKey`): return true to swallow
    /// the key before the default `ctrl+c` and [`Screen::handle_key`].
    fn global_key(&mut self, _key: &KeyEvent, _app: &mut AppCtl<M>) -> bool {
        false
    }

    /// The periodic tick.
    fn on_tick(&mut self, _app: &mut AppCtl<M>) {}

    /// A host message.
    fn on_message(&mut self, _msg: M, _app: &mut AppCtl<M>) {}

    /// The terminal was resized.
    fn on_resize(&mut self, _cols: u16, _rows: u16, _app: &mut AppCtl<M>) {}
}

struct ReaderShared {
    paused: Mutex<bool>,
    cv: Condvar,
    stop: AtomicBool,
}

static WINCH: AtomicBool = AtomicBool::new(false);

extern "C" fn on_winch(_: libc::c_int) {
    WINCH.store(true, Ordering::SeqCst);
}

fn install_winch() {
    // SAFETY: installing a handler that only stores an atomic flag.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_winch as extern "C" fn(libc::c_int) as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGWINCH, &sa, std::ptr::null_mut());
    }
}

fn reader_thread<M: Send + 'static>(
    shared: Arc<ReaderShared>,
    tx: Sender<AppEvent<M>>,
    stdin_fd: i32,
) {
    let mut buf = [0u8; 4096];
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        {
            let mut paused = shared.paused.lock().unwrap();
            while *paused {
                paused = shared.cv.wait(paused).unwrap();
                if shared.stop.load(Ordering::SeqCst) {
                    return;
                }
            }
        }
        let mut fds = [libc::pollfd {
            fd: stdin_fd,
            events: libc::POLLIN,
            revents: 0,
        }];
        // SAFETY: a valid pollfd array of length 1.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, 100) };
        if WINCH.swap(false, Ordering::SeqCst)
            && let Some((cols, rows)) = size_of(libc::STDOUT_FILENO)
            && tx.send(AppEvent::Resize(cols, rows)).is_err()
        {
            return;
        }
        if n <= 0 {
            continue;
        }
        if fds[0].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
            && fds[0].revents & libc::POLLIN == 0
        {
            // stdin is gone; nothing more to read.
            return;
        }
        if *shared.paused.lock().unwrap() {
            continue;
        }
        // SAFETY: reading into a stack buffer of the given length.
        let n = unsafe { libc::read(stdin_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            if n == 0 {
                return;
            }
            continue;
        }
        for ev in parse_input(&buf[..n as usize]) {
            let ev = match ev {
                InputEvent::Key(k) => AppEvent::Key(k),
                InputEvent::Mouse(m) => AppEvent::Mouse(m),
            };
            if tx.send(ev).is_err() {
                return;
            }
        }
    }
}

fn size_of(fd: i32) -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ writes a winsize.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    (rc == 0 && ws.ws_col > 0 && ws.ws_row > 0).then_some((ws.ws_col, ws.ws_row))
}

type Term = ratatui::Terminal<CrosstermBackend<io::Stdout>>;

/// The handle a screen drives the app through: quit, pause/resume, theme,
/// and a sender for background work.
pub struct AppCtl<M> {
    config: AppConfig,
    tx: Sender<AppEvent<M>>,
    reader: Arc<ReaderShared>,
    term: Option<Term>,
    paused: bool,
    exit: Option<i32>,
    /// Draw a frame after the current batch of events.
    dirty: bool,
}

impl<M> AppCtl<M> {
    /// End the run with this exit code (`ctx.quit()`, `app.ts:97-100`).
    pub fn quit(&mut self, code: i32) {
        self.exit = Some(code);
    }

    /// Has [`AppCtl::quit`] been called?
    pub fn quitting(&self) -> bool {
        self.exit.is_some()
    }

    /// A sender for background threads (relay refreshes, handle watchers).
    pub fn sender(&self) -> Sender<AppEvent<M>> {
        self.tx.clone()
    }

    pub fn theme(&self) -> Theme {
        self.config.theme
    }

    pub fn set_theme(&mut self, theme: Theme) {
        self.config.theme = theme;
        self.dirty = true;
    }

    pub fn box_style(&self) -> BoxStyle {
        self.config.box_style
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Ask for a redraw.
    pub fn redraw(&mut self) {
        self.dirty = true;
    }

    /// The terminal size (`cols`, `rows`), 120x35 when unknown (`app.ts:70`).
    pub fn size(&self) -> (u16, u16) {
        size_of(libc::STDOUT_FILENO).unwrap_or((120, 35))
    }

    /// Leave the terminal and stop reading stdin so another in-process
    /// client can use them (`app.pause()`, `app.ts:288-293`). No-op while
    /// paused.
    pub fn pause(&mut self) {
        if self.paused {
            return;
        }
        self.paused = true;
        {
            let mut p = self.reader.paused.lock().unwrap();
            *p = true;
        }
        self.reader.cv.notify_all();
        // Give the reader time to observe the flag before the tty changes hands.
        thread::sleep(Duration::from_millis(5));
        self.term = None;
        let _ = leave_terminal(&self.config, false);
    }

    /// Re-enter the terminal with a full redraw (`app.resume()`,
    /// `app.ts:295-302`).
    pub fn resume(&mut self) {
        if !self.paused {
            return;
        }
        self.paused = false;
        let _ = enter_terminal(&self.config);
        install_winch();
        self.term = ratatui::Terminal::new(CrosstermBackend::new(io::stdout())).ok();
        if let Some(t) = self.term.as_mut() {
            let _ = t.clear();
        }
        {
            let mut p = self.reader.paused.lock().unwrap();
            *p = false;
        }
        self.reader.cv.notify_all();
        self.dirty = true;
    }

    fn draw<S: Screen<M>>(&mut self, screen: &mut S) -> io::Result<()> {
        let Some(term) = self.term.as_mut() else {
            return Ok(());
        };
        let ctx = RenderCtx {
            theme: self.config.theme,
            box_style: self.config.box_style,
            rows: 0,
            cols: 0,
        };
        let mut out = io::stdout();
        let _ = execute!(out, BeginSynchronizedUpdate);
        let r = term.draw(|frame| {
            let area = frame.area();
            let ctx = RenderCtx {
                rows: area.height,
                cols: area.width,
                ..ctx
            };
            // The screen background (`screen.ts:116`).
            Background(ctx.theme).render(area, frame.buffer_mut());
            screen.render(frame, &ctx);
        });
        let _ = execute!(out, EndSynchronizedUpdate);
        r.map(|_| ())
    }
}

/// Fills an area with the theme's `bg1` (and `fg1`).
struct Background(Theme);

impl Widget for Background {
    fn render(self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        let style = ratatui::style::Style::default()
            .fg(self.0.resolve(Some(crate::theme::Color::Primary)))
            .bg(crate::theme::to_ratatui(self.0.bg1));
        buf.set_style(area, style);
    }
}

fn enter_terminal(config: &AppConfig) -> io::Result<()> {
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, cursor::Hide)?;
    if config.mouse {
        out.write_all(MOUSE_ENABLE_SGR.as_bytes())?;
    }
    if config.bracketed_paste {
        execute!(out, EnableBracketedPaste)?;
    }
    if config.kitty {
        let _ = execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    enable_raw_mode()?;
    out.flush()
}

fn leave_terminal(config: &AppConfig, full: bool) -> io::Result<()> {
    let mut out = io::stdout();
    if config.kitty {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    if config.bracketed_paste {
        execute!(out, DisableBracketedPaste)?;
    }
    if config.mouse {
        out.write_all(MOUSE_DISABLE_SGR.as_bytes())?;
    }
    if full {
        execute!(out, cursor::Show, crossterm::style::ResetColor, LeaveAlternateScreen)?;
    } else {
        execute!(out, cursor::Show, LeaveAlternateScreen)?;
    }
    disable_raw_mode()?;
    out.flush()
}

/// The runner.
pub struct App;

impl App {
    /// Run `screen` until it quits. Returns the exit code: the one passed
    /// to [`AppCtl::quit`], or 130 for the default `ctrl+c`.
    pub fn run<M: Send + 'static, S: Screen<M>>(config: AppConfig, screen: &mut S) -> io::Result<i32> {
        let (tx, rx) = mpsc::channel::<AppEvent<M>>();
        let reader = Arc::new(ReaderShared {
            paused: Mutex::new(false),
            cv: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        enter_terminal(&config)?;
        install_winch();
        let term = ratatui::Terminal::new(CrosstermBackend::new(io::stdout()))?;
        let mut ctl = AppCtl {
            config,
            tx: tx.clone(),
            reader: reader.clone(),
            term: Some(term),
            paused: false,
            exit: None,
            dirty: true,
        };
        let reader_handle = {
            let shared = reader.clone();
            let tx = tx.clone();
            thread::spawn(move || reader_thread(shared, tx, libc::STDIN_FILENO))
        };
        let result = Self::event_loop(&mut ctl, &rx, screen);
        reader.stop.store(true, Ordering::SeqCst);
        reader.cv.notify_all();
        ctl.term = None;
        if !ctl.paused {
            let _ = leave_terminal(&ctl.config, true);
        } else {
            let _ = execute!(io::stdout(), crossterm::style::ResetColor);
        }
        drop(reader_handle);
        result
    }

    fn event_loop<M: Send + 'static, S: Screen<M>>(
        ctl: &mut AppCtl<M>,
        rx: &Receiver<AppEvent<M>>,
        screen: &mut S,
    ) -> io::Result<i32> {
        let mut next_tick = ctl.config.tick.map(|t| Instant::now() + t);
        loop {
            if ctl.dirty && !ctl.paused {
                ctl.dirty = false;
                ctl.draw(screen)?;
            }
            if let Some(code) = ctl.exit {
                return Ok(code);
            }
            let ev = match next_tick {
                Some(at) => {
                    let now = Instant::now();
                    if now >= at {
                        next_tick = ctl.config.tick.map(|t| at + t);
                        Some(AppEvent::Tick)
                    } else {
                        match rx.recv_timeout(at - now) {
                            Ok(ev) => Some(ev),
                            Err(RecvTimeoutError::Timeout) => continue,
                            Err(RecvTimeoutError::Disconnected) => return Ok(ctl.exit.unwrap_or(0)),
                        }
                    }
                }
                None => match rx.recv() {
                    Ok(ev) => Some(ev),
                    Err(_) => return Ok(ctl.exit.unwrap_or(0)),
                },
            };
            let Some(ev) = ev else { continue };
            Self::dispatch(ctl, screen, ev);
            // Drain what else is queued before drawing once.
            while let Ok(ev) = rx.try_recv() {
                Self::dispatch(ctl, screen, ev);
                if ctl.exit.is_some() {
                    break;
                }
            }
            ctl.dirty = true;
        }
    }

    fn dispatch<M: Send + 'static, S: Screen<M>>(ctl: &mut AppCtl<M>, screen: &mut S, ev: AppEvent<M>) {
        match ev {
            AppEvent::Key(key) => {
                if screen.global_key(&key, ctl) {
                    return;
                }
                if key.name == "c" && key.ctrl {
                    ctl.quit(130);
                    return;
                }
                screen.handle_key(&key, ctl);
            }
            AppEvent::Mouse(m) => screen.handle_mouse(&m, ctl),
            AppEvent::Tick => screen.on_tick(ctl),
            AppEvent::Resize(cols, rows) => {
                if let Some(t) = ctl.term.as_mut() {
                    let _ = t.autoresize();
                    let _ = t.clear();
                }
                screen.on_resize(cols, rows, ctl);
            }
            AppEvent::Dirty => {}
            AppEvent::Message(m) => screen.on_message(m, ctl),
        }
    }

    /// Forward a handle's events — the receiver its `subscribe` returns — to
    /// the app as [`AppEvent::Dirty`] until the handle closes.
    pub fn watch_handle<E: Send + 'static, M: Send + 'static>(
        rx: Receiver<E>,
        tx: Sender<AppEvent<M>>,
    ) {
        thread::spawn(move || {
            while rx.recv().is_ok() {
                if tx.send(AppEvent::Dirty).is_err() {
                    return;
                }
            }
        });
    }
}
