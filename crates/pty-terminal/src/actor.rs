//! The terminal actor: the one owner of a libghostty `Terminal`.
//!
//! [`TerminalActor`] is synchronous and lives on the thread that created it
//! (the `Terminal` is `!Send`). It has no thread, channel or process of its
//! own: bytes go in through [`TerminalActor::write`] and state comes out. The
//! daemon and the testkit run their loop on the thread that owns it; the `pty`
//! crate's `TerminalHandle` wraps one in an actor thread fed by a child it
//! spawned or by a daemon socket.
//!
//! Every call is ordered: when [`TerminalActor::write`] returns, every byte it
//! was given has been parsed, every query in it has been answered into
//! [`TerminalActor::take_pty_replies`], and every mode flag and event is
//! up to date. That is what makes a SCREEN cut an exact baseline.

use std::cell::RefCell;
use std::rc::Rc;

use libghostty_vt::style::RgbColor;
use libghostty_vt::terminal::{Options, Terminal};

use crate::graphics::{self, CellSize, GraphicsOptions, GraphicsState, ImageBytes};
use crate::input::{self, KeyEvent, MouseEvent};
use crate::queries;
use crate::screenshot::{self, Screenshot};
use crate::serialize::{self, SerializeOpts};
use crate::snapshot::{self, CellGrid};
use crate::strip::{OutputScanner, Osc, Token};

/// Node's scrollback (`src/server.ts:333-338`), in lines.
pub const DEFAULT_SCROLLBACK: usize = 10_000;

/// The most memory one terminal's history may be given, whatever scrollback
/// it was asked for: 64 MiB, which is 10 000 lines of a 400-column terminal.
///
/// A session that asks for more history than this gets what fits and says so
/// ([`TerminalActor::scrollback`] reports what is actually retainable, not
/// what was requested), because a silently unmet promise is what this whole
/// conversion exists to remove.
pub const MAX_SCROLLBACK_BYTES: usize = 64 * 1024 * 1024;

/// libghostty's `max_scrollback` is a **byte** budget for the history page
/// list, not a line count — its own doc comment says "lines", but the
/// behaviour is bytes and the page list evicts whole pages. Passing 10 000
/// for "10 000 lines" buys one page: 745 rows at 80 columns, 456 at 200,
/// 3 310 at 20. A line promise therefore has to be converted, and the cost
/// of a row depends on how wide it is.
///
/// These two numbers are the conversion, measured against libghostty-vt
/// 0.2.1: a plain row costs ~838 bytes at 80 columns and ~1 804 at 200, so
/// ~9-10 bytes per column plus per-row overhead. They are deliberately
/// generous — roughly 1.8x the measured cost — because the page list rounds
/// up to whole pages and because a row of styled or multi-codepoint cells
/// costs more than a plain one. Under-budgeting loses history; over-budgeting
/// costs address space that is only touched when the history actually fills.
const SCROLLBACK_BYTES_PER_COL: usize = 16;
/// Fixed per-row cost, independent of width. See
/// [`SCROLLBACK_BYTES_PER_COL`].
const SCROLLBACK_ROW_OVERHEAD: usize = 256;

/// What one row of a `cols`-wide terminal costs in the history.
fn scrollback_row_bytes(cols: u16) -> usize {
    SCROLLBACK_ROW_OVERHEAD + SCROLLBACK_BYTES_PER_COL * cols.max(1) as usize
}

/// The byte budget that retains `lines` lines of a `cols`-wide terminal,
/// capped at [`MAX_SCROLLBACK_BYTES`].
fn scrollback_budget(lines: usize, cols: u16) -> usize {
    lines
        .saturating_mul(scrollback_row_bytes(cols))
        .min(MAX_SCROLLBACK_BYTES)
}

/// A desktop notification the child asked for (OSC 9, 99, or 777).
/// Shapes follow Node (`src/server.ts:421-454`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// OSC 99 `title=`/`t=` or OSC 777's second field; OSC 9 has none.
    pub title: Option<String>,
    /// OSC 9's whole payload, OSC 99 `body=`/`b=`, OSC 777's remaining fields.
    pub body: Option<String>,
    /// `"osc9"`, `"osc99"`, or `"osc777"`.
    pub source: &'static str,
}

/// Something the child did that a session consumer wants to hear about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    /// BEL.
    Bell,
    /// OSC 0/2 set a *different* title than the last one reported
    /// (`src/server.ts:413-418` deduplicates).
    TitleChange(String),
    /// OSC 9 / 99 / 777.
    Notification(Notification),
    /// `CSI ? 1004 h` — the child asked for focus events.
    FocusRequest,
    /// `CSI ? 25 h` while the cursor was hidden.
    CursorVisible,
}

/// The mode flags Node tracks itself, from the child's own escape sequences
/// (`src/server.ts:343-391`), plus the kitty keyboard stack. These feed the
/// mode prefix that precedes every SCREEN (see [`crate::serialize`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Modes {
    /// `?1006`.
    pub sgr_mouse: bool,
    /// `?1000`.
    pub mouse_1000: bool,
    /// `?1002`.
    pub mouse_1002: bool,
    /// `?1003`.
    pub mouse_1003: bool,
    /// `?9` — X10 mouse reporting: button presses only, no releases, no
    /// motion, and no wheel. Deliberately *not* part of
    /// [`Modes::mouse_tracking`]: an input encoder has to treat it
    /// differently from `?1000`/`?1002`/`?1003`, which do report the wheel.
    pub mouse_9: bool,
    /// `?1` (DECCKM) — application cursor keys: the child wants `SS3 A` for
    /// Up, not `CSI A`. Node does not track it; an input encoder cannot
    /// encode an arrow key correctly without it.
    pub app_cursor: bool,
    /// `?1049` / `?1047` / `?47`.
    pub alt_screen: bool,
    /// `?25 l` seen and not yet undone by `?25 h`.
    pub cursor_hidden: bool,
    /// `?2004`.
    pub bracketed_paste: bool,
    /// `?1004`.
    pub focus_events: bool,
    /// `CSI > flags u` pushes, oldest first; `CSI < u` pops one.
    pub kitty_stack: Vec<u8>,
}

impl Modes {
    /// Any of the three tracking modes (Node's `mouseMode`).
    pub fn mouse_tracking(&self) -> bool {
        self.mouse_1000 || self.mouse_1002 || self.mouse_1003
    }

    /// Whether any mouse reporting at all is on, X10 included. Use
    /// [`Modes::mouse_tracking`] for "reports the wheel".
    pub fn mouse_reporting(&self) -> bool {
        self.mouse_9 || self.mouse_tracking()
    }

    fn apply_dec(&mut self, mode: u16, set: bool, events: &mut Vec<TerminalEvent>) {
        match mode {
            1006 => self.sgr_mouse = set,
            1000 => self.mouse_1000 = set,
            1002 => self.mouse_1002 = set,
            1003 => self.mouse_1003 = set,
            9 => self.mouse_9 = set,
            1 => self.app_cursor = set,
            1049 | 1047 | 47 => self.alt_screen = set,
            25 => {
                if set {
                    if self.cursor_hidden {
                        events.push(TerminalEvent::CursorVisible);
                    }
                    self.cursor_hidden = false;
                } else {
                    self.cursor_hidden = true;
                }
            }
            1004 => {
                self.focus_events = set;
                if set {
                    events.push(TerminalEvent::FocusRequest);
                }
            }
            2004 => self.bracketed_paste = set,
            _ => {}
        }
    }
}

/// Which rows a plain-text read covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    /// The active area only (rows `base_y..len`) — Node's default peek.
    Viewport,
    /// Scrollback and active area — Node's `peek --full`.
    Full,
}

#[derive(Default)]
struct Shared {
    pty_replies: Vec<u8>,
    bells: u32,
    titles: Vec<String>,
}

/// The owner of a libghostty terminal. See the [module docs](self).
pub struct TerminalActor {
    term: Terminal<'static, 'static>,
    shared: Rc<RefCell<Shared>>,
    scanner: OutputScanner,
    modes: Modes,
    events: Vec<TerminalEvent>,
    last_title: Option<String>,
    /// Lines of history the owner asked for.
    scrollback_request: usize,
    /// The byte budget libghostty was given for the history. Fixed at
    /// construction: libghostty takes it in `Options` and exposes no setter,
    /// so a resize changes how many lines it holds, not how much memory.
    scrollback_bytes: usize,
    /// How big a cell is on the surface that draws this terminal, zero when
    /// undeclared. It comes from the client (a font on its host), travels on
    /// ATTACH and RESIZE, and decides the cell extent of any placement that
    /// did not name `c=`/`r=` itself.
    cell: CellSize,
    /// Whether kitty graphics are on, and with what bounds. Off means the
    /// storage limit is zero, which is libghostty's own "protocol disabled".
    graphics: Option<GraphicsOptions>,
    /// The normal screen, serialized at the moment the child switched to the
    /// alternate one. libghostty gives no reader for the screen that is not
    /// active, and a replay has to carry both: a client that reconnects while
    /// a full-screen program runs must still have the normal buffer to fall
    /// back to when that program exits. Node gets it for free because
    /// xterm's serialize addon holds both buffers.
    normal_replay: Option<String>,
}

impl TerminalActor {
    /// A terminal of `rows` x `cols` with `scrollback` lines of history.
    ///
    /// The line count is converted to the byte budget libghostty actually
    /// takes (see [`MAX_SCROLLBACK_BYTES`]); [`TerminalActor::scrollback`]
    /// reports how many lines that buys at the current width.
    pub fn new(rows: u16, cols: u16, scrollback: usize) -> TerminalActor {
        let shared: Rc<RefCell<Shared>> = Rc::new(RefCell::new(Shared::default()));
        let scrollback_bytes = scrollback_budget(scrollback, cols);
        let mut term = Terminal::new(Options {
            cols: cols.max(1),
            rows: rows.max(1),
            max_scrollback: scrollback_bytes,
        })
        .expect("libghostty terminal");
        {
            let s = shared.clone();
            term.on_pty_write(move |_t, data| s.borrow_mut().pty_replies.extend_from_slice(data))
                .expect("install on_pty_write");
        }
        {
            let s = shared.clone();
            term.on_bell(move |_t| s.borrow_mut().bells += 1)
                .expect("install on_bell");
        }
        {
            let s = shared.clone();
            term.on_title_changed(move |t| {
                let title = t.title().unwrap_or("").to_string();
                s.borrow_mut().titles.push(title);
            })
            .expect("install on_title_changed");
        }
        queries::install(&mut term);
        TerminalActor {
            term,
            shared,
            scanner: OutputScanner::new(),
            modes: Modes::default(),
            events: Vec::new(),
            last_title: None,
            scrollback_request: scrollback,
            scrollback_bytes,
            cell: CellSize::default(),
            graphics: None,
            normal_replay: None,
        }
    }

    /// Node's defaults: 24 x 80, 10 000 lines of scrollback.
    pub fn with_defaults() -> TerminalActor {
        TerminalActor::new(24, 80, DEFAULT_SCROLLBACK)
    }

    /// Turn kitty graphics on: a bounded image storage, the cell metrics
    /// every placement question is answered in, and a PNG decoder for
    /// `f=100` transmissions (installed on this thread, where the terminal
    /// lives). Returns false when libghostty refused the options.
    ///
    /// `storage_bytes` is clamped to [`graphics::MAX_STORAGE_BYTES`] so
    /// everything the terminal accepts can also be replayed; read back what
    /// took effect with [`TerminalActor::graphics_options`]. A zero
    /// `opts.cell` leaves the metrics undeclared — see
    /// [`TerminalActor::set_cell_size`].
    ///
    /// Off by default: an owner that never reads images should not let a
    /// child make the terminal hold any.
    pub fn enable_graphics(&mut self, opts: GraphicsOptions) -> bool {
        let opts = GraphicsOptions {
            storage_bytes: opts.storage_bytes.min(graphics::MAX_STORAGE_BYTES),
            ..opts
        };
        let previous = self.graphics;
        if opts.cell.is_declared() {
            self.cell = opts.cell;
        }
        if self.apply_cell().is_err() {
            self.rollback_graphics(previous);
            return false;
        }
        if self.term.set_apc_max_bytes_kitty(opts.apc_max_bytes).is_err()
            || self
                .term
                .set_kitty_image_storage_limit(opts.storage_bytes)
                .is_err()
        {
            self.rollback_graphics(previous);
            return false;
        }
        // The child must not be able to name a path or a shared-memory
        // segment the owner never authorized: inline transmission only.
        let _ = self.term.set_kitty_image_from_file_allowed(false);
        let _ = self.term.set_kitty_image_from_temp_file_allowed(false);
        let _ = self.term.set_kitty_image_from_shared_mem_allowed(false);
        if !graphics::install_png_decoder() {
            self.rollback_graphics(previous);
            return false;
        }
        self.graphics = Some(GraphicsOptions {
            cell: self.cell,
            ..opts
        });
        true
    }

    /// Put the storage limit back where a failed [`TerminalActor::enable_graphics`]
    /// found it.
    ///
    /// Leaving a non-zero limit behind after a failure is the worst of both
    /// worlds: `self.graphics` is `None`, so a child can fill the storage
    /// with images that no read can reach and `clear_graphics` will not
    /// free.
    fn rollback_graphics(&mut self, previous: Option<GraphicsOptions>) {
        let limit = previous.map(|o| o.storage_bytes).unwrap_or(0);
        let _ = self.term.set_kitty_image_storage_limit(limit);
        self.graphics = previous;
    }

    /// Push the current grid and cell metrics into libghostty.
    fn apply_cell(&mut self) -> Result<(), libghostty_vt::error::Error> {
        let cell = self.cell.or_fallback();
        self.term
            .resize(self.cols().max(1), self.rows().max(1), cell.width, cell.height)
            .map(|_| ())
    }

    /// Declare how big a cell is on the surface that draws this terminal.
    ///
    /// Nothing else can know: the metrics come from a font on the client's
    /// host, which a session daemon may never see. They change no bytes —
    /// only derived geometry, which is why a placement that named `c=`/`r=`
    /// itself is unaffected and one that did not now gets a cell extent that
    /// matches what the client will actually draw.
    ///
    /// A zero width or height clears the declaration and returns the
    /// terminal to [`CellSize::FALLBACK`].
    pub fn set_cell_size(&mut self, cell: CellSize) {
        if self.cell == cell {
            return;
        }
        self.cell = cell;
        let _ = self.apply_cell();
        if let Some(opts) = &mut self.graphics {
            opts.cell = cell;
        }
    }

    /// The graphics options in effect, or `None` when graphics are off.
    pub fn graphics_options(&self) -> Option<GraphicsOptions> {
        self.graphics
    }

    /// The image-storage generation: 0 when graphics are off or nothing has
    /// ever been stored. Unchanged means the images and the set of placements
    /// are identical; geometry can still have moved.
    pub fn graphics_generation(&self) -> u64 {
        if self.graphics.is_none() {
            return 0;
        }
        graphics::generation(&self.term)
    }

    /// The images and placements for the window `scroll_offset` rows above
    /// the live viewport — the same window [`TerminalActor::snapshot`] reads,
    /// so cell positions in both line up.
    pub fn graphics_state(&self, scroll_offset: usize) -> GraphicsState {
        if self.graphics.is_none() {
            return GraphicsState::default();
        }
        graphics::read(&self.term, self.cell, scroll_offset)
    }

    /// The pixels of one image, copied out of the storage. `None` when it is
    /// not there (a delete won the race).
    pub fn image_bytes(&self, id: u32) -> Option<ImageBytes> {
        self.graphics.and_then(|_| graphics::image_bytes(&self.term, id))
    }

    /// Drop every image and placement, keeping the protocol on: what a pane
    /// does when it closes or is reused. Zeroing the limit is libghostty's
    /// own delete-everything path, so this needs no synthesized escape.
    pub fn clear_graphics(&mut self) {
        let Some(opts) = self.graphics else { return };
        let _ = self.term.set_kitty_image_storage_limit(0);
        let _ = self.term.set_kitty_image_storage_limit(opts.storage_bytes);
    }

    /// Raise or lower the storage limit, clamped to
    /// [`graphics::MAX_STORAGE_BYTES`] so everything stored can still be
    /// replayed. Zero turns the protocol off and deletes everything stored.
    ///
    /// A non-zero limit goes through [`TerminalActor::enable_graphics`], so
    /// there is one path that turns graphics on: raising the limit on an
    /// actor that never enabled them installs the PNG decoder and the cell
    /// metrics too, instead of leaving a terminal that stores images it
    /// cannot decode or measure.
    pub fn set_graphics_storage_limit(&mut self, bytes: u64) {
        if bytes == 0 {
            let _ = self.term.set_kitty_image_storage_limit(0);
            self.graphics = None;
            return;
        }
        let opts = self.graphics.unwrap_or(GraphicsOptions {
            cell: self.cell,
            ..GraphicsOptions::DEFAULT
        });
        self.enable_graphics(GraphicsOptions {
            storage_bytes: bytes,
            ..opts
        });
    }

    /// The declared cell metrics, or a zero size when nobody has declared
    /// them. [`CellSize::or_fallback`] gives what geometry actually used.
    pub fn cell_size(&self) -> CellSize {
        self.cell
    }

    /// The bytes the child expects for one key event, given the keyboard
    /// state it asked for (DECCKM, keypad, `modifyOtherKeys`, kitty
    /// keyboard flags). Empty for an event the child should not see.
    ///
    /// This is the only child key encoder: a consumer that owns a surface
    /// sends [`KeyEvent`]s, not bytes (see [`crate::input`]).
    pub fn encode_key(&self, ev: &KeyEvent) -> Vec<u8> {
        input::key(&self.term, ev)
    }

    /// The bytes for one mouse event, or `None` when the mode the child
    /// chose does not report it (no tracking, or a wheel notch under `?9`)
    /// and the surface keeps the event.
    pub fn encode_mouse(&self, ev: &MouseEvent) -> Option<Vec<u8>> {
        input::mouse(&self.term, &self.modes, ev, self.cell_size())
    }

    /// The bytes for a focus change, or `None` when the child did not ask
    /// for focus events.
    pub fn encode_focus(&self, gained: bool) -> Option<Vec<u8>> {
        input::focus(&self.modes, gained)
    }

    /// Pasted text, bracketed when the child asked for it.
    pub fn encode_paste(&self, text: &str) -> Vec<u8> {
        input::paste(&self.modes, text)
    }

    /// The normal screen as it was when the child entered the alternate
    /// one, or `None` when the normal screen is the active one.
    pub fn normal_replay(&self) -> Option<&str> {
        self.normal_replay.as_deref()
    }

    /// The underlying terminal, for reads this API does not cover.
    pub fn terminal(&self) -> &Terminal<'static, 'static> {
        &self.term
    }

    /// Feed the child's output. Returns the bytes to broadcast to attached
    /// clients: the input minus terminal queries (which are answered into
    /// [`TerminalActor::take_pty_replies`] instead). Mode flags, the kitty
    /// stack, and events are updated as a side effect.
    pub fn write(&mut self, data: &[u8]) -> Vec<u8> {
        let tokens = self.scanner.feed(data);
        let mut feed: Vec<u8> = Vec::with_capacity(data.len());
        let mut broadcast: Vec<u8> = Vec::with_capacity(data.len());
        for tok in tokens {
            match tok {
                Token::Raw(b) => {
                    feed.extend_from_slice(&b);
                    broadcast.extend_from_slice(&b);
                }
                Token::Ris => {
                    self.flush_feed(&mut feed);
                    if self.modes.cursor_hidden {
                        self.events.push(TerminalEvent::CursorVisible);
                    }
                    self.modes = Modes::default();
                    self.normal_replay = None;
                    feed.extend_from_slice(b"\x1bc");
                    broadcast.extend_from_slice(b"\x1bc");
                }
                Token::Csi(c) => {
                    if let Some(flags) = c.kitty_push() {
                        self.modes.kitty_stack.push(flags);
                    } else if c.is_kitty_pop() {
                        self.modes.kitty_stack.pop();
                    } else if let Some((params, set)) = c.dec_modes() {
                        let was_alt = self.modes.alt_screen;
                        for &p in params {
                            self.modes.apply_dec(p, set, &mut self.events);
                        }
                        match (was_alt, self.modes.alt_screen) {
                            // Everything written so far belongs to the normal
                            // screen. Put it in the terminal first, then take
                            // the copy, because the switch itself is still in
                            // `feed` and has not been applied.
                            (false, true) => {
                                self.flush_feed(&mut feed);
                                self.normal_replay =
                                    Some(crate::serialize::vt(&self.term, true, self.cell));
                            }
                            // Back on the normal screen: it serializes itself.
                            (true, false) => self.normal_replay = None,
                            _ => {}
                        }
                    }
                    feed.extend_from_slice(&c.raw);
                    if !c.is_stripped_query() {
                        broadcast.extend_from_slice(&c.raw);
                    }
                }
                Token::Osc(o) => {
                    if let Some((id, index)) = o.color_query() {
                        // Answer in stream order: everything before the query
                        // reaches the terminal (and may itself be answered)
                        // before this reply is queued.
                        self.flush_feed(&mut feed);
                        if let Some(reply) = queries::color_query_reply(id, index) {
                            self.shared.borrow_mut().pty_replies.extend_from_slice(&reply);
                        }
                        continue;
                    }
                    self.tap_notification(&o);
                    feed.extend_from_slice(&o.raw);
                    broadcast.extend_from_slice(&o.raw);
                }
            }
        }
        self.flush_feed(&mut feed);
        self.collect_callback_events();
        broadcast
    }

    fn flush_feed(&mut self, feed: &mut Vec<u8>) {
        if !feed.is_empty() {
            self.term.vt_write(feed);
            feed.clear();
        }
    }

    fn collect_callback_events(&mut self) {
        let (bells, titles) = {
            let mut s = self.shared.borrow_mut();
            (std::mem::take(&mut s.bells), std::mem::take(&mut s.titles))
        };
        for _ in 0..bells {
            self.events.push(TerminalEvent::Bell);
        }
        for title in titles {
            if self.last_title.as_deref() != Some(title.as_str()) {
                self.last_title = Some(title.clone());
                self.events.push(TerminalEvent::TitleChange(title));
            }
        }
    }

    /// OSC 9 / 99 / 777 → [`TerminalEvent::Notification`]
    /// (`src/server.ts:421-454`, field by field).
    fn tap_notification(&mut self, osc: &Osc) {
        let (id, data) = osc.split();
        let data = String::from_utf8_lossy(data).into_owned();
        let n = match id {
            Some(9) => Notification {
                title: None,
                body: Some(data),
                source: "osc9",
            },
            Some(99) => {
                let mut fields: Vec<(String, String)> = Vec::new();
                for part in data.split(';') {
                    if let Some(eq) = part.find('=') {
                        fields.push((part[..eq].to_string(), part[eq + 1..].to_string()));
                    }
                }
                let get = |k: &str| fields.iter().rev().find(|(fk, _)| fk == k).map(|(_, v)| v.clone());
                Notification {
                    title: get("title").or_else(|| get("t")),
                    body: get("body").or_else(|| get("b")),
                    source: "osc99",
                }
            }
            Some(777) => {
                let parts: Vec<&str> = data.split(';').collect();
                if parts.first() == Some(&"notify") && parts.len() >= 2 {
                    Notification {
                        title: Some(parts[1].to_string()),
                        body: Some(parts[2..].join(";")),
                        source: "osc777",
                    }
                } else {
                    return;
                }
            }
            _ => return,
        };
        self.events.push(TerminalEvent::Notification(n));
    }

    /// Resize the terminal (the primary screen reflows). The cell metrics
    /// stay as they are: a resize is a change of grid, not of font, and
    /// dropping them would make every placement's geometry unanswerable.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cell = self.cell.or_fallback();
        let _ = self
            .term
            .resize(cols.max(1), rows.max(1), cell.width, cell.height);
    }

    /// Full reset (RIS): screen, scrollback, modes, title. The tracked mode
    /// flags and any partial sequence in the scanner are cleared too. Used
    /// before replaying a SCREEN.
    pub fn reset(&mut self) {
        self.term.reset();
        self.scanner.reset();
        self.modes = Modes::default();
        self.shared.borrow_mut().titles.clear();
        self.shared.borrow_mut().bells = 0;
        // RIS restores libghostty's defaults, which include no image storage
        // and no cell metrics. An owner that asked for graphics keeps them
        // across the reset that precedes a SCREEN replay — otherwise the
        // replay's own images would be rejected.
        if let Some(opts) = self.graphics {
            self.enable_graphics(opts);
        }
    }

    /// The plain-text screen: rows right-trimmed of never-written cells
    /// (written spaces kept, like xterm's `translateToString(true)`), trailing
    /// empty rows dropped, joined by `\n`.
    pub fn plain(&self, range: Range) -> String {
        match range {
            Range::Viewport => serialize::plain_viewport(&self.term),
            Range::Full => serialize::plain_full(&self.term),
        }
    }

    /// The replay payload: Node's mode prefix followed by the VT
    /// serialization of the screen (cursor, modes, kitty keyboard).
    pub fn serialize(&self, opts: SerializeOpts) -> String {
        serialize::serialize_for_replay(self, opts)
    }

    /// The typed cell grid, `scroll_offset` rows back into history (0 = the
    /// live viewport).
    pub fn snapshot(&self, scroll_offset: usize) -> CellGrid {
        snapshot::snapshot(&self.term, scroll_offset)
    }

    /// The testkit's screenshot (all rows, VT with modes).
    pub fn screenshot(&self) -> Screenshot {
        screenshot::capture(&self.term)
    }

    /// The Node-tracked mode flags and kitty stack.
    pub fn modes(&self) -> Modes {
        self.modes.clone()
    }

    /// Bytes the terminal wants written back to the child (query answers).
    pub fn take_pty_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.shared.borrow_mut().pty_replies)
    }

    /// Events since the last call, in order.
    pub fn take_events(&mut self) -> Vec<TerminalEvent> {
        std::mem::take(&mut self.events)
    }

    /// `(x, y, visible)`: the cursor in the active area, 0-based. Like xterm,
    /// `x` equals `cols` when the cursor is pending a wrap after printing
    /// in the last column.
    pub fn cursor(&self) -> (u16, u16, bool) {
        let mut x = self.term.cursor_x().unwrap_or(0);
        if self.term.is_cursor_pending_wrap().unwrap_or(false) {
            x = x.saturating_add(1);
        }
        (
            x,
            self.term.cursor_y().unwrap_or(0),
            self.term.is_cursor_visible().unwrap_or(true),
        )
    }

    /// The window title (OSC 0/2), `""` when none was set.
    pub fn title(&self) -> String {
        self.term.title().unwrap_or("").to_string()
    }

    /// Node's `scrollbackUsed`: rows in the buffer, history and viewport
    /// (`src/server.ts:1128`, `buffer.active.length`).
    pub fn scrollback_used(&self) -> usize {
        self.buffer_length()
    }

    /// Node's `scrollbackCapacity`: `rows + scrollback`.
    ///
    /// A guaranteed minimum, not a ceiling. libghostty's history is a list of
    /// pages and it never holds less than one, so a terminal asked for a
    /// small scrollback retains more than it promised (a 100-line request at
    /// 80 columns keeps ~1 000 rows). Node's number is a ceiling because
    /// xterm counts lines; this one is a floor because libghostty counts
    /// bytes and rounds to pages
    /// (docs/decisions/0013-scrollback-is-a-line-promise.md).
    pub fn scrollback_capacity(&self) -> usize {
        self.rows() as usize + self.scrollback()
    }

    /// How many lines of history this terminal retains at its current width.
    ///
    /// Normally the line count the owner asked for. It is less when the byte
    /// budget cannot buy that many — either because the request exceeded
    /// [`MAX_SCROLLBACK_BYTES`], or because the terminal has since been
    /// widened and libghostty's budget is fixed at construction. Reporting
    /// the request in that case is what made the promise a lie.
    pub fn scrollback(&self) -> usize {
        let fits = self.scrollback_bytes / scrollback_row_bytes(self.cols());
        self.scrollback_request.min(fits)
    }

    /// Lines of history the owner asked for, whether or not they fit.
    pub fn scrollback_request(&self) -> usize {
        self.scrollback_request
    }

    /// The byte budget libghostty holds the history in.
    pub fn scrollback_bytes(&self) -> usize {
        self.scrollback_bytes
    }

    /// Node's `baseY`: the buffer row where the active area starts.
    pub fn base_y(&self) -> usize {
        self.term.scrollback_rows().unwrap_or(0)
    }

    /// Node's `bufferLength`: history rows + viewport rows.
    pub fn buffer_length(&self) -> usize {
        self.term.total_rows().unwrap_or(self.rows() as usize)
    }

    /// Terminal height.
    pub fn rows(&self) -> u16 {
        self.term.rows().unwrap_or(0)
    }

    /// Terminal width.
    pub fn cols(&self) -> u16 {
        self.term.cols().unwrap_or(0)
    }

    /// Whether libghostty's active screen is the alternate one (its own
    /// view of `?1049`/`?1047`/`?47`; [`Modes::alt_screen`] is Node's).
    pub fn alt_screen_active(&self) -> bool {
        matches!(
            self.term.active_screen(),
            Ok(libghostty_vt::screen::Screen::Alternate)
        )
    }

    /// The kitty keyboard flags currently in effect (libghostty's value; the
    /// push/pop history is [`Modes::kitty_stack`]).
    pub fn kitty_flags(&self) -> u8 {
        self.term.kitty_keyboard_flags().map(|f| f.bits()).unwrap_or(0)
    }

    /// Override palette entries `0..colors.len()` (a theme). Cells keep
    /// their palette index; only what an RGB read resolves to changes.
    pub fn set_palette(&mut self, colors: &[(u8, u8, u8)]) {
        let mut palette = match self.term.default_color_palette() {
            Ok(p) => p,
            Err(_) => return,
        };
        for (i, &(r, g, b)) in colors.iter().take(256).enumerate() {
            palette.0[i] = RgbColor { r, g, b };
        }
        let _ = self.term.set_default_color_palette(Some(palette));
    }
}

impl std::fmt::Debug for TerminalActor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalActor")
            .field("rows", &self.rows())
            .field("cols", &self.cols())
            .field("modes", &self.modes)
            .finish()
    }
}
