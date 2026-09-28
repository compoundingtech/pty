//! The interactive attach loop, ported from `client.ts:444-752`.
//!
//! One thread, one `poll` set: the session socket, stdin, and a SIGWINCH
//! self-pipe. stdin bytes become DATA (with the Ctrl+\ detach key and its
//! 300 ms double-tap window), SIGWINCH becomes RESIZE, and the daemon's
//! packets go to stdout — or, in `--attach-stream-fd-v1` mode, are re-framed
//! to the inherited descriptor ([`super::stream`]). With a `reconnect`
//! callback (`attach --remote`) a loud disconnect re-dials with the backoff
//! table instead of ending.
//!
//! Every text the Node client prints is printed here, on the same stream; the
//! CLI only turns the [`AttachOutcome`] into an exit code.

use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use pty_core::protocol::{
    MessageType, Packet, PacketReader, decode_exit, encode_attach_with_identity, encode_data,
    encode_detach, encode_resize,
};
use pty_core::registry::now_epoch_ms;

use super::remote::RouteRefusedError;
use super::sanitize::{CLEAR_SCREEN_HOME, CURSOR_TO_BOTTOM, TERMINAL_SANITIZE};
use super::stream::{Accepted, MachineStream, truncated_line};
use super::summary::{SessionEnd, SummaryProvider, TrailerTarget, render_trailer, trailer_header};
use super::tty::{
    DETACH_KEY, DOUBLE_TAP_MS, FdWriter, RawMode, SigwinchPipe, is_tty, normalize_detach_key, poll,
    read_fd, size_or_default, tty_name, window_size,
};
use super::{
    ClientError, ClientIo, GoneSet, dropping_connection_line, is_gone, node_error_message,
};

/// Re-establish the routed socket after a loud disconnect (`attach --remote`).
/// `Ok(Some)` → re-attach over it; `Ok(None)` → transport failure, retry with
/// backoff; `Err` → the host is reachable but the session is gone, stop.
pub type Reconnect = Box<dyn FnMut() -> Result<Option<UnixStream>, RouteRefusedError> + Send>;

/// Backoff schedule for reconnect attempts, then a cap (`client.ts:432-433`).
pub const RECONNECT_BACKOFF_MS: [u64; 7] = [100, 250, 500, 1000, 2000, 5000, 10000];
/// Every attempt past the table waits this long.
pub const RECONNECT_BACKOFF_CAP_MS: u64 = 15000;

/// `PTY_RECONNECT_MAX_ATTEMPTS`: a positive integer bounds consecutive
/// transport failures; anything else means unlimited (`client.ts:439-442`).
pub fn reconnect_max_attempts_from_env() -> Option<usize> {
    let raw = std::env::var("PTY_RECONNECT_MAX_ATTEMPTS").ok()?;
    let t = raw.trim();
    let n: f64 = if t.is_empty() { 0.0 } else { t.parse().ok()? };
    if n.is_finite() && n.fract() == 0.0 && n > 0.0 {
        Some(n as usize)
    } else {
        None
    }
}

fn backoff(attempt: usize) -> Duration {
    Duration::from_millis(
        RECONNECT_BACKOFF_MS
            .get(attempt)
            .copied()
            .unwrap_or(RECONNECT_BACKOFF_CAP_MS),
    )
}

/// What [`attach`] needs.
pub struct AttachParams<'a> {
    /// Session name, for the printed texts.
    pub name: &'a str,
    /// The connected session socket (local `<name>.sock`, or a routed remote
    /// socket).
    pub socket: UnixStream,
    /// Selects the `Remote session …` wording (Node: a caller-supplied socket).
    pub remote: bool,
    /// Reconnect after a loud disconnect (see [`Reconnect`]).
    pub reconnect: Option<Reconnect>,
    /// `--attach-stream-fd-v1 <fd>`: an already-validated descriptor.
    pub stream_fd: Option<RawFd>,
    /// Bound on consecutive failed reconnect attempts; `None` = unlimited.
    pub max_reconnect_attempts: Option<usize>,
    /// The fabric peer of a `--remote` session, for the printed hints.
    pub peer: Option<String>,
    /// The session info the end-of-attach trailer shows.
    pub summary: Option<SummaryProvider>,
}

impl<'a> AttachParams<'a> {
    /// A plain local attach; the attempt bound comes from the environment.
    pub fn new(name: &'a str, socket: UnixStream) -> Self {
        AttachParams {
            name,
            socket,
            remote: false,
            reconnect: None,
            stream_fd: None,
            max_reconnect_attempts: reconnect_max_attempts_from_env(),
            peer: None,
            summary: None,
        }
    }
}

/// How the attach ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachOutcome {
    /// The session exited with this code — or the client failed (code 1)
    /// after printing why.
    Exited(i32),
    /// The user pressed the detach key.
    Detached,
}

impl AttachOutcome {
    /// The process exit code Node uses: the session's, or 0 for a detach.
    pub fn exit_code(self) -> i32 {
        match self {
            AttachOutcome::Exited(c) => c,
            AttachOutcome::Detached => 0,
        }
    }
}

enum Phase {
    Live,
    Reconnecting { attempt: usize, next_at: Instant },
}

struct Attach<'a> {
    name: &'a str,
    remote: bool,
    io: ClientIo,
    socket: Option<UnixStream>,
    reader: PacketReader,
    machine: Option<MachineStream>,
    raw: Option<RawMode>,
    reconnect: Option<Reconnect>,
    max_attempts: Option<usize>,
    peer: Option<String>,
    summary: Option<SummaryProvider>,
    exit_code: i32,
    session_exited: bool,
    /// This client dropped the connection (a malformed frame); the session
    /// itself did not end.
    dropped: bool,
    detach_armed: Option<Instant>,
    phase: Phase,
    stdin_open: bool,
    stdin_was_tty: bool,
    sigwinch: Option<SigwinchPipe>,
}

/// Attach interactively over `params.socket`. Blocks until the session exits,
/// the user detaches, or the connection fails.
///
/// node: tests/attach-stream.test.ts, tests/attach-no-restart.test.ts:208-255
pub fn attach(params: AttachParams, io: &ClientIo) -> AttachOutcome {
    let AttachParams {
        name,
        socket,
        remote,
        reconnect,
        stream_fd,
        max_reconnect_attempts,
        peer,
        summary,
    } = params;
    let mut a = Attach {
        name,
        remote,
        io: *io,
        socket: Some(socket),
        reader: PacketReader::new(),
        machine: stream_fd.map(MachineStream::new),
        raw: None,
        reconnect,
        max_attempts: max_reconnect_attempts,
        peer,
        summary,
        exit_code: 0,
        session_exited: false,
        dropped: false,
        detach_armed: None,
        phase: Phase::Live,
        stdin_open: true,
        stdin_was_tty: is_tty(io.stdin),
        sigwinch: None,
    };
    a.on_ready();
    a.run()
}

impl Attach<'_> {
    // ── output helpers ──

    fn stdout(&self, bytes: &[u8]) {
        let _ = FdWriter(self.io.stdout).write_all(bytes);
    }

    fn stderr(&self, bytes: &[u8]) {
        let _ = FdWriter(self.io.stderr).write_all(bytes);
    }

    /// Say why the session is no longer shown: the header alone on stderr in
    /// machine mode, else the full trailer after the terminal reset. The tty
    /// is restored first, so the trailer never lands on a raw-mode terminal.
    fn trailer(&mut self, end: SessionEnd) {
        self.clean_exit();
        let summary = self.summary.as_mut().and_then(|provide| provide());
        let target = TrailerTarget {
            id: self.name,
            peer: self.peer.as_deref(),
            summary: summary.as_ref(),
        };
        let now = now_epoch_ms();
        if self.machine.is_some() {
            self.stderr(format!("{}\n", trailer_header(end, &target, now)).as_bytes());
        } else {
            let text = render_trailer(end, &target, now);
            self.stdout(format!("{TERMINAL_SANITIZE}{CURSOR_TO_BOTTOM}{text}").as_bytes());
        }
    }

    fn socket_write(&mut self, bytes: &[u8]) {
        if let Some(s) = self.socket.as_mut() {
            let _ = s.write_all(bytes);
        }
    }

    // ── lifecycle ──

    /// Node `onReady`: raw mode, ATTACH with the stdout size, wire input.
    fn on_ready(&mut self) {
        if self.raw.is_none() {
            self.raw = RawMode::enable_if_tty(self.io.stdin);
        }
        let (rows, cols) = size_or_default(self.io.stdout);
        self.socket_write(&encode_attach_with_identity(
            rows,
            cols,
            std::process::id(),
            tty_name(self.io.stdin).as_deref(),
        ));
        if self.sigwinch.is_none() && is_tty(self.io.stdout) {
            self.sigwinch = SigwinchPipe::install().ok();
        }
    }

    /// Node `cleanExit`: drop input wiring, restore the tty, close the socket.
    fn clean_exit(&mut self) {
        self.sigwinch = None;
        self.raw = None;
        self.socket = None;
    }

    fn finish(&mut self, code: i32) -> AttachOutcome {
        self.clean_exit();
        AttachOutcome::Exited(code)
    }

    fn finish_detach(&mut self) -> AttachOutcome {
        if let Some(m) = self.machine.as_mut() {
            if let Err(failure) = m.write(&encode_detach()) {
                self.stderr(format!("{failure}\n").as_bytes());
                self.clean_exit();
                return AttachOutcome::Exited(1);
            }
            self.socket_write(&encode_detach());
            self.clean_exit();
            return AttachOutcome::Detached;
        }
        self.socket_write(&encode_detach());
        self.clean_exit();
        self.trailer(SessionEnd::Detached);
        AttachOutcome::Detached
    }

    // ── daemon → client ──

    fn handle_packets(&mut self, packets: Vec<Packet>) -> Option<AttachOutcome> {
        for p in packets {
            if let Some(m) = self.machine.as_mut() {
                match m.accept(&p) {
                    Err(failure) => {
                        self.stderr(format!("{failure}\n").as_bytes());
                        return Some(self.finish(1));
                    }
                    Ok(Accepted::Skipped) => continue,
                    Ok(Accepted::Forwarded) => {}
                }
                if p.type_ == MessageType::Exit {
                    self.exit_code = decode_exit(&p.payload);
                    self.session_exited = true;
                    let code = self.exit_code;
                    return Some(self.finish(code));
                }
                continue;
            }
            match p.type_ {
                MessageType::Data => self.stdout(&p.payload),
                MessageType::Screen => {
                    // Clear and replay (also how a reconnect resumes).
                    let mut out = Vec::with_capacity(CLEAR_SCREEN_HOME.len() + p.payload.len());
                    out.extend_from_slice(CLEAR_SCREEN_HOME.as_bytes());
                    out.extend_from_slice(&p.payload);
                    self.stdout(&out);
                }
                MessageType::Exit => {
                    self.exit_code = decode_exit(&p.payload);
                    self.session_exited = true;
                    let code = self.exit_code;
                    self.trailer(SessionEnd::Exited(code));
                    return Some(self.finish(code));
                }
                _ => {}
            }
        }
        None
    }

    fn handle_socket_readable(&mut self) -> Option<AttachOutcome> {
        let mut buf = [0u8; 65536];
        let read = self.socket.as_mut()?.read(&mut buf);
        match read {
            Ok(0) => self.on_disconnect(None),
            Ok(n) => match self.reader.feed(&buf[..n]) {
                Ok(packets) => self.handle_packets(packets),
                Err(e) => {
                    self.stderr(dropping_connection_line(&e).as_bytes());
                    self.socket = None;
                    self.dropped = true;
                    self.on_disconnect(None)
                }
            },
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::Interrupted =>
            {
                None
            }
            Err(e) => self.on_disconnect(Some(e)),
        }
    }

    /// Node `handleDisconnect`.
    fn on_disconnect(&mut self, err: Option<io::Error>) -> Option<AttachOutcome> {
        self.socket = None;
        if self.reconnect.is_some() && !self.session_exited {
            self.start_reconnect();
            return None;
        }
        match err {
            Some(e) => {
                self.clean_exit();
                if self.machine.is_some() && !self.session_exited {
                    let detail = node_error_message("read", None, &e);
                    self.stderr(truncated_line(&detail).as_bytes());
                    return Some(self.finish(1));
                }
                let text = if is_gone(&e, GoneSet::Broad) {
                    ClientError::NotReachable {
                        name: self.name.to_string(),
                        remote: self.remote,
                    }
                    .to_string()
                } else {
                    ClientError::Connection(node_error_message("read", None, &e)).to_string()
                };
                self.stderr(format!("{text}\n").as_bytes());
                Some(self.finish(1))
            }
            None => {
                if self.machine.is_some() && !self.session_exited {
                    self.stderr(truncated_line("connection closed").as_bytes());
                    Some(self.finish(1))
                } else {
                    // The daemon went away without an EXIT (`pty kill`, a
                    // crash): say so instead of dropping back to the shell
                    // silently.
                    if !self.session_exited && !self.dropped && self.machine.is_none() {
                        self.trailer(SessionEnd::Ended);
                    }
                    let code = self.exit_code;
                    Some(self.finish(code))
                }
            }
        }
    }

    // ── reconnect ──

    fn start_reconnect(&mut self) {
        let line = "\r\n[reconnecting… — Ctrl-\\ or Ctrl-C to stop]\r\n";
        if self.machine.is_some() {
            self.stderr(line.as_bytes());
        } else {
            self.stdout(line.as_bytes());
        }
        self.phase = Phase::Reconnecting {
            attempt: 0,
            next_at: Instant::now() + backoff(0),
        };
    }

    fn try_reconnect(&mut self) -> Option<AttachOutcome> {
        let attempt = match self.phase {
            Phase::Reconnecting { attempt, .. } => attempt,
            Phase::Live => return None,
        };
        let result = self.reconnect.as_mut()?();
        match result {
            Err(_refused) => {
                // Reachable host that says the session is gone: clean give-up.
                self.phase = Phase::Live;
                self.trailer(SessionEnd::Ended);
                let code = if self.machine.is_some() { 1 } else { 0 };
                Some(self.finish(code))
            }
            Ok(Some(fresh)) => {
                self.socket = Some(fresh);
                self.reader = PacketReader::new();
                self.dropped = false;
                if let Some(m) = self.machine.as_mut() {
                    m.reset();
                }
                self.phase = Phase::Live;
                self.on_ready();
                None
            }
            Ok(None) => {
                let next = attempt + 1;
                if self.max_attempts.is_some_and(|max| next >= max) {
                    self.phase = Phase::Live;
                    self.trailer(SessionEnd::ConnectionLost);
                    return Some(self.finish(1));
                }
                self.phase = Phase::Reconnecting {
                    attempt: next,
                    next_at: Instant::now() + backoff(next),
                };
                None
            }
        }
    }

    // ── client → daemon ──

    fn process_input(&mut self, raw: &[u8]) {
        let data = normalize_detach_key(raw);
        if !data.contains(&DETACH_KEY) {
            self.socket_write(&encode_data(&data));
            return;
        }
        let window = Duration::from_millis(DOUBLE_TAP_MS);
        let mut forward = Vec::with_capacity(data.len());
        for &b in &data {
            if b == DETACH_KEY {
                let now = Instant::now();
                match self.detach_armed {
                    Some(t) if now.duration_since(t) < window => {
                        // Double-tap: send Ctrl+\ to the process, cancel the detach.
                        self.detach_armed = None;
                        forward.push(DETACH_KEY);
                    }
                    _ => {
                        // First tap: detach fires when the window closes.
                        self.detach_armed = Some(now);
                    }
                }
            } else {
                forward.push(b);
            }
        }
        if !forward.is_empty() {
            self.socket_write(&encode_data(&forward));
        }
    }

    fn handle_stdin_readable(&mut self) {
        let mut buf = [0u8; 4096];
        match read_fd(self.io.stdin, &mut buf) {
            Ok(0) => self.stdin_open = false,
            Ok(n) => self.process_input(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => self.stdin_open = false,
        }
    }

    fn handle_sigwinch(&mut self) {
        let arrived = self.sigwinch.as_ref().is_some_and(|p| p.drain());
        if arrived && let Some((rows, cols)) = window_size(self.io.stdout) {
            self.socket_write(&encode_resize(rows, cols));
        }
    }

    // ── the loop ──

    fn run(&mut self) -> AttachOutcome {
        let window = Duration::from_millis(DOUBLE_TAP_MS);
        loop {
            let now = Instant::now();
            if let Some(t) = self.detach_armed
                && now >= t + window
            {
                return self.finish_detach();
            }
            if let Phase::Reconnecting { next_at, .. } = self.phase
                && now >= next_at
            {
                if let Some(outcome) = self.try_reconnect() {
                    return outcome;
                }
                continue;
            }

            let mut deadline = self.detach_armed.map(|t| t + window);
            if let Phase::Reconnecting { next_at, .. } = self.phase {
                deadline = Some(deadline.map_or(next_at, |d| d.min(next_at)));
            }

            let mut fds: Vec<libc::pollfd> = Vec::with_capacity(3);
            let socket_idx = self.socket.as_ref().map(|s| {
                fds.push(libc::pollfd {
                    fd: s.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
                fds.len() - 1
            });
            let stdin_idx = self.stdin_open.then(|| {
                fds.push(libc::pollfd {
                    fd: self.io.stdin,
                    events: libc::POLLIN,
                    revents: 0,
                });
                fds.len() - 1
            });
            let sigwinch_idx = self.sigwinch.as_ref().map(|p| {
                fds.push(libc::pollfd {
                    fd: p.fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
                fds.len() - 1
            });
            if fds.is_empty() && deadline.is_none() {
                // Nothing left to wait for (cannot happen while live, but never spin).
                let code = self.exit_code;
                return self.finish(code);
            }

            let timeout_ms = match deadline {
                Some(d) => {
                    let us = d.saturating_duration_since(Instant::now()).as_micros();
                    us.div_ceil(1000).min(i32::MAX as u128) as i32
                }
                None => -1,
            };
            if let Err(e) = poll(&mut fds, timeout_ms) {
                self.clean_exit();
                self.stderr(format!("Connection error: poll {e}\n").as_bytes());
                return AttachOutcome::Exited(1);
            }

            if let Some(i) = socket_idx
                && fds[i].revents != 0
                && let Some(outcome) = self.handle_socket_readable()
            {
                return outcome;
            }
            if let Some(i) = stdin_idx
                && fds[i].revents != 0
            {
                self.handle_stdin_readable();
                if self.stdin_was_tty && !self.stdin_open {
                    return self.finish(0);
                }
            }
            if let Some(i) = sigwinch_idx
                && fds[i].revents != 0
            {
                self.handle_sigwinch();
            }
        }
    }
}
