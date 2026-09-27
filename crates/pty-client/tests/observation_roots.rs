//! Public contracts for observing a registry selected by path instead of the
//! process environment.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pty_client::{
    ClientError, PeekScreenOptions, peek_screen_bytes_in, peek_screen_in, query_stats_in,
    query_stats_in_with_timeout,
};
use pty_core::events::{read_all_events_in, read_recent_events_in};
use pty_core::protocol::{
    MessageType, Packet, PacketReader, decode_peek, encode_data, encode_screen,
    encode_status_response,
};
use pty_core::registry::{
    ListOptions, SessionMetadata, SessionStatus, last_output_at_ms_in, list_sessions_in,
    probe_sockets_within_budget,
};
use pty_client::query_stats_batch_in;
use pty_core::busy_connects_on_this_thread;
use serde_json::json;

const T: Duration = Duration::from_secs(5);
static NEXT_ROOT: AtomicUsize = AtomicUsize::new(0);

struct TestRoot(PathBuf);

impl TestRoot {
    fn new() -> Self {
        let serial = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("ptyobs-{}-{serial}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create explicit test root");
        TestRoot(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn session_file(&self, name: &str, suffix: &str) -> PathBuf {
        self.0.join(format!("{name}.{suffix}"))
    }

    fn listen(&self, name: &str) -> UnixListener {
        UnixListener::bind(self.session_file(name, "sock")).expect("bind test session socket")
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn read_packet(stream: &mut UnixStream) -> Packet {
    let mut reader = PacketReader::new();
    let mut buf = [0_u8; 1024];
    loop {
        let n = stream.read(&mut buf).expect("read request packet");
        assert!(n > 0, "client closed before sending a packet");
        let mut packets = reader.feed(&buf[..n]).expect("valid request packet");
        if !packets.is_empty() {
            return packets.remove(0);
        }
    }
}

fn serve_once(listener: UnixListener, response: Vec<u8>) -> JoinHandle<Packet> {
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept client");
        let packet = read_packet(&mut stream);
        stream.write_all(&response).expect("write response");
        packet
    })
}

fn metadata(name: &str) -> String {
    json!({
        "command": "cat",
        "args": [],
        "displayCommand": "cat",
        "cwd": "/tmp",
        "createdAt": "2026-09-13T00:00:00.000Z",
        "displayName": name,
    })
    .to_string()
}

fn stats_body(name: &str) -> String {
    json!({
        "name": name,
        "terminal": {
            "cols": 80,
            "rows": 24,
            "cursorX": 0,
            "cursorY": 0,
            "scrollbackUsed": 0,
            "scrollbackCapacity": 10000,
        },
        "process": {
            "alive": true,
            "exitCode": null,
            "pid": 100,
            "resources": null,
        },
        "daemon": {
            "pid": 200,
            "resources": null,
        },
        "clients": {
            "total": 0,
            "attached": 0,
            "readOnly": 0,
        },
        "modes": {
            "sgrMouse": false,
            "cursorHidden": false,
            "kittyKeyboard": false,
            "kittyKeyboardFlags": [],
        },
        "uptimeSeconds": 1,
        "createdAt": "2026-09-13T00:00:00.000Z",
    })
    .to_string()
}

#[test]
fn listing_keeps_explicit_roots_independent() {
    let left = TestRoot::new();
    let right = TestRoot::new();
    std::fs::write(left.session_file("left", "json"), metadata("left")).unwrap();
    std::fs::write(right.session_file("right", "json"), metadata("right")).unwrap();

    let left_sessions = list_sessions_in(left.path(), &ListOptions::default());
    let right_sessions = list_sessions_in(right.path(), &ListOptions::default());

    assert_eq!(
        left_sessions
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["left"]
    );
    assert_eq!(
        left_sessions[0].socket_path,
        left.session_file("left", "sock")
    );
    assert_eq!(
        right_sessions
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["right"]
    );
    assert_eq!(
        right_sessions[0].socket_path,
        right.session_file("right", "sock")
    );
}

#[test]
fn stats_resolve_the_session_socket_in_the_supplied_root() {
    let left = TestRoot::new();
    let right = TestRoot::new();
    let name = "same-name";
    let left_server = serve_once(
        left.listen(name),
        encode_status_response(&stats_body("from-left")),
    );
    let right_server = serve_once(
        right.listen(name),
        encode_status_response(&stats_body("from-right")),
    );

    let left_stats = query_stats_in(left.path(), name).unwrap();
    let right_stats = query_stats_in_with_timeout(right.path(), name, T).unwrap();

    assert_eq!(left_stats.name, "from-left");
    assert_eq!(right_stats.name, "from-right");
    assert_eq!(left_server.join().unwrap().type_, MessageType::Status);
    assert_eq!(right_server.join().unwrap().type_, MessageType::Status);
}

#[test]
fn explicit_peek_has_a_text_api_and_a_lossless_byte_api() {
    let text_root = TestRoot::new();
    let bytes_root = TestRoot::new();
    let name = "peek";
    let text_server = serve_once(text_root.listen(name), encode_screen(b"text screen"));
    let raw = b"before\xff\x80after\x00".to_vec();
    let bytes_server = serve_once(bytes_root.listen(name), encode_screen(&raw));
    let options = PeekScreenOptions {
        plain: false,
        full: true,
    };

    assert_eq!(
        peek_screen_in(text_root.path(), name, options).unwrap(),
        "text screen"
    );
    assert_eq!(
        peek_screen_bytes_in(bytes_root.path(), name, options).unwrap(),
        raw
    );

    let text_request = text_server.join().unwrap();
    let bytes_request = bytes_server.join().unwrap();
    assert_eq!(text_request.type_, MessageType::Peek);
    assert_eq!(bytes_request.type_, MessageType::Peek);
    assert_eq!(decode_peek(&text_request.payload), (false, true));
    assert_eq!(decode_peek(&bytes_request.payload), (false, true));
}

#[test]
fn full_peek_uses_retained_last_lines_when_the_socket_is_unavailable() {
    let root = TestRoot::new();
    let name = "retained";
    std::fs::write(
        root.session_file(name, "json"),
        json!({
            "command": "cat",
            "args": [],
            "displayCommand": "cat",
            "cwd": "/tmp",
            "createdAt": "2026-09-13T00:00:00.000Z",
            "exitedAt": "2026-09-13T00:00:01.000Z",
            "lastLines": ["café", "snowman ☃ and \"quoted\""],
        })
        .to_string(),
    )
    .unwrap();
    let options = PeekScreenOptions {
        plain: false,
        full: true,
    };
    let expected = b"caf\xc3\xa9\nsnowman \xe2\x98\x83 and \"quoted\"\n";

    assert_eq!(
        peek_screen_bytes_in(root.path(), name, options).unwrap(),
        expected
    );
    assert_eq!(
        peek_screen_in(root.path(), name, options)
            .unwrap()
            .as_bytes(),
        expected
    );
}

#[test]
fn full_peek_returns_an_empty_retained_screen() {
    let root = TestRoot::new();
    let name = "empty-retained";
    std::fs::write(
        root.session_file(name, "json"),
        json!({
            "command": "cat",
            "args": [],
            "displayCommand": "cat",
            "cwd": "/tmp",
            "createdAt": "2026-09-13T00:00:00.000Z",
            "lastLines": [],
        })
        .to_string(),
    )
    .unwrap();

    assert_eq!(
        peek_screen_bytes_in(
            root.path(),
            name,
            PeekScreenOptions {
                plain: false,
                full: true,
            },
        )
        .unwrap(),
        Vec::<u8>::new()
    );
}

#[test]
fn full_peek_uses_retained_last_lines_when_the_socket_closes_before_screen() {
    let root = TestRoot::new();
    let name = "retained-race";
    std::fs::write(
        root.session_file(name, "json"),
        json!({
            "command": "cat",
            "args": [],
            "displayCommand": "cat",
            "cwd": "/tmp",
            "createdAt": "2026-09-13T00:00:00.000Z",
            "lastLines": ["saved output"],
        })
        .to_string(),
    )
    .unwrap();
    let server = std::thread::spawn({
        let listener = root.listen(name);
        move || {
            let (mut stream, _) = listener.accept().expect("accept client");
            assert_eq!(read_packet(&mut stream).type_, MessageType::Peek);
        }
    });

    assert_eq!(
        peek_screen_bytes_in(
            root.path(),
            name,
            PeekScreenOptions {
                plain: false,
                full: true,
            },
        )
        .unwrap(),
        b"saved output\n"
    );
    server.join().unwrap();
}

#[test]
fn full_peek_does_not_use_retained_lines_when_a_live_socket_times_out() {
    let root = TestRoot::new();
    let name = "slow-live-session";
    std::fs::write(
        root.session_file(name, "json"),
        json!({
            "command": "cat",
            "args": [],
            "displayCommand": "cat",
            "cwd": "/tmp",
            "createdAt": "2026-09-13T00:00:00.000Z",
            "lastLines": ["stale output"],
        })
        .to_string(),
    )
    .unwrap();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn({
        let listener = root.listen(name);
        move || {
            let (mut stream, _) = listener.accept().expect("accept client");
            assert_eq!(read_packet(&mut stream).type_, MessageType::Peek);
            release_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("client should finish before the server closes");
        }
    });

    let result = peek_screen_bytes_in(
        root.path(),
        name,
        PeekScreenOptions {
            plain: false,
            full: true,
        },
    );
    release_tx.send(()).unwrap();
    server.join().unwrap();

    assert_eq!(
        result.expect_err("a live timeout must not return retained output"),
        ClientError::ClosedBeforeScreen(name.to_string())
    );
}

#[test]
fn event_readers_use_only_the_supplied_root() {
    let left = TestRoot::new();
    let right = TestRoot::new();
    let name = "same-name";
    let left_events = [
        json!({
            "session": name,
            "type": "user.left-first",
            "ts": "2026-09-13T00:00:00.000Z",
        }),
        json!({
            "session": name,
            "type": "user.left-last",
            "ts": "2026-09-13T00:00:01.000Z",
        }),
    ]
    .map(|event| event.to_string())
    .join("\n")
        + "\n";
    let right_events = json!({
        "session": name,
        "type": "user.right",
        "ts": "2026-09-13T00:00:02.000Z",
    })
    .to_string()
        + "\n";
    std::fs::write(left.session_file(name, "events.jsonl"), left_events).unwrap();
    std::fs::write(right.session_file(name, "events.jsonl"), right_events).unwrap();

    let recent = read_recent_events_in(left.path(), name, 1);
    let all = read_all_events_in(right.path(), name);

    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].r#type, "user.left-last");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].r#type, "user.right");
}

/// A non-blocking connect(2), so filling an accept queue cannot hang the test.
fn nonblocking_connect(path: &Path) -> std::io::Result<UnixStream> {
    // SAFETY: all-zero is a valid sockaddr_un.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    assert!(bytes.len() < addr.sun_path.len(), "socket path too long");
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = *src as libc::c_char;
    }
    // SAFETY: socket(2) takes no pointers.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
    // SAFETY: `fd` is a fresh descriptor nothing else owns.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    stream.set_nonblocking(true).unwrap();
    // SAFETY: `addr` is initialised and its full size is passed.
    let rc = unsafe {
        libc::connect(
            stream.as_raw_fd(),
            (&addr as *const libc::sockaddr_un).cast(),
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if rc == 0 {
        Ok(stream)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// `<name>.sock` listening with `listen(0)`, never accepting, its queue full:
/// a blocking connect(2) to it waits forever on Linux and is refused on macOS.
/// Keep both returned values alive for as long as the queue must stay full.
fn full_backlog(root: &TestRoot, name: &str) -> (UnixListener, Vec<UnixStream>) {
    let listener = root.listen(name);
    // SAFETY: listen(2) on a socket the listener owns; re-listening only
    // changes the backlog.
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
    let path = root.session_file(name, "sock");
    let mut queued = Vec::new();
    for _ in 0..1024 {
        match nonblocking_connect(&path) {
            Ok(stream) => queued.push(stream),
            Err(e) => {
                let full = if cfg!(target_os = "linux") {
                    libc::EAGAIN
                } else {
                    libc::ECONNREFUSED
                };
                assert_eq!(e.raw_os_error(), Some(full), "filling the backlog: {e}");
                return (listener, queued);
            }
        }
    }
    panic!("accept queue of {name} never filled");
}

/// One shared deadline bounds the whole batch: a daemon that answers, one that
/// accepts and never replies, one whose accept queue is full, and a missing
/// socket each get the result `query_stats_in` gives them, and the silent and
/// wedged ones cost the deadline once rather than each.
#[test]
fn batch_stats_bound_every_session_by_one_deadline() {
    let root = TestRoot::new();
    let answered = serve_once(
        root.listen("ok"),
        encode_status_response(&stats_body("ok-body")),
    );
    let silent = root.listen("silent");
    let (release, held) = std::sync::mpsc::channel::<()>();
    // Never joined: if the batch never connected, accept would block forever.
    std::thread::spawn(move || {
        let (stream, _) = silent.accept().expect("accept silent client");
        let _ = held.recv();
        drop(stream);
    });
    let (_backlog, _queued) = full_backlog(&root, "backlog");
    let names: Vec<String> = ["ok", "silent", "backlog", "missing"]
        .map(String::from)
        .to_vec();

    let deadline = Duration::from_millis(500);
    let start = Instant::now();
    let results = query_stats_batch_in(root.path(), &names, deadline);
    let elapsed = start.elapsed();
    drop(release);

    assert!(
        elapsed >= deadline && elapsed < deadline + Duration::from_millis(500),
        "{elapsed:?}"
    );
    assert_eq!(
        results.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        names
    );
    assert_eq!(results[0].1.as_ref().expect("ok answers").name, "ok-body");
    assert_eq!(
        results[1].1.as_ref().err(),
        Some(&ClientError::StatsTimeout("silent".into()))
    );
    let backlog = if cfg!(target_os = "linux") {
        ClientError::StatsTimeout("backlog".into())
    } else {
        ClientError::NotReachable {
            name: "backlog".into(),
            remote: false,
        }
    };
    assert_eq!(results[2].1.as_ref().err(), Some(&backlog));
    assert_eq!(
        results[3].1.as_ref().err(),
        Some(&ClientError::NotReachable {
            name: "missing".into(),
            remote: false,
        })
    );
    assert_eq!(answered.join().unwrap().type_, MessageType::Status);
}

/// A peer that streams DATA without pause (the daemon broadcasts DATA to
/// command-role clients) never makes its socket unreadable; it must neither
/// outlive the deadline nor starve the session next to it.
#[test]
fn batch_stats_bound_a_peer_that_floods_data() {
    let root = TestRoot::new();
    let answered = serve_once(
        root.listen("ok"),
        encode_status_response(&stats_body("ok-body")),
    );
    let flood = root.listen("flood");
    let flooder = std::thread::spawn(move || {
        let (mut stream, _) = flood.accept().expect("accept flooded client");
        // Tiny packets make the reader's per-packet work outweigh the writer's.
        let chunk: Vec<u8> = std::iter::repeat_n(encode_data(b"x"), 16 * 1024)
            .flatten()
            .collect();
        // Ends once the batch drops its socket (EPIPE).
        while stream.write_all(&chunk).is_ok() {}
    });
    let names: Vec<String> = ["flood", "ok"].map(String::from).to_vec();

    let deadline = Duration::from_millis(500);
    let start = Instant::now();
    let results = query_stats_batch_in(root.path(), &names, deadline);
    let elapsed = start.elapsed();

    assert!(
        elapsed < deadline + Duration::from_millis(500),
        "{elapsed:?}"
    );
    assert_eq!(
        results[0].1.as_ref().err(),
        Some(&ClientError::StatsTimeout("flood".into()))
    );
    assert_eq!(results[1].1.as_ref().expect("ok answers").name, "ok-body");
    assert_eq!(answered.join().unwrap().type_, MessageType::Status);
    drop(results);
    flooder.join().unwrap();
}

/// A flooding peer returns every poll at once; a session with a full accept
/// queue beside it is still retried at most once per 10 ms retry tick, not on
/// every round the flood drives.
#[test]
fn batch_stats_throttle_busy_retries_beside_a_flooding_peer() {
    let root = TestRoot::new();
    let flood = root.listen("flood");
    let flooder = std::thread::spawn(move || {
        let (mut stream, _) = flood.accept().expect("accept flooded client");
        let chunk: Vec<u8> = std::iter::repeat_n(encode_data(b"x"), 16 * 1024)
            .flatten()
            .collect();
        // Ends once the batch drops its socket (EPIPE).
        while stream.write_all(&chunk).is_ok() {}
    });
    let (_backlog, _queued) = full_backlog(&root, "backlog");
    let names: Vec<String> = ["flood", "backlog"].map(String::from).to_vec();

    let deadline = Duration::from_millis(500);
    let before = busy_connects_on_this_thread();
    let results = query_stats_batch_in(root.path(), &names, deadline);
    let busy = busy_connects_on_this_thread() - before;

    // Attempts are at least one 10 ms tick apart and stop at the deadline:
    // one at entry, one per tick, and slack for the round that crosses it.
    let bound = (deadline.as_millis() / 10) as u64 + 2;
    assert!(busy <= bound, "{busy} busy connects, bound {bound}");
    if cfg!(target_os = "linux") {
        assert!(busy >= 1, "the full queue was never tried");
    }
    assert_eq!(
        results[0].1.as_ref().err(),
        Some(&ClientError::StatsTimeout("flood".into()))
    );
    drop(results);
    flooder.join().unwrap();
}

/// A deadline of exactly one retry tick (10 ms) makes the busy retry fall due
/// at or just after the deadline: the round that wakes there must not
/// connect again, so the full queue is tried once and reports
/// `StatsTimeout`, never a post-deadline connect outcome such as
/// `NotReachable`. Repeated so a lucky scheduling cannot hide a late retry.
#[test]
fn batch_stats_never_retry_a_busy_connect_at_the_deadline() {
    if !cfg!(target_os = "linux") {
        return; // macOS refuses a full queue outright; nothing is retried.
    }
    let root = TestRoot::new();
    let (_backlog, _queued) = full_backlog(&root, "backlog");
    let names = vec!["backlog".to_string()];
    for _ in 0..5 {
        let before = busy_connects_on_this_thread();
        let results = query_stats_batch_in(root.path(), &names, Duration::from_millis(10));
        assert_eq!(busy_connects_on_this_thread() - before, 1);
        assert_eq!(
            results[0].1.as_ref().err(),
            Some(&ClientError::StatsTimeout("backlog".into()))
        );
    }
}

/// The probe loop has the same bound: a busy retry due at the deadline is not
/// attempted, and the wedged socket stays absent.
#[test]
fn socket_probe_never_retries_a_busy_connect_at_the_deadline() {
    if !cfg!(target_os = "linux") {
        return; // macOS refuses a full queue outright; nothing is retried.
    }
    let root = TestRoot::new();
    let (_backlog, _queued) = full_backlog(&root, "backlog");
    let paths = vec![root.session_file("backlog", "sock")];
    for _ in 0..5 {
        let before = busy_connects_on_this_thread();
        let results = probe_sockets_within_budget(&paths, Duration::from_millis(10));
        assert_eq!(busy_connects_on_this_thread() - before, 1);
        assert_eq!(results.get(&paths[0]), None);
    }
}

/// The multiplexed probe answers what a blocking connect answers, and leaves a
/// listener that cannot accept unanswered (Linux) instead of waiting on it;
/// the listing classifies each accordingly.
#[test]
fn socket_probe_matches_a_blocking_connect() {
    let root = TestRoot::new();
    let _live = root.listen("live");
    // The socket file outlives its listener: connect is refused.
    drop(root.listen("stale"));
    let (_backlog, _queued) = full_backlog(&root, "backlog");
    let answerable: Vec<PathBuf> = ["live", "stale", "missing"]
        .iter()
        .map(|n| root.session_file(n, "sock"))
        .collect();
    let backlog = root.session_file("backlog", "sock");
    let mut paths = answerable.clone();
    paths.push(backlog.clone());

    let budget = Duration::from_millis(300);
    let start = Instant::now();
    let results = probe_sockets_within_budget(&paths, budget);
    assert!(
        start.elapsed() < budget + Duration::from_millis(500),
        "{:?}",
        start.elapsed()
    );
    for path in &answerable {
        assert_eq!(
            results.get(path),
            Some(&UnixStream::connect(path).is_ok()),
            "{}",
            path.display()
        );
    }
    let wedged = if cfg!(target_os = "linux") {
        None
    } else {
        Some(&false)
    };
    assert_eq!(results.get(&backlog), wedged);

    // A dead pid sends every listed session through the probe.
    for name in ["live", "stale", "backlog"] {
        std::fs::write(root.session_file(name, "json"), metadata(name)).unwrap();
        std::fs::write(root.session_file(name, "pid"), "2147483646").unwrap();
    }
    let listed: Vec<(String, SessionStatus)> = list_sessions_in(
        root.path(),
        &ListOptions {
            socket_probe_budget: budget,
        },
    )
    .into_iter()
    .map(|s| (s.name, s.status))
    .collect();
    assert_eq!(
        listed,
        [
            ("backlog".to_string(), SessionStatus::Vanished),
            ("live".to_string(), SessionStatus::Running),
            ("stale".to_string(), SessionStatus::Vanished),
        ]
    );
}

#[test]
fn output_activity_reads_only_the_selected_roots_generation() {
    let root = TestRoot::new();
    let dir = root.path().join(".activity");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("one.json"),
        serde_json::to_vec(&json!({"generation": "g1", "lastOutputAtMs": 42})).unwrap(),
    )
    .unwrap();
    let metadata = |generation: &str| SessionMetadata {
        generation: Some(generation.into()),
        ..SessionMetadata::default()
    };

    assert_eq!(
        last_output_at_ms_in(root.path(), "one", &metadata("g1")),
        Some(42)
    );
    assert_eq!(
        last_output_at_ms_in(root.path(), "one", &metadata("g2")),
        None
    );
    assert_eq!(
        last_output_at_ms_in(root.path(), "two", &metadata("g1")),
        None
    );
}
