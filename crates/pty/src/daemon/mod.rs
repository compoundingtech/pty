//! The per-session daemon: owns the PTY and a libghostty terminal actor,
//! serves the wire protocol over `<root>/<name>.sock`, publishes the
//! session's files, and records its exit — Node's `server.ts` with the
//! same ordering guarantees.
//!
//! - `pty-lifecycle` owns daemon launch and the shared start-up config.
//! - [`daemon_main`] implements the private `__daemon` entrypoint.
//! - [`env`], [`lifecycle`]: the child's environment, actor loop, publication,
//!   exit, and shutdown.
//! - [`clients`], [`geometry`], [`status`], [`events`]: the packet handlers,
//!   effective geometry, STATUS, and the events log.
//! - [`tree`]: descendant termination on an external kill.

pub mod clients;
pub mod env;
pub mod events;
pub mod geometry;
pub mod lifecycle;
mod ownership;
pub mod status;

use std::path::PathBuf;

pub use pty_lifecycle::{DaemonConfig, ReadyNotifier, set_process_title};

/// Write a diagnostic line to stderr and carry on if nobody is listening.
///
/// The CLI that launches a daemon pipes its stderr and reads it only until
/// the session is published, so that it can report a daemon that died on the
/// way up. After the CLI exits, the read end is gone and every write returns
/// `EPIPE`. `eprintln!` panics on that, which killed whichever daemon thread
/// happened to be reporting something — including the reader thread that
/// drops a client for sending an oversized frame. Node's daemon ignores the
/// same error, so this does too.
macro_rules! daemon_warn {
    ($($arg:tt)*) => {{
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}
pub(crate) use daemon_warn;

/// The `pty __daemon` entry: title, config from fd 3 (or
/// `PTY_SERVER_CONFIG`), then the daemon. Returns the process exit status.
///
/// node: src/server.ts:1458-1478
pub fn daemon_main() -> i32 {
    set_process_title("pty-daemon");
    let cfg = match DaemonConfig::from_process() {
        Ok(cfg) => cfg,
        Err(msg) => {
            eprintln!("{msg}");
            return 1;
        }
    };
    let readiness = ReadyNotifier::from_process();
    match lifecycle::run(cfg, readiness) {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("{msg}");
            1
        }
    }
}

/// The default session working directory when none is given.
pub fn default_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}
