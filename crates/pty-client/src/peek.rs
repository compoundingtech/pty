//! `pty peek`: one-shot, follow, and `--wait`, ported from `client.ts:76-194`
//! and `cli.ts:1941-1990`.

use std::fmt;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use pty_core::protocol::{MessageType, PacketReader, decode_connection_error, decode_exit, encode_peek};
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
    let mut socket = open(&mut params)?.0;
    let name = params.name;
    let path = registry::socket_path(name);
    let mut out = FdWriter(io.stdout);
    if let Err(error) = socket.write_all(&encode_peek(params.plain, params.full)) {
        let detail = super::node_error_message("write", Some(&path), &error);
        let _ = writeln!(FdWriter(io.stderr), "Connection lost: {detail}");
        if !params.plain {
            let _ = out.write_all(TERMINAL_SANITIZE.as_bytes());
            let _ = out.write_all(CURSOR_TO_BOTTOM.as_bytes());
        }
        let _ = out.write_all(trailer(&mut params, SessionEnd::ConnectionLost).as_bytes());
        return Ok(PeekOutcome::Exited(1));
    }
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
                let detail = super::node_error_message("read", None, &e);
                let _ = writeln!(FdWriter(io.stderr), "Connection lost: {detail}");
                if !params.plain {
                    let _ = out.write_all(TERMINAL_SANITIZE.as_bytes());
                    let _ = out.write_all(CURSOR_TO_BOTTOM.as_bytes());
                }
                let _ = out.write_all(trailer(&mut params, SessionEnd::ConnectionLost).as_bytes());
                return Ok(PeekOutcome::Exited(1));
            }
        };
        if n == 0 {
            // Without EXIT, the child status is unknown.
            drop(raw);
            if !params.plain {
                let _ = out.write_all(TERMINAL_SANITIZE.as_bytes());
                let _ = out.write_all(CURSOR_TO_BOTTOM.as_bytes());
            }
            let _ = out.write_all(trailer(&mut params, SessionEnd::ConnectionLost).as_bytes());
            return Ok(PeekOutcome::Exited(1));
        }
        let packets = match reader.feed(&buf[..n]) {
            Ok(p) => p,
            Err(e) => {
                let _ = FdWriter(io.stderr).write_all(dropping_connection_line(&e).as_bytes());
                drop(raw);
                return Ok(PeekOutcome::Exited(1));
            }
        };
        for p in packets {
            match p.type_ {
                MessageType::ConnectionError => {
                    drop(raw);
                    let reason = decode_connection_error(&p.payload)
                        .map_or("server rejected the attachment", |reason| reason.message());
                    let _ = writeln!(FdWriter(io.stderr), "Connection lost: {reason}");
                    if !params.plain {
                        let _ = out.write_all(TERMINAL_SANITIZE.as_bytes());
                        let _ = out.write_all(CURSOR_TO_BOTTOM.as_bytes());
                    }
                    let _ = out.write_all(trailer(&mut params, SessionEnd::ConnectionLost).as_bytes());
                    return Ok(PeekOutcome::Exited(1));
                }
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
    /// The timeout, for a pattern that is in the session's scrollback but was
    /// never seen while the wait watched: it was printed before the wait
    /// began, or while the output stream was lost (`watched` false).
    Missed {
        timeout_secs: f64,
        patterns: String,
        watched: bool,
    },
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
            PeekWaitError::Missed {
                timeout_secs,
                patterns,
                watched,
            } => {
                write!(f, "Timed out after {timeout_secs}s waiting for {patterns}.")?;
                if *watched {
                    write!(
                        f,
                        "\nIt is in the scrollback from before the wait began, no longer on screen."
                    )
                } else {
                    write!(
                        f,
                        "\nIt is in the scrollback, but the wait lost the output stream and did not see it printed."
                    )
                }
            }
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

/// `peek --wait`: wait until any pattern is a substring of what the session
/// shows. Returns the text to print (the plain screen, or a fresh ANSI
/// screen when `plain` is false; the CLI appends `"\n"`). When the session
/// has exited, `lastLines` from the metadata is matched instead.
/// A `timeout_secs` that is not a finite positive number waits forever.
///
/// Two looks, and either one ends the wait. The plain screen is read every
/// 200 ms, as the Node tool does, which finds text drawn anywhere on it. The
/// output is also watched as it arrives, on one read-only peek connection,
/// which finds a pattern printed and scrolled away between two reads of the
/// screen. Reading the screen alone missed that, and the wait ran on to the
/// timeout or to the session's exit. On a timeout the full scrollback tells
/// such a miss apart from a pattern that never appeared.
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
    // The watch's own screen is the first look, so a pattern already on
    // screen costs no more connections than it did.
    let mut watch = OutputWatch::open(name);
    let mut watched = watch.is_some();
    let mut next_look = if watched {
        start + PEEK_WAIT_INTERVAL
    } else {
        start
    };
    let mut last_screen = String::new();
    loop {
        if let Some(t) = timeout
            && start.elapsed() > t
        {
            // Output that arrived since the last look still counts.
            if let Some(w) = watch.as_mut()
                && w.wait_until(Instant::now(), patterns) == Watched::Seen
            {
                return Ok(screen_now(name, plain, &last_screen, &w.screen));
            }
            return Err(timed_out(name, patterns, timeout_secs, desc, watched));
        }
        if let Some(w) = watch.as_mut() {
            match w.wait_until(next_look, patterns) {
                Watched::Seen => return Ok(screen_now(name, plain, &last_screen, &w.screen)),
                Watched::Quiet => {}
                Watched::Lost => {
                    watch = None;
                    watched = false;
                }
            }
        } else {
            std::thread::sleep(next_look.saturating_duration_since(Instant::now()));
        }
        if Instant::now() < next_look {
            continue;
        }
        next_look = Instant::now() + PEEK_WAIT_INTERVAL;
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
                last_screen = screen;
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
    }
}

/// The screen to print once the watch saw a pattern. It may have scrolled
/// away by now; the wait was satisfied when it was printed.
fn screen_now(name: &str, plain: bool, last_screen: &str, watch_screen: &str) -> String {
    peek_screen(name, PeekScreenOptions { plain, full: false }).unwrap_or_else(|_| {
        if last_screen.is_empty() {
            watch_screen
        } else {
            last_screen
        }
        .to_string()
    })
}

/// The timeout, telling a pattern that is in the scrollback, so was printed
/// before the wait could see it, apart from one that never appeared.
fn timed_out(
    name: &str,
    patterns: &[String],
    timeout_secs: f64,
    patterns_text: String,
    watched: bool,
) -> PeekWaitError {
    let in_scrollback = peek_screen(
        name,
        PeekScreenOptions {
            plain: true,
            full: true,
        },
    )
    .is_ok_and(|text| patterns.iter().any(|p| text.contains(p.as_str())));
    if in_scrollback {
        PeekWaitError::Missed {
            timeout_secs,
            patterns: patterns_text,
            watched,
        }
    } else {
        PeekWaitError::TimedOut {
            timeout_secs,
            patterns: patterns_text,
        }
    }
}

/// What the output watch saw before its deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Watched {
    Seen,
    Quiet,
    /// The connection ended or failed; the wait goes on reading the screen.
    Lost,
}

/// A read-only peek connection held open for a whole `peek --wait`. The
/// daemon sends it the screen and then every byte of output after it, so
/// nothing printed while it is open goes unseen.
struct OutputWatch {
    socket: UnixStream,
    reader: PacketReader,
    text: VisibleText,
    /// The plain screen the connection started with.
    screen: String,
}

impl OutputWatch {
    fn open(name: &str) -> Option<Self> {
        let mut socket = connect_session(name).ok()?;
        socket.write_all(&encode_peek(true, false)).ok()?;
        Some(OutputWatch {
            socket,
            reader: PacketReader::new(),
            text: VisibleText::default(),
            screen: String::new(),
        })
    }

    /// Read what arrives until `until`, stopping as soon as a pattern shows.
    /// It returns by `until` even while output keeps coming, so the caller
    /// still reaches its screen look and its timeout.
    fn wait_until(&mut self, until: Instant, patterns: &[String]) -> Watched {
        use std::os::fd::AsRawFd;
        let mut buf = [0u8; 16384];
        loop {
            let left = until.saturating_duration_since(Instant::now());
            let mut fds = [libc::pollfd {
                fd: self.socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            }];
            match poll(&mut fds, left.as_millis().min(i32::MAX as u128) as i32) {
                Ok(0) => return Watched::Quiet,
                Ok(_) => {}
                Err(_) => return Watched::Lost,
            }
            let n = match self.socket.read(&mut buf) {
                Ok(0) | Err(_) => return Watched::Lost,
                Ok(n) => n,
            };
            let Ok(packets) = self.reader.feed(&buf[..n]) else {
                return Watched::Lost;
            };
            for p in packets {
                match p.type_ {
                    MessageType::Screen => {
                        self.screen = String::from_utf8_lossy(&p.payload).into_owned();
                        if contains_any(&p.payload, patterns) {
                            return Watched::Seen;
                        }
                    }
                    MessageType::Data => {
                        if self.text.feed(&p.payload, patterns) {
                            return Watched::Seen;
                        }
                    }
                    MessageType::Exit | MessageType::ConnectionError => return Watched::Lost,
                    _ => {}
                }
            }
            // Under output that never pauses, poll reports ready every time,
            // and only this check ends the read.
            if Instant::now() >= until {
                return Watched::Quiet;
            }
        }
    }
}

fn contains_any(haystack: &[u8], patterns: &[String]) -> bool {
    patterns.iter().any(|p| {
        let p = p.as_bytes();
        p.is_empty() || haystack.windows(p.len()).any(|w| w == p)
    })
}

/// The visible text of an output stream. Escape sequences are dropped, also
/// when a chunk boundary splits one, and the newest text is kept only as
/// long as a pattern could still end in the next chunk.
#[derive(Default)]
struct VisibleText {
    state: Escape,
    tail: Vec<u8>,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Escape {
    #[default]
    Ground,
    Esc,
    /// `ESC` and intermediates, waiting for the final byte.
    Intermediate,
    Csi,
    /// OSC, DCS, SOS, PM or APC: a string ended by BEL or ST.
    Text,
    TextEsc,
}

impl VisibleText {
    /// Add `chunk` and say whether any pattern now appears.
    ///
    /// Text is joined across a style change, a charset designation or an
    /// escape string, which leave the cursor where it was. It is not joined
    /// across anything that can move the cursor or change the text around
    /// it: every other CSI, and the single-byte escapes such as `ESC 8`.
    /// Text drawn that way is left to the screen read.
    fn feed(&mut self, chunk: &[u8], patterns: &[String]) -> bool {
        let mut found = false;
        for &b in chunk {
            let (state, breaks) = match (self.state, b) {
                (Escape::Ground, 0x1b) => (Escape::Esc, false),
                (Escape::Ground, _) => {
                    self.tail.push(b);
                    (Escape::Ground, false)
                }
                (Escape::Esc, b'[') => (Escape::Csi, false),
                (Escape::Esc, b']' | b'P' | b'X' | b'^' | b'_') => (Escape::Text, false),
                (Escape::Esc | Escape::Intermediate, 0x20..=0x2f) => (Escape::Intermediate, false),
                (Escape::Intermediate, _) => (Escape::Ground, false),
                (Escape::Esc, _) => (Escape::Ground, true),
                (Escape::Csi, b'm') => (Escape::Ground, false),
                (Escape::Csi, 0x40..=0x7e) => (Escape::Ground, true),
                (Escape::Csi, _) => (Escape::Csi, false),
                (Escape::Text, 0x07) => (Escape::Ground, false),
                (Escape::Text, 0x1b) => (Escape::TextEsc, false),
                (Escape::Text, _) => (Escape::Text, false),
                (Escape::TextEsc, _) => (Escape::Ground, false),
            };
            self.state = state;
            if breaks {
                // A pattern completed before the move still counts.
                found |= contains_any(&self.tail, patterns);
                self.tail.clear();
            }
        }
        found |= contains_any(&self.tail, patterns);
        let keep = patterns
            .iter()
            .map(String::len)
            .max()
            .unwrap_or(0)
            .saturating_sub(1);
        let drop = self.tail.len().saturating_sub(keep);
        self.tail.drain(..drop);
        found
    }
}

#[cfg(test)]
mod visible_text_tests {
    use super::VisibleText;

    fn feed_all(chunks: &[&[u8]], pattern: &str) -> bool {
        let mut text = VisibleText::default();
        let patterns = [pattern.to_string()];
        chunks.iter().any(|chunk| text.feed(chunk, &patterns))
    }

    #[test]
    fn a_pattern_split_across_chunks_and_styles_is_seen() {
        assert!(feed_all(
            &[b"\x1b[1mEND ", b"0", b"\x1b[0m04\r\n"],
            "END 004"
        ));
        assert!(feed_all(&[b"END 0\x1b", b"[32", b"m04"], "END 004"));
    }

    #[test]
    fn escape_strings_and_their_payloads_are_not_text() {
        // A window title, a DCS and a charset designation carry no text.
        assert!(!feed_all(&[b"\x1b]0;END 004\x07"], "END 004"));
        assert!(!feed_all(&[b"\x1b]2;END ", b"004\x1b\\"], "END 004"));
        assert!(!feed_all(&[b"\x1bPEND 004\x1b\\"], "END 004"));
        assert!(feed_all(&[b"\x1b(BEND 004"], "END 004"));
    }

    #[test]
    fn text_either_side_of_a_cursor_move_is_not_joined() {
        assert!(!feed_all(&[b"END \x1b[2;1H004"], "END 004"));
        assert!(!feed_all(&[b"END \x1b[1A", b"004"], "END 004"));
        assert!(!feed_all(&[b"END \x1b8004"], "END 004"));
        // A pattern completed before the move still counts.
        assert!(feed_all(&[b"END 004\x1b[2J"], "END 004"));
    }

    #[test]
    fn only_a_pattern_length_of_text_is_kept() {
        let mut text = VisibleText::default();
        let patterns = ["END 004".to_string()];
        assert!(!text.feed(&[b'x'; 10_000], &patterns));
        assert_eq!(text.tail.len(), "END 004".len() - 1);
        assert!(!text.feed(b"END 00", &patterns));
        assert!(text.feed(b"4", &patterns));
    }
}

#[cfg(test)]
mod output_watch_tests {
    use super::*;
    use pty_core::protocol::encode_data;

    /// Once its deadline has passed the watch reads one chunk and returns,
    /// however much output is waiting, so a flood cannot hold the wait.
    #[cfg(target_os = "linux")]
    #[test]
    fn past_its_deadline_the_watch_reads_one_chunk_and_returns() {
        let (client, mut daemon) = UnixStream::pair().unwrap();
        daemon.set_nonblocking(true).unwrap();
        let packet = encode_data(&[b'y'; 1000]);
        let mut written = 0;
        while daemon.write_all(&packet).is_ok() {
            written += packet.len();
        }
        assert!(
            written > 4 * 16384,
            "only {written} bytes fit in the socket"
        );
        let mut watch = OutputWatch {
            socket: client,
            reader: PacketReader::new(),
            text: VisibleText::default(),
            screen: String::new(),
        };
        assert_eq!(
            watch.wait_until(Instant::now(), &["NEVER".to_string()]),
            Watched::Quiet
        );
        watch.socket.set_nonblocking(true).unwrap();
        let mut left = 0;
        let mut buf = [0u8; 65536];
        while let Ok(n) = watch.socket.read(&mut buf) {
            if n == 0 {
                break;
            }
            left += n;
        }
        assert!(
            left >= written - 16384,
            "read {} of {written} bytes",
            written - left
        );
    }
}
