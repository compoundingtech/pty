//! `pty peek`: one-shot, follow, and `--wait`, ported from `client.ts:76-194`
//! and `cli.ts:1941-1990`.

use std::fmt;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use pty_core::protocol::{MessageType, PacketReader, decode_exit, encode_peek};
use pty_core::registry;

use super::connection::{PeekScreenOptions, peek_screen};
use super::sanitize::{CURSOR_TO_BOTTOM, TERMINAL_SANITIZE};
use super::summary::{SessionEnd, SummaryProvider, TrailerTarget, render_trailer_now};
use super::tty::{DETACH_KEY, FdWriter, RawMode, normalize_detach_key, poll, read_fd};
use super::{
    ClientError, ClientIo, GoneSet, connect_session, dropping_connection_line, map_io_error,
};

/// What a peek asks for.
pub struct PeekParams<'a> {
    pub name: &'a str,
    /// Plain text (no ANSI) instead of the ANSI screen.
    pub plain: bool,
    /// Full scrollback rather than the viewport.
    pub full: bool,
    /// Speak the protocol over this already-connected socket (a `--remote`
    /// route) instead of dialing `<name>.sock`; `name` is then display only.
    pub socket: Option<UnixStream>,
    /// The fabric peer of a `--remote` peek, for the printed hints.
    pub peer: Option<String>,
    /// The session info the `-f` trailer shows.
    pub summary: Option<SummaryProvider>,
}

impl<'a> PeekParams<'a> {
    /// A local peek of `name`.
    pub fn new(name: &'a str) -> Self {
        PeekParams {
            name,
            plain: false,
            full: false,
            socket: None,
            peer: None,
            summary: None,
        }
    }
}

/// How a peek ended. The CLI exits 0 for `Printed`/`Detached` and with the
/// code for `Exited`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeekOutcome {
    /// One-shot: the screen was written.
    Printed,
    /// The session exited (follow mode prints the exit line first).
    Exited(i32),
    /// Follow mode: Ctrl+\ was pressed.
    Detached,
}

fn open(params: &mut PeekParams) -> Result<(UnixStream, bool), ClientError> {
    match params.socket.take() {
        Some(s) => Ok((s, true)),
        None => connect_session(params.name).map(|s| (s, false)),
    }
}

/// One-shot peek: write the SCREEN payload to stdout, then (unless plain)
/// `TERMINAL_SANITIZE + CURSOR_TO_BOTTOM`, then `"\n"`. A close before any
/// screen is the not-found error.
///
/// node: tests/integration.test.ts:1812-1862
pub fn peek(mut params: PeekParams, io: &ClientIo) -> Result<PeekOutcome, ClientError> {
    let (mut socket, remote) = open(&mut params)?;
    let name = params.name;
    let path = registry::socket_path(name);
    let mut out = FdWriter(io.stdout);
    socket
        .write_all(&encode_peek(params.plain, params.full))
        .map_err(|e| map_io_error(name, remote, GoneSet::Broad, "write", Some(&path), &e))?;
    let mut reader = PacketReader::new();
    let mut buf = [0u8; 16384];
    loop {
        let n = match socket.read(&mut buf) {
            Ok(n) => n,
            Err(e) => return Err(map_io_error(name, remote, GoneSet::Broad, "read", None, &e)),
        };
        if n == 0 {
            return Err(ClientError::NotReachable {
                name: name.to_string(),
                remote,
            });
        }
        let packets = match reader.feed(&buf[..n]) {
            Ok(p) => p,
            Err(e) => {
                let _ = FdWriter(io.stderr).write_all(dropping_connection_line(&e).as_bytes());
                return Err(ClientError::NotReachable {
                    name: name.to_string(),
                    remote,
                });
            }
        };
        for p in packets {
            match p.type_ {
                MessageType::Screen => {
                    let _ = out.write_all(&p.payload);
                    if !params.plain {
                        let _ = out.write_all(TERMINAL_SANITIZE.as_bytes());
                        let _ = out.write_all(CURSOR_TO_BOTTOM.as_bytes());
                    }
                    let _ = out.write_all(b"\n");
                    return Ok(PeekOutcome::Printed);
                }
                MessageType::Exit => {
                    let code = decode_exit(&p.payload);
                    if !params.plain {
                        let _ = out.write_all(TERMINAL_SANITIZE.as_bytes());
                        let _ = out.write_all(CURSOR_TO_BOTTOM.as_bytes());
                    }
                    return Ok(PeekOutcome::Exited(code));
                }
                _ => {}
            }
        }
    }
}

/// `peek -f`: stream the session read-only. Raw mode on a tty stdin; Ctrl+\
/// (single tap) detaches; DATA goes to stdout (ANSI-stripped when plain);
/// detach and EXIT print the session trailer ([`super::summary::render_trailer`]).
///
/// node: client.ts:88-103, :139-157
pub fn follow(mut params: PeekParams, io: &ClientIo) -> Result<PeekOutcome, ClientError> {
    let (mut socket, remote) = open(&mut params)?;
    let name = params.name;
    let path = registry::socket_path(name);
    let mut out = FdWriter(io.stdout);
    socket
        .write_all(&encode_peek(params.plain, params.full))
        .map_err(|e| map_io_error(name, remote, GoneSet::Broad, "write", Some(&path), &e))?;
    let raw = RawMode::enable_if_tty(io.stdin);
    let mut reader = PacketReader::new();
    let mut buf = [0u8; 16384];
    let mut stdin_open = true;
    use std::os::unix::io::AsRawFd;
    let sock_fd = socket.as_raw_fd();
    loop {
        let mut fds = vec![libc::pollfd {
            fd: sock_fd,
            events: libc::POLLIN,
            revents: 0,
        }];
        if stdin_open {
            fds.push(libc::pollfd {
                fd: io.stdin,
                events: libc::POLLIN,
                revents: 0,
            });
        }
        poll(&mut fds, -1).map_err(|e| ClientError::Connection(e.to_string()))?;
        if stdin_open && fds[1].revents != 0 {
            match read_fd(io.stdin, &mut buf) {
                Ok(0) => stdin_open = false,
                Ok(n) => {
                    let data = normalize_detach_key(&buf[..n]);
                    if data.contains(&DETACH_KEY) {
                        drop(raw);
                        drop(socket);
                        let _ = out.write_all(TERMINAL_SANITIZE.as_bytes());
                        let _ = out.write_all(CURSOR_TO_BOTTOM.as_bytes());
                        let _ = out
                            .write_all(trailer(&mut params, SessionEnd::PeekDetached).as_bytes());
                        return Ok(PeekOutcome::Detached);
                    }
                    // All other input is silently ignored (read-only).
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => stdin_open = false,
            }
        }
        if fds[0].revents == 0 {
            continue;
        }
        let n = match socket.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                drop(raw);
                return Err(map_io_error(name, remote, GoneSet::Broad, "read", None, &e));
            }
        };
        if n == 0 {
            // The daemon went away without an EXIT (`pty kill`, a crash):
            // say so, then end with code 0.
            drop(raw);
            if !params.plain {
                let _ = out.write_all(TERMINAL_SANITIZE.as_bytes());
                let _ = out.write_all(CURSOR_TO_BOTTOM.as_bytes());
            }
            let _ = out.write_all(trailer(&mut params, SessionEnd::Ended).as_bytes());
            return Ok(PeekOutcome::Exited(0));
        }
        let packets = match reader.feed(&buf[..n]) {
            Ok(p) => p,
            Err(e) => {
                let _ = FdWriter(io.stderr).write_all(dropping_connection_line(&e).as_bytes());
                drop(raw);
                return Ok(PeekOutcome::Exited(0));
            }
        };
        for p in packets {
            match p.type_ {
                MessageType::Screen => {
                    let _ = out.write_all(&p.payload);
                }
                MessageType::Data => {
                    if params.plain {
                        let text = String::from_utf8_lossy(&p.payload);
                        let _ = out.write_all(strip_ansi(&text).as_bytes());
                    } else {
                        let _ = out.write_all(&p.payload);
                    }
                }
                MessageType::Exit => {
                    let code = decode_exit(&p.payload);
                    drop(raw);
                    drop(socket);
                    if !params.plain {
                        let _ = out.write_all(TERMINAL_SANITIZE.as_bytes());
                        let _ = out.write_all(CURSOR_TO_BOTTOM.as_bytes());
                    }
                    let _ =
                        out.write_all(trailer(&mut params, SessionEnd::Exited(code)).as_bytes());
                    return Ok(PeekOutcome::Exited(code));
                }
                _ => {}
            }
        }
    }
}

/// The `-f` trailer for `end`, ANSI-stripped when plain.
fn trailer(params: &mut PeekParams, end: SessionEnd) -> String {
    let summary = params.summary.as_mut().and_then(|provide| provide());
    let target = TrailerTarget {
        id: params.name,
        peer: params.peer.as_deref(),
        summary: summary.as_ref(),
    };
    let text = render_trailer_now(end, &target);
    if params.plain {
        strip_ansi(&text)
    } else {
        text
    }
}

/// Remove CSI sequences (`ESC [ … <letter>`), the pty project's `stripAnsi`
/// (`src/tui/colors.ts:29-30`).
pub fn strip_ansi(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b';') {
                j += 1;
            }
            if j < bytes.len() && bytes[j].is_ascii_alphabetic() {
                i = j + 1;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    // Only ASCII bytes were removed, so the result is still valid UTF-8.
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// `peek --wait` failures; the `Display` is the exact stderr text (multi-line
/// for the exited case), exit 1.
#[derive(Debug, Clone, PartialEq)]
pub enum PeekWaitError {
    /// `Timed out after <sec>s waiting for "<p>".`
    TimedOut { timeout_secs: f64, patterns: String },
    /// `Session "<name>" exited (code <c|?>) without matching "<p>".` plus
    /// `Last output:` and the indented lines when there are any.
    Exited {
        name: String,
        exit_code: Option<i32>,
        patterns: String,
        last_lines: Vec<String>,
    },
}

impl fmt::Display for PeekWaitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PeekWaitError::TimedOut {
                timeout_secs,
                patterns,
            } => write!(f, "Timed out after {timeout_secs}s waiting for {patterns}."),
            PeekWaitError::Exited {
                name,
                exit_code,
                patterns,
                last_lines,
            } => {
                let code = match exit_code {
                    Some(c) => c.to_string(),
                    None => "?".to_string(),
                };
                write!(
                    f,
                    "Session \"{name}\" exited (code {code}) without matching {patterns}."
                )?;
                if !last_lines.is_empty() {
                    write!(f, "\nLast output:")?;
                    for line in last_lines {
                        write!(f, "\n  {line}")?;
                    }
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for PeekWaitError {}

/// Render patterns as `"a"` or `"a" or "b"` (`cli.ts:1946`).
pub fn describe_patterns(patterns: &[String]) -> String {
    patterns
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(" or ")
}

/// Poll interval of `peek --wait`.
pub const PEEK_WAIT_INTERVAL: Duration = Duration::from_millis(200);

/// `peek --wait`: poll the plain screen every 200 ms until any pattern is a
/// substring. Returns the text to print (the plain screen, or a fresh ANSI
/// screen when `plain` is false; the CLI appends `"\n"`). When the session
/// has exited, `lastLines` from the metadata is matched instead.
/// A `timeout_secs` that is not a finite positive number waits forever.
///
/// node: tests/peek-wait.test.ts:111-188
pub fn peek_wait(
    name: &str,
    patterns: &[String],
    timeout_secs: f64,
    plain: bool,
) -> Result<String, PeekWaitError> {
    let start = Instant::now();
    // A finite, positive number of seconds is a deadline. Anything else
    // waits, which is what the Node tool does with the same input.
    // `Duration::from_secs_f64` panics on infinity, and `pty peek --wait x
    // --timeout inf` is an ordinary thing for somebody to type.
    let timeout = if timeout_secs.is_finite() && timeout_secs > 0.0 {
        Some(Duration::from_secs_f64(timeout_secs))
    } else {
        None
    };
    let matches_any = |text: &str| patterns.iter().any(|p| text.contains(p.as_str()));
    let desc = describe_patterns(patterns);
    loop {
        if let Some(t) = timeout
            && start.elapsed() > t
        {
            return Err(PeekWaitError::TimedOut {
                timeout_secs,
                patterns: desc,
            });
        }
        match peek_screen(
            name,
            PeekScreenOptions {
                plain: true,
                full: false,
            },
        ) {
            Ok(screen) => {
                if matches_any(&screen) {
                    if plain {
                        return Ok(screen);
                    }
                    if let Ok(ansi) = peek_screen(
                        name,
                        PeekScreenOptions {
                            plain: false,
                            full: false,
                        },
                    ) {
                        return Ok(ansi);
                    }
                    // The session vanished between the two peeks; Node would
                    // reject and exit 1 with the raw error. Fall back to the
                    // plain screen, which did match.
                    return Ok(screen);
                }
            }
            Err(_) => {
                // Session might have exited — check metadata for lastLines.
                if let Some(meta) = registry::read_metadata(name)
                    && meta.exited_at.is_some()
                    && let Some(last_lines) = meta.last_lines
                {
                    let last_output = last_lines.join("\n");
                    if matches_any(&last_output) {
                        return Ok(last_output);
                    }
                    return Err(PeekWaitError::Exited {
                        name: name.to_string(),
                        exit_code: meta.exit_code,
                        patterns: desc,
                        last_lines,
                    });
                }
                // No exitedAt — might be a transient connection error, retry.
            }
        }
        std::thread::sleep(PEEK_WAIT_INTERVAL);
    }
}
