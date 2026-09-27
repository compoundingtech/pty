//! The client side of `--remote`, ported from `src/remote.ts:196-301`:
//! `fabric dial <peer> pty-remote` hands us a local unix socket path; we
//! speak the one-line control protocol over it (`{"op":"route"}` or
//! `{"op":"list"}`) and, for a route, hand back the socket ready for the
//! ordinary per-session protocol.

use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// ALPN / fabric service name (`remote.ts:10`).
pub const PTY_REMOTE_ALPN: &str = "pty-remote";

/// Default transport binary; `PTY_FABRIC_BIN` overrides (`remote.ts:16`).
pub const DEFAULT_FABRIC_BIN: &str = "fabric";

/// Dial / handshake / list budget (`remote.ts:202`, `:268`).
pub const REMOTE_TIMEOUT: Duration = Duration::from_secs(10);

/// One session row over the control protocol (`remote.ts:20-27`); the same
/// shape the local `--remote` host-group renderer consumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteSessionRow {
    pub name: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// The dial reached the peer's control server but it REFUSED the route: the
/// host is reachable and reports the session gone/absent. `attach --remote`
/// gives up cleanly on this and retries forever on a transport failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteRefusedError(pub String);

impl fmt::Display for RouteRefusedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RouteRefusedError {}

/// A failure of the remote dial/route/list path, with Node's message as its
/// `Display` (the CLI prefixes `pty <cmd> --remote <peer>: `).
#[derive(Debug)]
pub enum RemoteError {
    /// `fabric dial` could not be run or failed (`execFileSync` error text).
    Dial(String),
    /// `fabric dial` printed nothing.
    NoSocket { peer: String },
    /// A socket failure (Node-style `connect ECONNREFUSED <path>` text).
    Io(String),
    /// No ack line within the budget.
    HandshakeTimeout,
    /// The ack line was not JSON (first 80 chars quoted).
    BadRouteResponse(String),
    /// The peer refused the route.
    Refused(RouteRefusedError),
    /// The socket closed before the ack line.
    NotReachable { name: String },
    /// `list` produced no response within the budget.
    ListTimeout,
    /// The list body was not JSON.
    BadListResponse(String),
    /// The peer answered `{"error": …}` to a list.
    Remote(String),
}

impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RemoteError::Dial(msg) => f.write_str(msg),
            RemoteError::NoSocket { peer } => write!(f, "fabric dial {peer} returned no socket"),
            RemoteError::Io(msg) => f.write_str(msg),
            RemoteError::HandshakeTimeout => f.write_str("route handshake timed out"),
            RemoteError::BadRouteResponse(line) => write!(f, "bad route response: {line}"),
            RemoteError::Refused(e) => f.write_str(&e.0),
            RemoteError::NotReachable { name } => {
                write!(f, "remote session \"{name}\" not reachable")
            }
            RemoteError::ListTimeout => f.write_str("remote list timed out"),
            RemoteError::BadListResponse(msg) => write!(f, "bad remote response: {msg}"),
            RemoteError::Remote(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for RemoteError {}

impl RemoteError {
    /// Is this the reachable-but-gone refusal?
    pub fn is_refused(&self) -> bool {
        matches!(self, RemoteError::Refused(_))
    }
}

/// How to reach fabric: the binary and the per-step budget. `Default` reads
/// `PTY_FABRIC_BIN` and uses [`REMOTE_TIMEOUT`].
#[derive(Debug, Clone)]
pub struct RemoteDialer {
    pub fabric_bin: String,
    pub timeout: Duration,
}

impl Default for RemoteDialer {
    fn default() -> Self {
        RemoteDialer {
            fabric_bin: std::env::var("PTY_FABRIC_BIN")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_FABRIC_BIN.to_string()),
            timeout: REMOTE_TIMEOUT,
        }
    }
}

#[derive(Serialize)]
struct RouteRequest<'a> {
    op: &'static str,
    name: &'a str,
}

#[derive(Deserialize, Default)]
struct RouteAck {
    #[serde(default)]
    ok: Option<bool>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize, Default)]
struct ListResponse {
    #[serde(default)]
    sessions: Option<Vec<RemoteSessionRow>>,
    #[serde(default)]
    error: Option<String>,
}

impl RemoteDialer {
    /// `fabric dial <peer> pty-remote`: the local socket path it prints
    /// (trimmed). Errors carry Node's `execFileSync` message shapes.
    pub fn dial(&self, peer: &str) -> Result<String, RemoteError> {
        let mut child = Command::new(&self.fabric_bin)
            .args(["dial", peer, PTY_REMOTE_ALPN])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                let code = e
                    .raw_os_error()
                    .and_then(super::errno_name)
                    .unwrap_or("EUNKNOWN");
                RemoteError::Dial(format!("spawnSync {} {code}", self.fabric_bin))
            })?;
        let deadline = Instant::now() + self.timeout;
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let out_reader = std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = stdout.read_to_end(&mut v);
            v
        });
        let err_reader = std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = stderr.read_to_end(&mut v);
            v
        });
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(RemoteError::Dial(format!(
                            "spawnSync {} ETIMEDOUT",
                            self.fabric_bin
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => {
                    return Err(RemoteError::Dial(format!(
                        "spawnSync {} {e}",
                        self.fabric_bin
                    )));
                }
            }
        };
        let out = out_reader.join().unwrap_or_default();
        let err = err_reader.join().unwrap_or_default();
        if !status.success() {
            return Err(RemoteError::Dial(format!(
                "Command failed: {} dial {peer} {PTY_REMOTE_ALPN}\n{}",
                self.fabric_bin,
                String::from_utf8_lossy(&err)
            )));
        }
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    /// Dial `peer` and route the socket to `name`. The returned socket is a
    /// transparent pipe to that session's daemon; only the ack line has been
    /// consumed (it is read byte by byte, so any bytes after it stay in the
    /// stream for the per-session protocol).
    pub fn dial_and_route(&self, peer: &str, name: &str) -> Result<UnixStream, RemoteError> {
        let path = self.dial(peer)?;
        if path.is_empty() {
            return Err(RemoteError::NoSocket {
                peer: peer.to_string(),
            });
        }
        self.route(&path, name)
    }

    /// [`Self::dial_and_route`], plus the session's row from the same host's
    /// `list` for the lines a client prints. The row is best effort: `None`
    /// when the list fails or does not carry the session.
    pub fn dial_route_and_describe(
        &self,
        peer: &str,
        name: &str,
    ) -> Result<(UnixStream, Option<RemoteSessionRow>), RemoteError> {
        let path = self.dial(peer)?;
        if path.is_empty() {
            return Err(RemoteError::NoSocket {
                peer: peer.to_string(),
            });
        }
        // The list goes first so nothing waits on a second request while the
        // routed session socket is already live.
        let row = self
            .fetch_remote_list(&path)
            .ok()
            .and_then(|rows| rows.into_iter().find(|r| r.name == name));
        let socket = self.route(&path, name)?;
        Ok((socket, row))
    }

    /// The route handshake over the control socket at `path`.
    pub fn route(&self, path: &str, name: &str) -> Result<UnixStream, RemoteError> {
        let deadline = Instant::now() + self.timeout;
        let p = std::path::Path::new(path);
        let mut sock = UnixStream::connect(p)
            .map_err(|e| RemoteError::Io(super::node_error_message("connect", Some(p), &e)))?;
        let mut line = serde_json::to_string(&RouteRequest { op: "route", name })
            .expect("route request serializes");
        line.push('\n');
        sock.write_all(line.as_bytes())
            .map_err(|e| RemoteError::Io(super::node_error_message("write", None, &e)))?;
        let ack = match read_line(&mut sock, deadline) {
            Ok(Some(ack)) => ack,
            Ok(None) => {
                return Err(RemoteError::NotReachable {
                    name: name.to_string(),
                });
            }
            Err(LineError::Timeout) => return Err(RemoteError::HandshakeTimeout),
            Err(LineError::Io(e)) => {
                return Err(RemoteError::Io(super::node_error_message("read", None, &e)));
            }
        };
        let text = String::from_utf8_lossy(&ack).into_owned();
        let resp: RouteAck = match serde_json::from_str(&text) {
            Ok(r) => r,
            Err(_) => {
                let short: String = text.chars().take(80).collect();
                return Err(RemoteError::BadRouteResponse(short));
            }
        };
        if resp.error.is_some() || resp.ok != Some(true) {
            return Err(RemoteError::Refused(RouteRefusedError(
                resp.error.unwrap_or_else(|| "route refused".to_string()),
            )));
        }
        let _ = sock.set_read_timeout(None);
        Ok(sock)
    }

    /// Request the session list from the control socket at `path`.
    pub fn fetch_remote_list(&self, path: &str) -> Result<Vec<RemoteSessionRow>, RemoteError> {
        let deadline = Instant::now() + self.timeout;
        let p = std::path::Path::new(path);
        let mut sock = UnixStream::connect(p)
            .map_err(|e| RemoteError::Io(super::node_error_message("connect", Some(p), &e)))?;
        sock.write_all(b"{\"op\":\"list\"}\n")
            .map_err(|e| RemoteError::Io(super::node_error_message("write", None, &e)))?;
        let mut body = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(RemoteError::ListTimeout);
            }
            let _ = sock.set_read_timeout(Some(remaining));
            match sock.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => body.extend_from_slice(&buf[..n]),
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    return Err(RemoteError::ListTimeout);
                }
                Err(e) => return Err(RemoteError::Io(super::node_error_message("read", None, &e))),
            }
        }
        let text = String::from_utf8_lossy(&body);
        let resp: ListResponse = serde_json::from_str(text.trim())
            .map_err(|e| RemoteError::BadListResponse(e.to_string()))?;
        if let Some(err) = resp.error {
            return Err(RemoteError::Remote(err));
        }
        Ok(resp.sessions.unwrap_or_default())
    }
}

enum LineError {
    Timeout,
    Io(io::Error),
}

/// Read up to (not including) the first `\n`, one byte at a time so nothing
/// past the line is consumed. `Ok(None)` when the socket closes first.
fn read_line(sock: &mut UnixStream, deadline: Instant) -> Result<Option<Vec<u8>>, LineError> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(LineError::Timeout);
        }
        let _ = sock.set_read_timeout(Some(remaining));
        match sock.read(&mut byte) {
            Ok(0) => return Ok(None),
            Ok(_) => {
                if byte[0] == b'\n' {
                    return Ok(Some(line));
                }
                line.push(byte[0]);
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                return Err(LineError::Timeout);
            }
            Err(e) => return Err(LineError::Io(e)),
        }
    }
}

/// Dial `peer` and route to `name` with the default dialer (`PTY_FABRIC_BIN`,
/// 10 s).
pub fn dial_and_route(peer: &str, name: &str) -> Result<UnixStream, RemoteError> {
    RemoteDialer::default().dial_and_route(peer, name)
}

/// [`RemoteDialer::dial_route_and_describe`] with the default dialer.
pub fn dial_route_and_describe(
    peer: &str,
    name: &str,
) -> Result<(UnixStream, Option<RemoteSessionRow>), RemoteError> {
    RemoteDialer::default().dial_route_and_describe(peer, name)
}

/// Fetch the session list from a control socket path with the default dialer.
pub fn fetch_remote_list(path: &str) -> Result<Vec<RemoteSessionRow>, RemoteError> {
    RemoteDialer::default().fetch_remote_list(path)
}
