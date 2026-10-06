//! # pty
//!
//! The library side of the `pty` crate, next to the per-session daemon the
//! binary runs: [`TerminalHandle`], a live terminal that owns what feeds it —
//! a child it spawned in a PTY, or a socket to a session daemon — on its own
//! actor thread.
//!
//! The terminal state itself is `pty-terminal`'s: bytes in, libghostty screen,
//! cells, cursor, modes, graphics, input encoding and serialization out. That
//! crate spawns nothing and owns no process. This handle, like the daemon,
//! owns the child or the socket and feeds the terminal from it.
//!
//! - [`handle`]: [`TerminalHandle`], `Send + Sync`, either spawning a child
//!   ([`TerminalHandle::spawn`]) or attaching to a session daemon
//!   ([`TerminalHandle::attach`]). It is also a `pty_tui::LiveTerminal`, so a
//!   `pty_tui::PtyPane` can draw it.

pub mod handle;

pub use handle::{
    AttachOptions, AttemptId, Frame, FrameObserver, HandleEvent, ReadyOutcome, SessionRef,
    SpawnOptions, TerminalHandle,
};
