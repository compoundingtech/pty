//! # pty-core
//!
//! The terminal-free half of the Rust
//! [pty](https://github.com/compoundingtech/pty-original-experiment) port: the
//! wire protocol, the on-disk session registry, the client operations (peek /
//! send / status / attach), and the pure-logic modules ported from the Node
//! project ([`keys`], [`paste`], [`duration`], [`input`], [`queries`],
//! [`ptyfile`]).
//!
//! This crate deliberately does not depend on libghostty, so it builds without
//! a Zig toolchain. Terminal emulation lives in `pty-terminal`; the daemon and
//! CLI live in the `pty` binary crate.
//!
//! # Lock compatibility boundary
//!
//! Rust publishes complete owner records atomically and makes stale stealing
//! exclusive between Rust contenders. Node retains an empty-publication window
//! and an unbound stale read-then-unlink. Rust respects a live, complete Node
//! lock, but a delayed Node stale contender can unlink a newer Rust or Node
//! claim. [`registry::lock`] describes the mixed-registry boundary.

pub mod client;
pub mod duration;
pub mod events;
pub mod input;
pub mod keys;
pub mod paste;
pub mod proctable;
pub mod protocol;
pub mod ptyfile;
pub mod queries;
pub mod registry;
pub mod spawn;
pub mod stats;
mod unix_connect;
pub mod unix_peer;

pub use client::query_stats_batch_in;
#[doc(hidden)]
pub use unix_connect::busy_connects_on_this_thread;
