//! Programmatic session access, ported from `src/connection.ts`:
//! [`SessionConnection`] (ATTACH on connect, resolves on the first SCREEN,
//! effective geometry from GEOMETRY, `write`/`press`/`resize`/`disconnect`),
//! [`send_data`], and [`peek_screen`]. Unlike [`super::attach()`] nothing here
//! touches stdin/stdout or prints.
//!
//! With the `tokio` feature, `AsyncConnection` offers the same surface on
//! `tokio::net::UnixStream`.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use pty_core::keys::{KeyError, resolve_key};
use pty_core::paste::{BRACKETED_PASTE_END, BRACKETED_PASTE_START};
use pty_core::protocol::{
    MessageType, Packet, PacketReader, decode_exit, decode_geometry, encode_attach, encode_data,
    encode_detach, encode_peek, encode_resize,
};
use pty_core::registry;

use super::{ClientError, GoneSet, connect_session_at, connect_session_with, map_io_error};

/// Something the daemon sent (the `SessionConnection` events of the Node API).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// Effective shared grid, before the output it affects.
    Geometry { rows: u16, cols: u16 },
    /// A screen replay (the initial one, or after a reconnect).
    Screen(Vec<u8>),
    /// Terminal output.
    Data(Vec<u8>),
    /// The session process exited.
    Exit(i32),
    /// The daemon closed the socket.
    Closed,
}

fn packet_event(p: Packet) -> Option<SessionEvent> {
    Some(match p.type_ {
        MessageType::Geometry => {
            let (rows, cols) = decode_geometry(&p.payload);
            SessionEvent::Geometry { rows, cols }
        }
        MessageType::Screen => SessionEvent::Screen(p.payload),
        MessageType::Data => SessionEvent::Data(p.payload),
        MessageType::Exit => SessionEvent::Exit(decode_exit(&p.payload)),
        _ => return None,
    })
}

/// A bidirectional connection to a session.
///
/// node: tests/connection.test.ts:102-202
pub struct SessionConnection {
    name: String,
    socket: Option<UnixStream>,
    reader: PacketReader,
    rows: u16,
    cols: u16,
    effective_rows: u16,
    effective_cols: u16,
    screen: Vec<u8>,
    pending: VecDeque<SessionEvent>,
    closed: bool,
}

impl SessionConnection {
    /// Connect to a local session, send ATTACH(rows, cols) and wait for the
    /// first SCREEN. GEOMETRY before it updates the effective size.
    pub fn connect(name: &str, rows: u16, cols: u16) -> Result<SessionConnection, ClientError> {
        Self::connect_with_timeout(name, rows, cols, None)
    }

    /// [`connect`](Self::connect) that gives up (as "closed before screen")
    /// after `timeout`.
    pub fn connect_with_timeout(
        name: &str,
        rows: u16,
        cols: u16,
        timeout: Option<Duration>,
    ) -> Result<SessionConnection, ClientError> {
        let socket = connect_session_with(name, GoneSet::Strict)?;
        Self::attach_over(socket, name, rows, cols, timeout)
    }

    /// Attach over an already-connected socket.
    pub fn attach_over(
        mut socket: UnixStream,
        name: &str,
        rows: u16,
        cols: u16,
        timeout: Option<Duration>,
    ) -> Result<SessionConnection, ClientError> {
        let path = registry::socket_path(name);
        socket
            .write_all(&encode_attach(rows, cols))
            .map_err(|e| map_io_error(name, false, GoneSet::Strict, "write", Some(&path), &e))?;
        let mut conn = SessionConnection {
            name: name.to_string(),
            socket: Some(socket),
            reader: PacketReader::new(),
            rows,
            cols,
            effective_rows: rows,
            effective_cols: cols,
            screen: Vec::new(),
            pending: VecDeque::new(),
            closed: false,
        };
        let deadline = timeout.map(|t| Instant::now() + t);
        // Events that arrive before the SCREEN belong to the caller, so they
        // are kept. They must NOT go into `conn.pending` yet: `read_more`
        // drains that queue before it touches the socket, so a queued event
        // would be handed straight back to this loop, queued again, and the
        // loop would spin at full CPU without ever reading. With no timeout
        // it never ends. One DATA frame ahead of the first SCREEN is enough,
        // and a remote peer can send one.
        let mut before_screen: VecDeque<SessionEvent> = VecDeque::new();
        loop {
            let remaining = deadline.map(|d| d.saturating_duration_since(Instant::now()));
            if remaining.is_some_and(|r| r.is_zero()) {
                conn.socket = None;
                return Err(ClientError::ClosedBeforeScreen(name.to_string()));
            }
            match conn.read_more(remaining)? {
                Some(SessionEvent::Screen(s)) => {
                    conn.screen = s;
                    // Put them back in front of anything that arrived in the
                    // same read as the SCREEN, so the caller sees one stream
                    // in arrival order.
                    for ev in before_screen.into_iter().rev() {
                        conn.pending.push_front(ev);
                    }
                    return Ok(conn);
                }
                Some(SessionEvent::Closed) => {
                    return Err(ClientError::ClosedBeforeScreen(name.to_string()));
                }
                // GEOMETRY already applied; anything else is queued for the
                // caller.
                Some(SessionEvent::Geometry { .. }) | None => {}
                Some(other) => before_screen.push_back(other),
            }
        }
    }

    /// The initial screen replay.
    pub fn screen(&self) -> &[u8] {
        &self.screen
    }

    /// Session name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Is the socket still open?
    pub fn connected(&self) -> bool {
        self.socket.is_some() && !self.closed
    }

    /// The size we last requested.
    pub fn rows(&self) -> u16 {
        self.rows
    }
    /// The size we last requested.
    pub fn cols(&self) -> u16 {
        self.cols
    }
    /// The shared grid the daemon reported in its last GEOMETRY.
    pub fn effective_rows(&self) -> u16 {
        self.effective_rows
    }
    /// The shared grid the daemon reported in its last GEOMETRY.
    pub fn effective_cols(&self) -> u16 {
        self.effective_cols
    }

    /// Read one chunk from the socket and return the first event it yields
    /// (others are queued). `None` on timeout.
    fn read_more(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<Option<SessionEvent>, ClientError> {
        if let Some(ev) = self.pending.pop_front() {
            return Ok(Some(ev));
        }
        if self.closed {
            return Ok(Some(SessionEvent::Closed));
        }
        let Some(socket) = self.socket.as_mut() else {
            return Ok(Some(SessionEvent::Closed));
        };
        let _ = socket.set_read_timeout(timeout);
        let mut buf = [0u8; 16384];
        let n = match socket.read(&mut buf) {
            Ok(n) => n,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Ok(None);
            }
            Err(e) => {
                self.closed = true;
                self.socket = None;
                return Err(map_io_error(
                    &self.name,
                    false,
                    GoneSet::Strict,
                    "read",
                    None,
                    &e,
                ));
            }
        };
        if n == 0 {
            self.closed = true;
            self.socket = None;
            return Ok(Some(SessionEvent::Closed));
        }
        let packets = match self.reader.feed(&buf[..n]) {
            Ok(p) => p,
            Err(_) => {
                // Node destroys the socket silently on an oversize packet.
                self.closed = true;
                self.socket = None;
                return Ok(Some(SessionEvent::Closed));
            }
        };
        for p in packets {
            if let Some(ev) = packet_event(p) {
                if let SessionEvent::Geometry { rows, cols } = ev {
                    self.effective_rows = rows;
                    self.effective_cols = cols;
                }
                self.pending.push_back(ev);
            }
        }
        Ok(self.pending.pop_front())
    }

    /// The next event, waiting up to `timeout` (`None` = forever). Returns
    /// `Ok(None)` on timeout and [`SessionEvent::Closed`] once (and forever
    /// after) the socket is gone.
    pub fn next_event(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<Option<SessionEvent>, ClientError> {
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            let remaining = deadline.map(|d| d.saturating_duration_since(Instant::now()));
            if let Some(ev) = self.read_more(remaining)? {
                return Ok(Some(ev));
            }
            if remaining.is_some_and(|r| r.is_zero()) {
                return Ok(None);
            }
        }
    }

    /// Send terminal input. Silently dropped when not connected (Node).
    pub fn write(&mut self, data: &[u8]) {
        if self.closed {
            return;
        }
        if let Some(s) = self.socket.as_mut() {
            let _ = s.write_all(&encode_data(data));
        }
    }

    /// Send a named key (`ctrl+c`, `return`, …; see [`pty_core::keys`]).
    pub fn press(&mut self, key: &str) -> Result<(), KeyError> {
        let bytes = resolve_key(key)?;
        self.write(bytes.as_bytes());
        Ok(())
    }

    /// Request a new size.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        if self.closed {
            return;
        }
        if let Some(s) = self.socket.as_mut() {
            self.rows = rows;
            self.cols = cols;
            let _ = s.write_all(&encode_resize(rows, cols));
        }
    }

    /// DETACH, then close.
    pub fn disconnect(&mut self) {
        if let Some(mut s) = self.socket.take() {
            let _ = s.write_all(&encode_detach());
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
        self.closed = true;
    }
}

impl Drop for SessionConnection {
    fn drop(&mut self) {
        self.disconnect();
    }
}

impl std::fmt::Debug for SessionConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionConnection")
            .field("name", &self.name)
            .field("connected", &self.connected())
            .field("rows", &self.rows)
            .field("cols", &self.cols)
            .field("effective_rows", &self.effective_rows)
            .field("effective_cols", &self.effective_cols)
            .finish()
    }
}

/// Pacing/wrapping for [`send_data`] (`connection.ts:23-35`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SendDataOptions {
    pub delay_ms: u64,
    pub paste: bool,
}

/// Send `items` as DATA packets and return once they are in the daemon's
/// receive buffer. Strict gone set (`connection.ts:250-256`).
///
/// node: tests/connection.test.ts:257-314
pub fn send_data<T: AsRef<[u8]>>(
    name: &str,
    items: &[T],
    opts: SendDataOptions,
) -> Result<(), ClientError> {
    let mut socket = connect_session_with(name, GoneSet::Strict)?;
    let path = registry::socket_path(name);
    let map =
        |e: &std::io::Error| map_io_error(name, false, GoneSet::Strict, "write", Some(&path), e);
    let paste = opts.paste && !items.is_empty() && crate::query_stats(name)?.modes.bracketed_paste;
    if paste {
        socket
            .write_all(&encode_data(BRACKETED_PASTE_START.as_bytes()))
            .map_err(|e| map(&e))?;
    }
    for (i, item) in items.iter().enumerate() {
        if i > 0 && opts.delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(opts.delay_ms));
        }
        socket
            .write_all(&encode_data(item.as_ref()))
            .map_err(|e| map(&e))?;
    }
    if paste {
        socket
            .write_all(&encode_data(BRACKETED_PASTE_END.as_bytes()))
            .map_err(|e| map(&e))?;
    }
    let _ = socket.shutdown(std::net::Shutdown::Write);
    Ok(())
}

/// What [`peek_screen`] asks for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PeekScreenOptions {
    pub plain: bool,
    pub full: bool,
}

/// How long [`peek_screen`] waits for the SCREEN packet.
pub const PEEK_SCREEN_TIMEOUT: Duration = Duration::from_secs(5);

enum PeekScreenReadError {
    Client(ClientError),
    TimedOut,
}

impl PeekScreenReadError {
    fn into_client_error(self, name: &str) -> ClientError {
        match self {
            PeekScreenReadError::Client(error) => error,
            PeekScreenReadError::TimedOut => ClientError::ClosedBeforeScreen(name.to_string()),
        }
    }
}

/// Fetch the current screen (one PEEK, first SCREEN). Strict gone set; a close
/// before the screen is `Connection to "<name>" closed before screen received.`
///
/// node: tests/connection.test.ts:318-349
pub fn peek_screen(name: &str, opts: PeekScreenOptions) -> Result<String, ClientError> {
    let path = registry::socket_path(name);
    peek_screen_text_at(&path, name, opts)
}

/// Fetch the current screen from `root/<name>.sock`.
pub fn peek_screen_in(
    root: &Path,
    name: &str,
    opts: PeekScreenOptions,
) -> Result<String, ClientError> {
    let bytes = peek_screen_bytes_in(root, name, opts)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Fetch the exact SCREEN payload from `root/<name>.sock`. For a full peek
/// whose socket is gone, return the retained metadata `lastLines` in the same
/// newline-delimited form as `pty peek --full`.
pub fn peek_screen_bytes_in(
    root: &Path,
    name: &str,
    opts: PeekScreenOptions,
) -> Result<Vec<u8>, ClientError> {
    match peek_screen_bytes_at(&root.join(format!("{name}.sock")), name, opts) {
        Err(PeekScreenReadError::Client(
            error @ (ClientError::NotReachable { .. } | ClientError::ClosedBeforeScreen { .. }),
        )) if opts.full => retained_screen_bytes_in(root, name).ok_or(error),
        result => result.map_err(|error| error.into_client_error(name)),
    }
}

fn retained_screen_bytes_in(root: &Path, name: &str) -> Option<Vec<u8>> {
    let lines = registry::read_metadata_in(root, name)?.last_lines?;
    if lines.is_empty() {
        return Some(Vec::new());
    }

    let mut screen = Vec::with_capacity(lines.iter().map(String::len).sum::<usize>() + lines.len());
    for line in lines {
        screen.extend_from_slice(line.as_bytes());
        screen.push(b'\n');
    }
    Some(screen)
}

fn peek_screen_text_at(
    path: &Path,
    name: &str,
    opts: PeekScreenOptions,
) -> Result<String, ClientError> {
    let bytes =
        peek_screen_bytes_at(path, name, opts).map_err(|error| error.into_client_error(name))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn peek_screen_bytes_at(
    path: &Path,
    name: &str,
    opts: PeekScreenOptions,
) -> Result<Vec<u8>, PeekScreenReadError> {
    let socket =
        connect_session_at(path, name, GoneSet::Strict).map_err(PeekScreenReadError::Client)?;
    peek_screen_bytes_over(socket, path, name, opts, PEEK_SCREEN_TIMEOUT)
}

/// [`peek_screen`] over an already-connected socket with an explicit budget.
pub fn peek_screen_over(
    socket: UnixStream,
    name: &str,
    opts: PeekScreenOptions,
    timeout: Duration,
) -> Result<String, ClientError> {
    let path = registry::socket_path(name);
    let bytes = peek_screen_bytes_over(socket, &path, name, opts, timeout)
        .map_err(|error| error.into_client_error(name))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn peek_screen_bytes_over(
    mut socket: UnixStream,
    path: &Path,
    name: &str,
    opts: PeekScreenOptions,
    timeout: Duration,
) -> Result<Vec<u8>, PeekScreenReadError> {
    let deadline = Instant::now() + timeout;
    socket
        .write_all(&encode_peek(opts.plain, opts.full))
        .map_err(|e| {
            PeekScreenReadError::Client(map_io_error(
                name,
                false,
                GoneSet::Strict,
                "write",
                Some(path),
                &e,
            ))
        })?;
    let mut reader = PacketReader::new();
    let mut buf = [0u8; 16384];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(PeekScreenReadError::TimedOut);
        }
        let _ = socket.set_read_timeout(Some(remaining));
        match socket.read(&mut buf) {
            Ok(0) => {
                return Err(PeekScreenReadError::Client(
                    ClientError::ClosedBeforeScreen(name.to_string()),
                ));
            }
            Ok(n) => match reader.feed(&buf[..n]) {
                Ok(packets) => {
                    for p in packets {
                        if p.type_ == MessageType::Screen {
                            return Ok(p.payload);
                        }
                    }
                }
                Err(e) => {
                    return Err(PeekScreenReadError::Client(ClientError::Connection(
                        e.to_string(),
                    )));
                }
            },
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Err(PeekScreenReadError::TimedOut);
            }
            Err(e) => {
                return Err(PeekScreenReadError::Client(map_io_error(
                    name,
                    false,
                    GoneSet::Strict,
                    "read",
                    None,
                    &e,
                )));
            }
        }
    }
}

#[cfg(feature = "tokio")]
mod async_connection {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream as TokioStream;

    /// [`SessionConnection`] on tokio: the same surface, `async`.
    pub struct AsyncConnection {
        name: String,
        socket: Option<TokioStream>,
        reader: PacketReader,
        rows: u16,
        cols: u16,
        effective_rows: u16,
        effective_cols: u16,
        screen: Vec<u8>,
        pending: VecDeque<SessionEvent>,
        closed: bool,
    }

    impl AsyncConnection {
        /// Connect, ATTACH, and wait for the first SCREEN.
        pub async fn connect(
            name: &str,
            rows: u16,
            cols: u16,
        ) -> Result<AsyncConnection, ClientError> {
            let path = registry::socket_path(name);
            let socket = TokioStream::connect(&path).await.map_err(|e| {
                map_io_error(name, false, GoneSet::Strict, "connect", Some(&path), &e)
            })?;
            Self::attach_over(socket, name, rows, cols).await
        }

        /// Attach over an already-connected tokio socket.
        pub async fn attach_over(
            mut socket: TokioStream,
            name: &str,
            rows: u16,
            cols: u16,
        ) -> Result<AsyncConnection, ClientError> {
            let path = registry::socket_path(name);
            socket
                .write_all(&encode_attach(rows, cols))
                .await
                .map_err(|e| {
                    map_io_error(name, false, GoneSet::Strict, "write", Some(&path), &e)
                })?;
            let mut conn = AsyncConnection {
                name: name.to_string(),
                socket: Some(socket),
                reader: PacketReader::new(),
                rows,
                cols,
                effective_rows: rows,
                effective_cols: cols,
                screen: Vec::new(),
                pending: VecDeque::new(),
                closed: false,
            };
            // Held aside, not queued on the connection: `next_event` drains
            // that queue before it reads the socket, so a queued event comes
            // straight back here and the loop spins at full CPU without ever
            // reading. This path has no timeout at all, so it spins forever.
            // The synchronous `attach_over` had the same defect.
            let mut before_screen: VecDeque<SessionEvent> = VecDeque::new();
            loop {
                match conn.next_event().await? {
                    SessionEvent::Screen(s) => {
                        conn.screen = s;
                        for ev in before_screen.into_iter().rev() {
                            conn.pending.push_front(ev);
                        }
                        return Ok(conn);
                    }
                    SessionEvent::Closed => {
                        return Err(ClientError::ClosedBeforeScreen(name.to_string()));
                    }
                    SessionEvent::Geometry { .. } => {}
                    other => before_screen.push_back(other),
                }
            }
        }

        /// The initial screen replay.
        pub fn screen(&self) -> &[u8] {
            &self.screen
        }
        /// Session name.
        pub fn name(&self) -> &str {
            &self.name
        }
        /// Is the socket still open?
        pub fn connected(&self) -> bool {
            self.socket.is_some() && !self.closed
        }
        /// The size we last requested.
        pub fn rows(&self) -> u16 {
            self.rows
        }
        /// The size we last requested.
        pub fn cols(&self) -> u16 {
            self.cols
        }
        /// The shared grid from the last GEOMETRY.
        pub fn effective_rows(&self) -> u16 {
            self.effective_rows
        }
        /// The shared grid from the last GEOMETRY.
        pub fn effective_cols(&self) -> u16 {
            self.effective_cols
        }

        /// The next event; [`SessionEvent::Closed`] once the socket is gone.
        pub async fn next_event(&mut self) -> Result<SessionEvent, ClientError> {
            loop {
                if let Some(ev) = self.pending.pop_front() {
                    return Ok(ev);
                }
                if self.closed {
                    return Ok(SessionEvent::Closed);
                }
                let Some(socket) = self.socket.as_mut() else {
                    return Ok(SessionEvent::Closed);
                };
                let mut buf = [0u8; 16384];
                let n = match socket.read(&mut buf).await {
                    Ok(n) => n,
                    Err(e) => {
                        self.closed = true;
                        self.socket = None;
                        return Err(map_io_error(
                            &self.name,
                            false,
                            GoneSet::Strict,
                            "read",
                            None,
                            &e,
                        ));
                    }
                };
                if n == 0 {
                    self.closed = true;
                    self.socket = None;
                    return Ok(SessionEvent::Closed);
                }
                let packets = match self.reader.feed(&buf[..n]) {
                    Ok(p) => p,
                    Err(_) => {
                        self.closed = true;
                        self.socket = None;
                        return Ok(SessionEvent::Closed);
                    }
                };
                for p in packets {
                    if let Some(ev) = packet_event(p) {
                        if let SessionEvent::Geometry { rows, cols } = ev {
                            self.effective_rows = rows;
                            self.effective_cols = cols;
                        }
                        self.pending.push_back(ev);
                    }
                }
            }
        }

        /// Send terminal input (dropped when not connected).
        pub async fn write(&mut self, data: &[u8]) {
            if self.closed {
                return;
            }
            if let Some(s) = self.socket.as_mut() {
                let _ = s.write_all(&encode_data(data)).await;
            }
        }

        /// Send a named key.
        pub async fn press(&mut self, key: &str) -> Result<(), KeyError> {
            let bytes = resolve_key(key)?;
            self.write(bytes.as_bytes()).await;
            Ok(())
        }

        /// Request a new size.
        pub async fn resize(&mut self, rows: u16, cols: u16) {
            if self.closed {
                return;
            }
            if let Some(s) = self.socket.as_mut() {
                self.rows = rows;
                self.cols = cols;
                let _ = s.write_all(&encode_resize(rows, cols)).await;
            }
        }

        /// DETACH, then close.
        pub async fn disconnect(&mut self) {
            if let Some(mut s) = self.socket.take() {
                let _ = s.write_all(&encode_detach()).await;
                let _ = s.shutdown().await;
            }
            self.closed = true;
        }
    }

    impl std::fmt::Debug for AsyncConnection {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("AsyncConnection")
                .field("name", &self.name)
                .field("connected", &self.connected())
                .field("rows", &self.rows)
                .field("cols", &self.cols)
                .field("effective_rows", &self.effective_rows)
                .field("effective_cols", &self.effective_cols)
                .finish()
        }
    }

    /// Async [`send_data`](super::send_data).
    pub async fn send_data_async<T: AsRef<[u8]>>(
        name: &str,
        items: &[T],
        opts: SendDataOptions,
    ) -> Result<(), ClientError> {
        let path = registry::socket_path(name);
        let mut socket = TokioStream::connect(&path)
            .await
            .map_err(|e| map_io_error(name, false, GoneSet::Strict, "connect", Some(&path), &e))?;
        let map = |e: &std::io::Error| {
            map_io_error(name, false, GoneSet::Strict, "write", Some(&path), e)
        };
        let paste = if opts.paste && !items.is_empty() {
            let name = name.to_owned();
            tokio::task::spawn_blocking(move || crate::query_stats(&name))
                .await
                .map_err(|error| ClientError::Connection(format!("stats task failed: {error}")))??
                .modes
                .bracketed_paste
        } else {
            false
        };
        if paste {
            socket
                .write_all(&encode_data(BRACKETED_PASTE_START.as_bytes()))
                .await
                .map_err(|e| map(&e))?;
        }
        for (i, item) in items.iter().enumerate() {
            if i > 0 && opts.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(opts.delay_ms)).await;
            }
            socket
                .write_all(&encode_data(item.as_ref()))
                .await
                .map_err(|e| map(&e))?;
        }
        if paste {
            socket
                .write_all(&encode_data(BRACKETED_PASTE_END.as_bytes()))
                .await
                .map_err(|e| map(&e))?;
        }
        let _ = socket.shutdown().await;
        Ok(())
    }

    /// Async [`peek_screen`](super::peek_screen).
    pub async fn peek_screen_async(
        name: &str,
        opts: PeekScreenOptions,
    ) -> Result<String, ClientError> {
        let path = registry::socket_path(name);
        let mut socket = TokioStream::connect(&path)
            .await
            .map_err(|e| map_io_error(name, false, GoneSet::Strict, "connect", Some(&path), &e))?;
        socket
            .write_all(&encode_peek(opts.plain, opts.full))
            .await
            .map_err(|e| map_io_error(name, false, GoneSet::Strict, "write", Some(&path), &e))?;
        let mut reader = PacketReader::new();
        let mut buf = [0u8; 16384];
        let read_loop = async {
            loop {
                let n = socket
                    .read(&mut buf)
                    .await
                    .map_err(|e| map_io_error(name, false, GoneSet::Strict, "read", None, &e))?;
                if n == 0 {
                    return Err(ClientError::ClosedBeforeScreen(name.to_string()));
                }
                let packets = reader
                    .feed(&buf[..n])
                    .map_err(|e| ClientError::Connection(e.to_string()))?;
                for p in packets {
                    if p.type_ == MessageType::Screen {
                        return Ok(String::from_utf8_lossy(&p.payload).into_owned());
                    }
                }
            }
        };
        match tokio::time::timeout(PEEK_SCREEN_TIMEOUT, read_loop).await {
            Ok(r) => r,
            Err(_) => Err(ClientError::ClosedBeforeScreen(name.to_string())),
        }
    }
}

#[cfg(feature = "tokio")]
pub use async_connection::{AsyncConnection, peek_screen_async, send_data_async};
