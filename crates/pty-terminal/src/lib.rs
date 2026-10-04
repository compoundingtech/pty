//! # pty-terminal
//!
//! The libghostty-backed terminal side of the Rust
//! [pty](https://github.com/compoundingtech/pty-original-experiment) port: one
//! owner of the `!Send` `libghostty_vt::terminal::Terminal`, typed reads,
//! Node-equivalent serialization, terminal query answers, and terminal events.
//!
//! **Bytes in, terminal state out, and nothing else.** This crate spawns no
//! process, opens no PTY or socket, and starts no thread. Whatever feeds a
//! terminal — the session daemon, the `pty` crate's `TerminalHandle`, the
//! testkit's `Session` — owns the process and calls in here. Its only
//! dependencies are libghostty and a PNG decoder.
//!
//! - [`actor`]: [`TerminalActor`], the synchronous owner of the terminal. Feed
//!   it the child's output with [`TerminalActor::write`]; read the screen with
//!   [`TerminalActor::plain`], [`TerminalActor::serialize`], and
//!   [`TerminalActor::snapshot`]; drain query answers with
//!   [`TerminalActor::take_pty_replies`] and events with
//!   [`TerminalActor::take_events`].
//! - [`strip`]: the streaming scanner that keeps terminal queries out of the
//!   DATA broadcast and tracks the mode flags Node tracks.
//! - [`queries`]: the exact answers Node gives to DA1/DA2/DSR/XTVERSION and the
//!   OSC 10/11/4 colour queries.
//! - [`serialize`]: the ATTACH/PEEK replay (Node mode prefix + VT) and the
//!   plain-text screen (viewport or full scrollback).
//! - [`snapshot`]: [`CellGrid`], the typed cell grid for renderers.
//! - [`graphics`]: the kitty graphics state — bounded image bytes,
//!   placements, source crops, and where each placement sits in the window
//!   that was read — plus the replay block that carries it through
//!   ATTACH/PEEK.
//! - [`input`]: the child input encoder — keys (kitty keyboard included),
//!   mouse, focus, and paste, encoded from the terminal's own state so no
//!   consumer needs a second encoder.
//! - [`screenshot`]: the testkit's [`Screenshot`] capture.

pub mod actor;
pub mod graphics;
pub mod input;
pub mod history;
pub mod queries;
pub mod screenshot;
pub mod serialize;
pub mod snapshot;
pub mod strip;

pub use actor::{Modes, Notification, Range, TerminalActor, TerminalEvent};
pub use graphics::{
    CellSize, Compression, GraphicsOptions, GraphicsState, ImageBytes, ImageDesc, PixelFormat,
    PlaceholderRect, Placement, PlacementPosition, SourceRect,
};
pub use input::{Key, KeyAction, KeyEvent, KittyKeyFlags, Mods, MouseAction, MouseButton, MouseEvent};
pub use screenshot::{Screenshot, capture, serialize_for_replay};
pub use serialize::SerializeOpts;
pub use snapshot::{CellGrid, CellSnap, ColorSnap, Wide};
