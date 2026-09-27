//! Registry listing with attached-client identities: the one implementation
//! behind `pty list --json [--clients]` and embedded consumers. The CLI calls
//! [`attached_clients`] on its already-filtered rows; embedded consumers call
//! [`list`].
//!
//! **Unstable.** This API will move onto `SessionRef` / `PtyRoot` with
//! compoundingtech/pty-rust#1 and #3. Until then [`SessionInfo`] exposes the
//! on-disk session metadata as-is.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde::{Serialize, Serializer};

use pty_core::protocol::AttachedClient;
use pty_core::registry::{self, SessionInfo};

use super::stats::query_attached_clients;

/// How to ask running daemons for their attached clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientQuery {
    /// One deadline for the whole listing, not per daemon.
    pub deadline: Duration,
    /// Sockets queried at once; a stalled daemon holds one worker only.
    pub concurrency: usize,
}

impl Default for ClientQuery {
    fn default() -> Self {
        ClientQuery {
            deadline: Duration::from_millis(500),
            concurrency: 16,
        }
    }
}

/// A running session's attached clients as far as this listing knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientSet {
    /// No answer within the deadline, or the daemon predates the request.
    /// Never an empty set.
    Unknown,
    Known(Vec<AttachedClient>),
}

impl ClientSet {
    /// The clients, or `None` when unknown.
    pub fn known(&self) -> Option<&[AttachedClient]> {
        match self {
            ClientSet::Unknown => None,
            ClientSet::Known(clients) => Some(clients),
        }
    }
}

/// `null` for unknown, an array otherwise: the `list --json --clients` shape.
impl Serialize for ClientSet {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.known().serialize(s)
    }
}

/// What [`list`] reads.
#[derive(Debug, Clone, Default)]
pub struct ListOptions {
    pub registry: registry::list::ListOptions,
    /// `None`: do not contact daemons for clients.
    pub clients: Option<ClientQuery>,
}

/// One listed session.
#[derive(Debug, Clone)]
pub struct ListedSession {
    pub info: SessionInfo,
    /// `None` when not requested or the session is not running.
    pub clients: Option<ClientSet>,
}

/// One bounded observation of `root`, sorted by session name.
pub fn list(root: &Path, options: &ListOptions) -> Vec<ListedSession> {
    let sessions = registry::list::list_sessions_in(root, &options.registry);
    let clients = match &options.clients {
        Some(query) => attached_clients(&sessions, query)
            .into_iter()
            .map(Some)
            .collect(),
        None => vec![None; sessions.len()],
    };
    sessions
        .into_iter()
        .zip(clients)
        .map(|(info, clients)| ListedSession {
            clients: clients.filter(|_| info.is_running()),
            info,
        })
        .collect()
}

/// Attached clients for each of `sessions`, index-aligned. Rows that are not
/// running come back [`ClientSet::Unknown`] and are never contacted.
pub fn attached_clients(sessions: &[SessionInfo], query: &ClientQuery) -> Vec<ClientSet> {
    let next = AtomicUsize::new(0);
    let deadline = Instant::now() + query.deadline;
    let workers = sessions
        .iter()
        .filter(|s| s.is_running())
        .count()
        .min(query.concurrency.max(1));
    let mut results = vec![ClientSet::Unknown; sessions.len()];
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut found = Vec::new();
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(session) = sessions.get(index) else {
                            break;
                        };
                        if !session.is_running() {
                            continue;
                        }
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            break;
                        }
                        if let Some(clients) =
                            query_attached_clients(&session.socket_path, &session.name, remaining)
                        {
                            found.push((index, clients));
                        }
                    }
                    found
                })
            })
            .collect();
        for handle in handles {
            for (index, clients) in handle.join().expect("client query worker") {
                results[index] = ClientSet::Known(clients);
            }
        }
    });
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use pty_core::protocol::{encode_status_clients, encode_status_response};
    use pty_core::registry::SessionStatus;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    fn running(socket_path: PathBuf) -> SessionInfo {
        SessionInfo {
            name: "test".into(),
            socket_path,
            pid: Some(42),
            status: SessionStatus::Running,
            metadata: None,
        }
    }

    fn sock(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pty-core-clients-{}-{tag}.sock", std::process::id()))
    }

    #[test]
    fn queries_do_not_wait_for_an_earlier_session() {
        let (first_path, second_path) = (sock("first"), sock("second"));
        let first_listener = UnixListener::bind(&first_path).unwrap();
        let second_listener = UnixListener::bind(&second_path).unwrap();
        let (ready, started) = std::sync::mpsc::channel();
        // The first daemon answers only after the second one was asked, so a
        // sequential query would leave the first row unknown.
        let first = std::thread::spawn(move || {
            let (mut socket, _) = first_listener.accept().unwrap();
            let mut request = [0; 12];
            socket.read_exact(&mut request).unwrap();
            if started.recv_timeout(Duration::from_secs(1)).is_ok() {
                socket
                    .write_all(&encode_status_response(
                        r#"[{"pid":1,"tty":null,"attachedAt":"2026-09-25T12:00:00Z"}]"#,
                    ))
                    .unwrap();
            }
        });
        let second = std::thread::spawn(move || {
            let (mut socket, _) = second_listener.accept().unwrap();
            let mut request = [0; 12];
            socket.read_exact(&mut request).unwrap();
            assert_eq!(request.to_vec(), encode_status_clients());
            ready.send(()).unwrap();
            socket
                .write_all(&encode_status_response(
                    r#"[{"pid":2,"tty":null,"attachedAt":"2026-09-25T12:00:00Z"}]"#,
                ))
                .unwrap();
        });
        let sessions = [running(first_path.clone()), running(second_path.clone())];
        let clients = attached_clients(&sessions, &ClientQuery::default());
        assert_eq!(clients[0].known().unwrap()[0].pid, Some(1));
        assert_eq!(clients[1].known().unwrap()[0].pid, Some(2));
        first.join().unwrap();
        second.join().unwrap();
        std::fs::remove_file(first_path).unwrap();
        std::fs::remove_file(second_path).unwrap();
    }

    #[test]
    fn stalled_daemon_is_unknown_within_the_deadline() {
        let path = sock("stalled");
        let listener = UnixListener::bind(&path).unwrap();
        let (release, waiting) = std::sync::mpsc::channel::<()>();
        let daemon = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 12];
            socket.read_exact(&mut request).unwrap();
            let _ = waiting.recv_timeout(Duration::from_secs(3));
        });
        let start = Instant::now();
        let clients = attached_clients(&[running(path.clone())], &ClientQuery::default());
        let elapsed = start.elapsed();
        release.send(()).unwrap();
        daemon.join().unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(clients, vec![ClientSet::Unknown]);
        assert!(elapsed < Duration::from_secs(2), "stalled query took {elapsed:?}");
    }

    #[test]
    fn legacy_daemon_is_unknown_and_serializes_as_null() {
        let path = sock("legacy");
        let listener = UnixListener::bind(&path).unwrap();
        let daemon = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 12];
            socket.read_exact(&mut request).unwrap();
            // A daemon that predates the clients query answers with stats.
            socket.write_all(&encode_status_response("{}")).unwrap();
        });
        let clients = attached_clients(&[running(path.clone())], &ClientQuery::default());
        daemon.join().unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(clients, vec![ClientSet::Unknown]);
        assert_eq!(serde_json::to_string(&clients[0]).unwrap(), "null");
        assert_eq!(serde_json::to_string(&ClientSet::Known(Vec::new())).unwrap(), "[]");
    }

    #[test]
    fn sessions_that_are_not_running_are_never_contacted() {
        let path = sock("exited");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut exited = running(path.clone());
        exited.status = SessionStatus::Exited;
        let clients = attached_clients(&[exited], &ClientQuery::default());
        let contacted = listener.accept().is_ok();
        std::fs::remove_file(path).unwrap();
        assert_eq!(clients, vec![ClientSet::Unknown]);
        assert!(!contacted, "an exited session was queried");
    }
}
