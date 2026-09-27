//! # pty-client
//!
//! The typed operations over a `pty` session's socket, ported from the pty
//! project's `src/client.ts`, `src/connection.ts` and `src/remote.ts`, and the
//! `pty kill` / `pty rm` operations. The `pty` binary's client commands are
//! thin printers over these. `pty-core` keeps the wire protocol and the
//! registry; this crate is what a program uses to drive sessions without
//! running the `pty` binary.
//!
//! | Operation | Here |
//! |---|---|
//! | list, with attached clients | [`list::list`] |
//! | screen | [`peek_screen_in`] (text), [`peek_screen_bytes_in`] (bytes) |
//! | send | [`send_in`], [`send_data`], or [`SessionConnection`] |
//! | stats and the child's pid | [`query_stats_in`], [`query_stats_batch_in`] |
//! | signal the program | [`signal_in`] |
//! | stop (`pty kill`) | [`stop_in`] |
//! | remove (`pty rm`) | [`remove_in`] |
//! | attach, peek, follow | [`attach()`], [`peek()`], [`follow`], [`peek_wait`] |
//!
//! The events log is `pty_core::events` (it is part of the on-disk
//! registry), and starting a session's daemon is `pty_lifecycle::spawn_daemon`.
//!
//! **Every operation that takes a root works on that registry, not on
//! `$PTY_ROOT`**, so a program can keep sessions of its own without setting the
//! variable. The forms without a root, like [`stop()`], are the `pty` binary's:
//! they use `$PTY_ROOT`. The rooted forms that go through the registry use
//! `pty_core::registry::with_root` on the calling thread.
//!
//! - [`attach`](mod@attach) — the interactive attach loop (with the
//!   `--attach-stream-fd-v1` machine stream and the `--remote` reconnect
//!   loop),
//! - [`peek`](mod@peek) — one-shot peek, follow, and `peek --wait`,
//! - [`send`](mod@send) — `pty send` framing and pacing,
//! - [`connection`] — [`SessionConnection`] (and `AsyncConnection`, with the
//!   `tokio` feature) for programs (deskset, the testkit) that drive a
//!   session without owning a terminal,
//! - [`stats`] — `pty stats` STATUS queries,
//! - [`signal`](mod@signal), [`stop`](mod@stop), [`remove`](mod@remove) —
//!   signalling, stopping and removing a session,
//! - [`remote`] — `fabric dial` + route handshake for `--remote`,
//! - [`sanitize`], [`tty`] — the terminal-reset byte string and tty helpers.
//!
//! Texts printed here are byte-for-byte the Node client's; the CLI only adds
//! its own prefixes and exit codes.

pub mod attach;
pub mod connection;
pub mod list;
pub mod peek;
pub mod readiness;
pub mod remote;
pub mod remove;
pub mod sanitize;
pub mod send;
pub mod signal;
pub mod stats;
pub mod stop;
pub mod stream;
pub mod summary;
pub mod tty;

use std::fmt;
use std::io;
use std::os::unix::io::RawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;

pub use attach::{AttachOutcome, AttachParams, Reconnect, attach};
#[cfg(feature = "tokio")]
pub use connection::AsyncConnection;
pub use connection::{
    PeekScreenOptions, SendDataOptions, SessionConnection, SessionEvent, peek_screen,
    peek_screen_bytes_in, peek_screen_in, send_data,
};
pub use peek::{PeekOutcome, PeekParams, PeekWaitError, follow, peek, peek_wait, strip_ansi};
pub use readiness::{
    READINESS_TIMEOUT, compare_and_set_lifecycle, query_accepted_socket_ownership,
};
pub use remote::{
    RemoteDialer, RemoteError, RemoteSessionRow, RouteRefusedError, dial_and_route,
    fetch_remote_list,
};
pub use remove::{RemoveError, remove, remove_in};
pub use sanitize::{CLEAR_SCREEN_HOME, CURSOR_TO_BOTTOM, TERMINAL_SANITIZE};
pub use send::{DEFAULT_SEQ_DELAY_MS, SendOptions, resolve_seq_delay_ms, send, send_in, send_over};
pub use signal::{Delivery, SignalError, Signalled, signal, signal_in};
pub use stats::{
    STATS_TIMEOUT, query_stats, query_stats_batch_in, query_stats_in, query_stats_in_with_timeout,
    query_stats_with_timeout, query_status_json,
};
pub use stop::{Aftermath, SHUTDOWN_WAIT, StopError, Stopped, stop, stop_in};
pub use stream::{parse_attach_stream_fd_token, validate_attach_stream_fd};

use pty_core::registry;

/// The descriptors an interactive operation ([`attach()`], [`peek()`], [`follow`])
/// talks to. Defaults to the process's stdin/stdout/stderr; tests hand in pipes.
#[derive(Debug, Clone, Copy)]
pub struct ClientIo {
    pub stdin: RawFd,
    pub stdout: RawFd,
    pub stderr: RawFd,
}

impl Default for ClientIo {
    fn default() -> Self {
        ClientIo {
            stdin: libc::STDIN_FILENO,
            stdout: libc::STDOUT_FILENO,
            stderr: libc::STDERR_FILENO,
        }
    }
}

/// A failure of a client operation, with the Node client's exact message as
/// its `Display`. The CLI prints it to stderr and exits 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// The socket is missing, refused, reset, or broken: the session is gone.
    /// `remote` selects the `Remote session …` wording (Node: a caller-supplied
    /// routed socket).
    NotReachable { name: String, remote: bool },
    /// Any other socket failure; the payload is the Node-style message
    /// (`connect EACCES /path`, `read EIO`, …).
    Connection(String),
    /// The socket closed before the first SCREEN (`SessionConnection`,
    /// `peekScreen`).
    ClosedBeforeScreen(String),
    /// `queryStats` got no STATUS packet within its budget.
    StatsTimeout(String),
    /// The STATUS payload was not JSON.
    InvalidStats(String),
    /// A readiness control request produced no matching packet in its budget.
    ReadinessTimeout(String),
    /// A readiness control response was not one of the tagged JSON outcomes.
    InvalidReadiness(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::NotReachable {
                name,
                remote: false,
            } => {
                write!(f, "Session \"{name}\" not found or not running.")
            }
            ClientError::NotReachable { name, remote: true } => {
                write!(f, "Remote session \"{name}\" not found or not running.")
            }
            ClientError::Connection(msg) => write!(f, "Connection error: {msg}"),
            ClientError::ClosedBeforeScreen(name) => {
                write!(f, "Connection to \"{name}\" closed before screen received.")
            }
            ClientError::StatsTimeout(name) => write!(f, "Timeout querying stats for \"{name}\""),
            ClientError::InvalidStats(name) => {
                write!(f, "Invalid stats response from \"{name}\"")
            }
            ClientError::ReadinessTimeout(name) => {
                write!(f, "Timeout querying readiness control for \"{name}\"")
            }
            ClientError::InvalidReadiness(name) => {
                write!(f, "Invalid readiness control response from \"{name}\"")
            }
        }
    }
}

impl std::error::Error for ClientError {}

/// Which errno values count as "the session is gone". Interactive commands
/// (`attach`, `peek`, `send`) use the broad set (`client.ts:672-681`); the
/// programmatic API and `queryStats` only the first two (`connection.ts:151`,
/// `client.ts:379`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoneSet {
    /// ENOENT, ECONNREFUSED, ECONNRESET, EPIPE.
    Broad,
    /// ENOENT, ECONNREFUSED.
    Strict,
}

/// Does `err` mean the session is not reachable under `set`?
pub fn is_gone(err: &io::Error, set: GoneSet) -> bool {
    match err.raw_os_error() {
        Some(libc::ENOENT) | Some(libc::ECONNREFUSED) => true,
        Some(libc::ECONNRESET) | Some(libc::EPIPE) => set == GoneSet::Broad,
        _ => {
            matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) || (set == GoneSet::Broad
                && matches!(
                    err.kind(),
                    io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
                ))
        }
    }
}

/// Map a socket failure to the client error the Node client would print.
/// `syscall` and `path` build the Node-style detail (`connect ENOENT /x.sock`).
pub fn map_io_error(
    name: &str,
    remote: bool,
    set: GoneSet,
    syscall: &str,
    path: Option<&Path>,
    err: &io::Error,
) -> ClientError {
    if is_gone(err, set) {
        ClientError::NotReachable {
            name: name.to_string(),
            remote,
        }
    } else {
        ClientError::Connection(node_error_message(syscall, path, err))
    }
}

/// Node's `err.message` for a libuv syscall failure: `<syscall> <ERRNO>` plus
/// the path for `connect`. Unknown errnos fall back to the Rust description.
pub fn node_error_message(syscall: &str, path: Option<&Path>, err: &io::Error) -> String {
    let code = err.raw_os_error().and_then(errno_name);
    match (code, path) {
        (Some(code), Some(p)) => format!("{syscall} {code} {}", p.display()),
        (Some(code), None) => format!("{syscall} {code}"),
        (None, _) => format!("{syscall} {err}"),
    }
}

/// Node's `<ERRNO>: <description>, <syscall>` message shape (used by `fs`
/// failures such as the `--attach-stream-fd-v1` probe).
pub fn node_fs_error_message(syscall: &str, err: &io::Error) -> String {
    match err
        .raw_os_error()
        .and_then(|n| errno_name(n).map(|c| (c, errno_description(n))))
    {
        Some((code, desc)) => format!("{code}: {desc}, {syscall}"),
        None => format!("{err}, {syscall}"),
    }
}

/// The symbolic name of an errno value, for the codes a socket client meets.
pub fn errno_name(code: i32) -> Option<&'static str> {
    Some(match code {
        libc::ENOENT => "ENOENT",
        libc::ECONNREFUSED => "ECONNREFUSED",
        libc::ECONNRESET => "ECONNRESET",
        libc::EPIPE => "EPIPE",
        libc::EBADF => "EBADF",
        libc::EACCES => "EACCES",
        libc::EAGAIN => "EAGAIN",
        libc::EINVAL => "EINVAL",
        libc::EIO => "EIO",
        libc::EISDIR => "EISDIR",
        libc::ENOTSOCK => "ENOTSOCK",
        libc::EPERM => "EPERM",
        libc::ETIMEDOUT => "ETIMEDOUT",
        libc::ENOTDIR => "ENOTDIR",
        libc::ENOTCONN => "ENOTCONN",
        libc::EINTR => "EINTR",
        libc::ENOSPC => "ENOSPC",
        libc::ENAMETOOLONG => "ENAMETOOLONG",
        libc::EMFILE => "EMFILE",
        _ => return None,
    })
}

/// libuv's description for an errno value.
fn errno_description(code: i32) -> &'static str {
    match code {
        libc::ENOENT => "no such file or directory",
        libc::ECONNREFUSED => "connection refused",
        libc::ECONNRESET => "connection reset by peer",
        libc::EPIPE => "broken pipe",
        libc::EBADF => "bad file descriptor",
        libc::EACCES => "permission denied",
        libc::EAGAIN => "resource temporarily unavailable",
        libc::EINVAL => "invalid argument",
        libc::EIO => "i/o error",
        libc::EISDIR => "illegal operation on a directory",
        libc::ENOTSOCK => "socket operation on non-socket",
        libc::EPERM => "operation not permitted",
        libc::ETIMEDOUT => "connection timed out",
        libc::ENOTDIR => "not a directory",
        libc::ENOTCONN => "socket is not connected",
        libc::EINTR => "interrupted system call",
        libc::ENOSPC => "no space left on device",
        libc::ENAMETOOLONG => "name too long",
        libc::EMFILE => "too many open files",
        _ => "unknown error",
    }
}

/// Connect to a session's unix socket (raw; see [`connect_session`] for the
/// error mapping).
pub fn connect(name: &str) -> io::Result<UnixStream> {
    UnixStream::connect(registry::socket_path(name))
}

/// Connect to a session's socket, mapping the failure the way the Node
/// client does for `attach`/`peek`/`send` (broad gone set).
pub fn connect_session(name: &str) -> Result<UnixStream, ClientError> {
    connect_session_with(name, GoneSet::Broad)
}

/// [`connect_session`] with an explicit gone set.
pub fn connect_session_with(name: &str, set: GoneSet) -> Result<UnixStream, ClientError> {
    let path = registry::socket_path(name);
    connect_session_at(&path, name, set)
}

pub(crate) fn connect_session_at(
    path: &Path,
    name: &str,
    set: GoneSet,
) -> Result<UnixStream, ClientError> {
    UnixStream::connect(path).map_err(|e| map_io_error(name, false, set, "connect", Some(path), &e))
}

/// Is a session's socket connectable right now?
pub fn is_alive(name: &str) -> bool {
    connect(name).is_ok()
}

/// The line the CLI clients print when a peer declares an oversize packet
/// (`client.ts:120-124`, `:361-365`, `:589-594`), before dropping the socket.
pub fn dropping_connection_line(err: &io::Error) -> String {
    format!("pty client: dropping connection — {err}\n")
}
