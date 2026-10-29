//! Server mode: drive a session that a `pty` daemon owns, rather than a
//! process this library started.
//!
//! Spawn mode puts a process on the end of a pty this library opened. Server
//! mode asks the `pty` binary for a session and talks to its daemon over the
//! session socket. The screen comes back as SCREEN and DATA frames, which go
//! into the same terminal actor spawn mode uses, so a screenshot from either
//! is built the same way.
//!
//! node: `pty/testing`'s server-mode Session

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU16, Ordering};
use std::sync::{Arc, mpsc::Sender};
use std::time::{Duration, Instant};

use pty_core::protocol::{
    MessageType, PacketReader, decode_exit, decode_geometry, encode_attach, encode_data,
    encode_resize,
};

/// How long to wait for the daemon to publish its socket.
const START_TIMEOUT: Duration = Duration::from_secs(15);

/// What the reader thread learns from the daemon and the session needs.
#[derive(Debug)]
pub struct ServerState {
    pub rows: AtomicU16,
    pub cols: AtomicU16,
    pub exited: AtomicBool,
    pub exit_code: AtomicI32,
}

impl ServerState {
    fn new(rows: u16, cols: u16) -> Self {
        ServerState {
            rows: AtomicU16::new(rows),
            cols: AtomicU16::new(cols),
            exited: AtomicBool::new(false),
            exit_code: AtomicI32::new(0),
        }
    }
}

/// A [`Write`] that wraps each write in a DATA frame, so the session's input
/// methods reach a daemon without knowing they are talking to one.
pub struct DataFramer(pub UnixStream);

impl Write for DataFramer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write_all(&encode_data(buf))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// Where a server-mode session lives and how to take it down.
pub struct ServerBacking {
    pub name: String,
    pub root: PathBuf,
    pub bin: String,
    pub socket: UnixStream,
    pub state: Arc<ServerState>,
    /// This session created the daemon, so closing it should stop it. A
    /// session that merely connected to somebody else's leaves it running.
    pub owned: bool,
    /// The root is this library's temp directory and should go with it.
    pub owns_root: bool,
}

impl ServerBacking {
    /// Ask the daemon for a new size.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let _ = self.socket.write_all(&encode_resize(rows, cols));
        let _ = self.socket.flush();
    }

    /// Stop the session if this handle owns it, and take the temp root with
    /// it. `pty kill` then `pty rm`, not a signal, so the daemon writes its
    /// exit record the way it would for anybody.
    pub fn close(&mut self) {
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        if self.owned {
            for verb in ["kill", "rm"] {
                let _ = std::process::Command::new(&self.bin)
                    .args([verb, &self.name])
                    .env("PTY_ROOT", &self.root)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        }
        if self.owns_root {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

/// The `pty` binary to drive: `PTY_BIN`, else `pty` on PATH.
pub fn pty_bin() -> String {
    std::env::var("PTY_BIN")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "pty".to_string())
}

/// Connect to `<root>/<name>.sock`, attach at `rows`x`cols`, and start a
/// thread that turns frames into terminal bytes on `tx`.
///
/// Returns the socket to write input on, and the shared state the reader
/// updates from GEOMETRY and EXIT.
pub fn connect(
    root: &Path,
    name: &str,
    rows: u16,
    cols: u16,
    tx: Sender<Vec<u8>>,
) -> io::Result<(UnixStream, Arc<ServerState>)> {
    let socket_path = pty_core::registry::socket_path_in(root, name);
    let deadline = Instant::now() + START_TIMEOUT;
    let mut socket = loop {
        match UnixStream::connect(&socket_path) {
            Ok(s) => break s,
            Err(e) if Instant::now() < deadline => {
                if !socket_path.exists() {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                std::thread::sleep(Duration::from_millis(20));
                let _ = e;
            }
            Err(e) => return Err(e),
        }
    };
    socket.write_all(&encode_attach(rows, cols))?;
    socket.flush()?;

    let state = Arc::new(ServerState::new(rows, cols));
    let reader_state = state.clone();
    let mut reader = socket.try_clone()?;
    std::thread::spawn(move || {
        let mut parser = PacketReader::new();
        let mut buf = [0u8; 16384];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let Ok(packets) = parser.feed(&buf[..n]) else {
                break;
            };
            for packet in packets {
                match packet.type_ {
                    // Both carry terminal bytes: SCREEN is the replay of
                    // what is already on screen, DATA is what arrives after.
                    MessageType::Screen | MessageType::Data => {
                        if tx.send(packet.payload).is_err() {
                            return;
                        }
                    }
                    MessageType::Geometry => {
                        let (rows, cols) = decode_geometry(&packet.payload);
                        reader_state.rows.store(rows, Ordering::Relaxed);
                        reader_state.cols.store(cols, Ordering::Relaxed);
                    }
                    MessageType::Exit => {
                        reader_state
                            .exit_code
                            .store(decode_exit(&packet.payload), Ordering::Relaxed);
                        reader_state.exited.store(true, Ordering::Release);
                    }
                    _ => {}
                }
            }
        }
        reader_state.exited.store(true, Ordering::Release);
    });
    Ok((socket, state))
}

/// `pty run -d` the way the testing package does: detached, ephemeral, no
/// display name, with the size and environment the caller asked for.
#[allow(clippy::too_many_arguments)]
pub fn spawn_daemon(
    bin: &str,
    root: &Path,
    name: &str,
    command: &str,
    args: &[&str],
    rows: u16,
    cols: u16,
    cwd: Option<&str>,
    env: &[(String, String)],
) -> io::Result<()> {
    let rows_s = rows.to_string();
    let cols_s = cols.to_string();
    let mut cmd = std::process::Command::new(bin);
    cmd.args(["run", "-d", "-e", "--no-display-name", "--id", name]);
    if let Some(cwd) = cwd {
        cmd.args(["--cwd", cwd]);
    }
    cmd.args(["--rows", &rows_s, "--cols", &cols_s]);
    for (k, v) in env {
        cmd.arg("--env").arg(format!("{k}={v}"));
    }
    cmd.arg("--").arg(command).args(args);
    cmd.env("PTY_ROOT", root);
    // The session must not inherit this process's session identity, or the
    // nesting guard turns the spawn into a direct exec.
    for key in ["PTY_SESSION", "PTY_SESSION_GENERATION", "PTY_SESSION_DIR"] {
        cmd.env_remove(key);
    }
    cmd.env_remove("PTY_REAP_ON_EXIT");
    cmd.env_remove("PTY_RETAIN_FINISHED");
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "pty run failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// A short random id, the same alphabet the registry uses.
pub fn random_id() -> String {
    pty_core::registry::generate_id()
}
