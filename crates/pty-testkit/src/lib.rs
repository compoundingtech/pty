//! # pty-testkit
//!
//! A Rust port of the [pty](https://github.com/compoundingtech/pty-original-experiment)
//! project's Playwright-style TUI testing library, using **libghostty** (the
//! Ghostty terminal library, via [`libghostty-vt`](https://docs.rs/libghostty-vt))
//! as the terminal-emulation backend in place of `@xterm/headless`.
//!
//! The core type is [`Session`]: spawn a process in a real PTY, feed its output
//! into a libghostty terminal, take text/ANSI "screenshots", wait for content,
//! and send named keys.
//!
//! Key names come from `pty_core::keys`; screenshots come from
//! `pty_terminal::screenshot`. Both are re-exported here only as far as the
//! `Session` API needs them.

pub mod server;
pub mod session;

pub use pty_terminal::Screenshot;
pub use session::{ServerOptions, Session, SpawnOptions, build_spawn_env};
