//! Allocate a pseudo-terminal and start a child process in it.
//!
//! **This crate is deliberately small, and deliberately free of the terminal
//! emulator.** It uses `portable-pty` and the emulator-free `pty-core` registry
//! helpers, so it builds with plain `cargo` on supported platforms.
//!
//! That matters more than it looks. The rest of this workspace builds
//! `libghostty` from source with Zig, pinned at 0.15.2, and that Zig cannot
//! link the current macOS SDK — so a consumer that only wants a pty must not be
//! made to pay for a terminal emulator it will never ask a question of. Adding
//! any dependency here that pulls Zig in makes this crate unusable for its one
//! purpose. See `docs/parity.md` and the README build requirements.
//!
//! Three places in this workspace opened a pty independently before this crate
//! existed, with three different behaviours when it failed: a descriptive
//! error, a bare `io::Error`, and a panic. They now share one.

use portable_pty::native_pty_system;
use std::io;

/// Re-exported so a consumer needs no direct `portable-pty` dependency to name
/// the types this crate hands back, or to do the thing it will inevitably do
/// next: `PtySize` is what `MasterPty::resize` takes, and every caller that
/// opens a pty eventually resizes one.
pub use portable_pty::{Child, CommandBuilder, MasterPty, PtyPair, PtySize, SlavePty};

/// Open a pty of the given size.
///
/// The pixel dimensions are zero, as every caller in this workspace has always
/// passed them: the size that matters to a child is the character grid, and
/// nothing here reports pixel geometry.
///
/// # Errors
///
/// Returns the underlying failure with its cause attached. A caller that knows
/// *which* session it was opening for should add that context itself rather
/// than have this crate guess at it.
pub fn open(rows: u16, cols: u16) -> io::Result<PtyPair> {
    native_pty_system()
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| io::Error::other(format!("could not open a pty ({rows}x{cols}): {e}")))
}

/// A command that runs `command` through `/bin/sh`, so a PATH lookup, a
/// shebang, and a symlinked executable all behave the way they would if a
/// person had typed it.
///
/// The exact form is `/bin/sh -c 'exec "$@"' sh <command> <args...>`. The
/// `exec` matters: without it the shell stays alive as the child's parent and
/// every signal, exit code and process-group operation addresses the wrapper
/// instead of the program. Passing the arguments after `sh` rather than
/// interpolating them into the script matters too, because it keeps argument
/// boundaries literal — an argument containing a space or a shell
/// metacharacter reaches the program unchanged.
///
/// This mirrors the Node implementation, which is the behaviour reference.
pub fn shell_exec(command: &str, args: &[String]) -> CommandBuilder {
    let mut cmd = CommandBuilder::new("/bin/sh");
    cmd.args(["-c", "exec \"$@\"", "sh"]);
    cmd.arg(command);
    cmd.args(args);
    cmd
}
/// Typed two-plane ownership boundary for externally-owned PTYs.
pub mod substrate;

pub use substrate::{
    AttachStream, DEFAULT_ATTACHMENT_CAPACITY, EXIT_OUTPUT_DRAIN, ExitStatus, Lifecycle,
    MIN_ATTACHMENT_CAPACITY, ReapFence, SessionClient, SessionError, SessionEvent, SessionOwner,
    SessionRef, external_owned, external_owned_master_attached, external_owned_pair,
    external_owned_pair_attached, external_owned_pair_attached_with_capacity,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_gives_a_usable_pair() {
        let pair = open(24, 80).expect("open a pty");
        // Dropping the slave is what every caller does once the child holds it;
        // proving the pair is well formed here is enough for this crate.
        drop(pair.slave);
        drop(pair.master);
    }

    #[test]
    fn a_zero_sized_pty_is_still_openable() {
        // Callers pass 0 when they do not know the size yet, and the kernel
        // accepts it. This is a real path, not a curiosity.
        let pair = open(0, 0).expect("open a zero-sized pty");
        drop(pair.slave);
        drop(pair.master);
    }

    #[test]
    fn shell_exec_keeps_argument_boundaries_literal() {
        // The argv is the contract: /bin/sh -c 'exec "$@"' sh <cmd> <args...>.
        // An argument with a space must stay ONE argument.
        let cmd = shell_exec("echo", &["a b".to_string(), "c;d".to_string()]);
        let argv: Vec<String> =
            cmd.get_argv().iter().map(|s| s.to_string_lossy().into_owned()).collect();
        assert_eq!(argv, vec!["/bin/sh", "-c", "exec \"$@\"", "sh", "echo", "a b", "c;d"]);
    }

    /// **Read the master before waiting for the child, and never the other way
    /// round.**
    ///
    /// An earlier version of this test called `child.wait()` first and then
    /// read. That passes on Linux and DEADLOCKS on macOS, where closing the
    /// slave waits for unread terminal output before the child becomes
    /// waitable: the child cannot finish because nobody has drained it, and
    /// nobody drains it because the test is blocked in `wait4`. Measured on
    /// macOS 26.6 arm64, 2026-09-06 — the main thread sat in `wait4` with the
    /// child parked in state `?Es` until it was killed.
    ///
    /// The read happens on another thread with a deadline, so a future
    /// regression of this shape FAILS instead of hanging.
    ///
    /// **The deadline is not about this bug.** On 2026-09-05 a CI shard hung
    /// three times, each for the full 45-minute job timeout, and left no
    /// evidence at all: the instrumentation only produced its output when the
    /// command ended, and a command killed by a timeout never ends. It took
    /// most of a night to identify, and the cause turned out not even to be in
    /// this repository. A test that hangs tells you nothing, and it costs
    /// whoever hits it far more than a test that fails. Where a deadline can
    /// turn the first into the second, it is worth the four lines.
    #[test]
    fn shell_exec_runs_the_program_through_a_real_pty() {
        use std::io::Read;
        use std::sync::mpsc;
        use std::time::Duration;

        let pair = open(24, 80).expect("open");
        let mut child = pair
            .slave
            .spawn_command(shell_exec("printf", &["ok-%s".to_string(), "1".to_string()]))
            .expect("spawn");
        // The slave must go before the reader can ever see end-of-file: the
        // child holds the only other copy, so EOF arrives when it exits.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().expect("reader");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut out = Vec::new();
            // A pty master reports EIO rather than EOF once the last slave
            // closes on Linux, so a read error here is an ordinary ending.
            let _ = reader.read_to_end(&mut out);
            let _ = tx.send(String::from_utf8_lossy(&out).into_owned());
        });

        let out = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the child's output should arrive; a timeout here means the read deadlocked");
        assert!(out.contains("ok-1"), "child output was {out:?}");
        child.wait().expect("wait");
    }
}
