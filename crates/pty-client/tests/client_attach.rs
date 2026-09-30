//! `pty attach` in terminal mode against a fake daemon: the exact stdout
//! texts (`client.ts:642-657`, `:503-523`), the detach key handling, the error
//! mapping, and the reconnect status lines.

mod common;

use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::thread::JoinHandle;
use std::time::Duration;

use common::*;
use pty_client::attach::{AttachOutcome, AttachParams, Reconnect, attach};
use pty_client::summary::SessionSummary;
use pty_client::{
    CURSOR_TO_BOTTOM, ClientError, ClientIo, RouteRefusedError, TERMINAL_SANITIZE, connect_session,
};
use pty_core::protocol::{
    MessageType, PacketReader, encode_data, encode_exit, encode_geometry, encode_screen,
};

const T: Duration = Duration::from_secs(5);

struct Run {
    stdin: Option<OwnedFd>,
    stdout: Collector,
    stderr: Collector,
    handle: JoinHandle<AttachOutcome>,
}

fn start(socket: UnixStream, reconnect: Option<Reconnect>) -> Run {
    start_with(socket, move |params| params.reconnect = reconnect)
}

fn start_with(
    socket: UnixStream,
    configure: impl FnOnce(&mut AttachParams) + Send + 'static,
) -> Run {
    let stdin = pipe();
    let stdout = pipe();
    let stderr = pipe();
    let io = ClientIo {
        stdin: stdin.r.as_raw_fd(),
        stdout: stdout.w.as_raw_fd(),
        stderr: stderr.w.as_raw_fd(),
    };
    let keep = (stdin.r, stdout.w, stderr.w);
    let handle = std::thread::spawn(move || {
        let _keep = keep;
        let mut params = AttachParams::new("demo", socket);
        params.max_reconnect_attempts = None;
        configure(&mut params);
        attach(params, &io)
    });
    Run {
        stdin: Some(stdin.w),
        stdout: collect(stdout.r),
        stderr: collect(stderr.r),
        handle,
    }
}

impl Run {
    fn type_stdin(&self, bytes: &[u8]) {
        pty_client::tty::write_all_fd(self.stdin.as_ref().unwrap().as_raw_fd(), bytes)
            .unwrap();
    }
    fn finish(mut self) -> (AttachOutcome, String, String) {
        let outcome = join_within(self.handle, T, "attach");
        drop(self.stdin.take());
        (
            outcome,
            String::from_utf8_lossy(&self.stdout.finish()).into_owned(),
            String::from_utf8_lossy(&self.stderr.finish()).into_owned(),
        )
    }
}

fn daemon(f: impl FnOnce(UnixStream) + Send + 'static) -> (FakeDaemon, JoinHandle<()>) {
    let d = FakeDaemon::bind("att");
    let listener = d.listener.try_clone().unwrap();
    let h = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let first = read_chunk(&mut s);
        assert_eq!(types(&first), vec![MessageType::Attach]);
        f(s);
    });
    (d, h)
}

/// node: client.ts:642-657 — SCREEN clears first; DATA is raw; EXIT prints the
/// sanitize string, cursor-to-bottom and the exit line, then exits with the code.
#[test]
fn screen_data_and_exit_texts() {
    let (d, h) = daemon(|mut s| {
        use std::io::Write;
        s.write_all(&concat(&[
            encode_geometry(24, 80),
            encode_screen(b"$ hello"),
            encode_data(b"\r\nmore"),
            encode_exit(7),
        ]))
        .unwrap();
    });
    let (outcome, out, err) = start(d.connect(), None).finish();
    assert_eq!(outcome, AttachOutcome::Exited(7));
    assert_eq!(
        out,
        format!(
            "\x1b[2J\x1b[H$ hello\r\nmore{TERMINAL_SANITIZE}{CURSOR_TO_BOTTOM}\r\n[demo exited with code 7]\r\n"
        )
    );
    assert!(err.is_empty());
    h.join().unwrap();
}

/// node: client.ts:503-523, :540-569 — a single Ctrl+\ detaches after the
/// 300 ms window: DETACH to the daemon, then the detach trailer.
#[test]
fn single_detach_key_detaches_and_prints_the_detached_line() {
    let (d, h) = daemon(|mut s| {
        use std::io::Write;
        s.write_all(&concat(&[encode_geometry(24, 80), encode_screen(b"ready")]))
            .unwrap();
        let rest = read_packets_until_eof(&mut s, T);
        assert_eq!(
            rest.iter().map(|p| p.type_).collect::<Vec<_>>(),
            vec![MessageType::Data, MessageType::Detach]
        );
        assert_eq!(rest[0].payload, b"ab");
    });
    let run = start(d.connect(), None);
    run.stdout.wait_for(T, |b| b.ends_with(b"ready"));
    run.type_stdin(b"ab\x1c");
    let (outcome, out, err) = run.finish();
    assert_eq!(outcome, AttachOutcome::Detached);
    assert_eq!(
        out,
        format!(
            "\x1b[2J\x1b[Hready{TERMINAL_SANITIZE}{CURSOR_TO_BOTTOM}\r\n[detached from demo]\r\n  reattach: pty attach demo\r\n"
        )
    );
    assert!(err.is_empty());
    h.join().unwrap();
}

/// The detach trailer shows what the provider knows when the detach fires,
/// and a remote session's hint names its peer.
#[test]
fn detach_trailer_shows_the_session_summary_at_detach_time() {
    let (d, h) = daemon(|mut s| {
        use std::io::Write;
        s.write_all(&concat(&[encode_geometry(24, 80), encode_screen(b"ready")]))
            .unwrap();
        read_packets_until_eof(&mut s, T);
    });
    let run = start_with(d.connect(), |params| {
        params.peer = Some("box".into());
        let mut calls = 0;
        params.summary = Some(Box::new(move || {
            calls += 1;
            Some(SessionSummary {
                id: "demo".into(),
                display_name: Some(format!("Demo {calls}")),
                command: Some("cat".into()),
                ..Default::default()
            })
        }));
    });
    run.stdout.wait_for(T, |b| b.ends_with(b"ready"));
    run.type_stdin(b"\x1c");
    let (outcome, out, _) = run.finish();
    assert_eq!(outcome, AttachOutcome::Detached);
    assert_eq!(
        out,
        format!(
            "\x1b[2J\x1b[Hready{TERMINAL_SANITIZE}{CURSOR_TO_BOTTOM}\r\n[detached from demo]\r\n  \x1b[1mDemo 1\x1b[0m \x1b[2m(demo)\x1b[0m —  — \x1b[2mcat\x1b[0m\r\n  reattach: pty attach --remote box demo\r\n"
        )
    );
    h.join().unwrap();
}

/// node: client.ts:20-31, :551-556 — a double tap forwards a literal 0x1c and
/// cancels the detach; the kitty encoding counts as the same key.
#[test]
fn double_tap_forwards_ctrl_backslash_and_kitty_encoding_is_normalized() {
    let (d, h) = daemon(|mut s| {
        use std::io::Write;
        s.write_all(&concat(&[encode_geometry(24, 80), encode_screen(b"ready")]))
            .unwrap();
        let mut reader = PacketReader::new();
        let got = read_until(&mut s, &mut reader, MessageType::Data, T);
        assert_eq!(got.last().unwrap().payload, vec![0x1c]);
        s.write_all(&encode_exit(0)).unwrap();
    });
    let run = start(d.connect(), None);
    run.stdout.wait_for(T, |b| b.ends_with(b"ready"));
    run.type_stdin(b"\x1c");
    run.type_stdin(b"\x1b[92;5u");
    let (outcome, _, _) = run.finish();
    assert_eq!(outcome, AttachOutcome::Exited(0));
    h.join().unwrap();
}

/// node: client.ts:686-690 — a close without error and without EXIT ends
/// with the last known code (0), after saying the session ended.
#[test]
fn close_without_exit_says_the_session_ended_and_exits_0() {
    let (d, h) = daemon(|mut s| {
        use std::io::Write;
        s.write_all(&concat(&[encode_geometry(24, 80), encode_screen(b"x")]))
            .unwrap();
    });
    let (outcome, out, err) = start(d.connect(), None).finish();
    assert_eq!(outcome, AttachOutcome::Exited(0));
    assert_eq!(
        out,
        format!("\x1b[2J\x1b[Hx{TERMINAL_SANITIZE}{CURSOR_TO_BOTTOM}\r\n[demo session ended]\r\n")
    );
    assert!(err.is_empty());
    h.join().unwrap();
}

/// Does closing a socket that still holds unread data reach the peer as a
/// reset, or as an ordinary end of stream?
///
/// **The two kernels we run on disagree, so the test below asks rather than
/// assumes.** Linux hands the peer the bytes that were already in flight and
/// then fails its next read with `ECONNRESET`. Measured 2026-09-02: errno 104
/// after the data. On Apple silicon the same sequence ends in a plain end of
/// stream, which this test found on 2026-09-02 by failing there five times
/// out of five, natively and under nix.
///
/// **This is not a difference between the two pty implementations.** The Node
/// client decides the same way, on `err.code === "ECONNRESET"`
/// (`src/client.ts`), so it reaches the same two answers on the same two
/// kernels. Neither client can report a reset that its kernel never
/// delivered.
///
/// **The consequence is worth knowing where the reset does not arrive:** a
/// daemon that drops a client with input still unread is indistinguishable
/// from one that closed politely, so `pty attach` ends quietly with the last
/// known code instead of saying the session is gone. Every other way of
/// losing a daemon — it exits, it is killed, its socket disappears — is
/// reported the same way on both.
fn close_with_unread_data_reaches_the_peer_as_a_reset() -> bool {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    let (mut kept, mut dropped) = UnixStream::pair().expect("socket pair");
    kept.write_all(b"a").expect("write");
    dropped.write_all(b"b").expect("write");
    // `kept` never reads what `dropped` sent, so the close below happens with
    // data still pending, which is the condition that provokes a reset.
    std::thread::sleep(Duration::from_millis(50));
    drop(dropped);
    let mut buf = [0u8; 64];
    loop {
        match kept.read(&mut buf) {
            Ok(0) => return false,
            Ok(_) => continue,
            Err(_) => return true,
        }
    }
}

/// node: client.ts:672-681 — a reset maps to the not-found text.
#[test]
fn reset_maps_to_not_found_or_not_running() {
    let (d, h) = daemon(|mut s| {
        use std::io::Write;
        s.write_all(&concat(&[encode_geometry(24, 80), encode_screen(b"x")]))
            .unwrap();
        wait_unread(&s, T);
        drop(s);
    });
    let run = start(d.connect(), None);
    run.stdout.wait_for(T, |b| b.ends_with(b"x"));
    run.type_stdin(b"typed");
    let (outcome, _, err) = run.finish();
    if close_with_unread_data_reaches_the_peer_as_a_reset() {
        assert_eq!(outcome, AttachOutcome::Exited(1));
        assert_eq!(err, "Session \"demo\" not found or not running.\n");
    } else {
        // This kernel gave the client an ordinary end of stream, so there was
        // no reset to report. Pinned rather than skipped, so that a change in
        // either the kernel or the client is still caught here.
        assert_eq!(outcome, AttachOutcome::Exited(0), "stderr: {err:?}");
        assert!(err.is_empty(), "{err:?}");
    }
    h.join().unwrap();
}

/// node: client.ts:589-594 — an oversize frame is reported and the socket
/// dropped; with no reconnect that is a silent close (code 0).
#[test]
fn oversize_packet_prints_the_dropping_line() {
    let (d, h) = daemon(|mut s| {
        use std::io::Write;
        let mut bad = vec![0u8];
        bad.extend_from_slice(&0xffff_ffffu32.to_be_bytes());
        s.write_all(&bad).unwrap();
        let _ = read_packets_until_eof(&mut s, T);
    });
    let (outcome, out, err) = start(d.connect(), None).finish();
    assert_eq!(outcome, AttachOutcome::Exited(0));
    assert!(out.is_empty());
    assert_eq!(
        err,
        "pty client: dropping connection — Packet length 4294967295 exceeds maximum 33554432\n"
    );
    h.join().unwrap();
}

/// node: client.ts:709-729 — the reconnect status line goes to stdout, and a
/// refused route prints `[<name> session ended]` and exits 0.
#[test]
fn reconnect_refusal_prints_session_ended_and_exits_0() {
    let (d, h) = daemon(|mut s| {
        use std::io::Write;
        s.write_all(&concat(&[encode_geometry(24, 80), encode_screen(b"x")]))
            .unwrap();
    });
    let dial: Reconnect = Box::new(|| Err(RouteRefusedError("session \"demo\" not found".into())));
    let (outcome, out, err) = start(d.connect(), Some(dial)).finish();
    assert_eq!(outcome, AttachOutcome::Exited(0));
    assert_eq!(
        out,
        format!(
            "\x1b[2J\x1b[Hx\r\n[reconnecting… — Ctrl-\\ or Ctrl-C to stop]\r\n{TERMINAL_SANITIZE}{CURSOR_TO_BOTTOM}\r\n[demo session ended]\r\n"
        )
    );
    assert!(err.is_empty());
    h.join().unwrap();
}

/// node: client.ts:731-735 — a reconnect re-ATTACHes and the fresh SCREEN
/// clears and repaints.
#[test]
fn reconnect_replays_the_fresh_screen() {
    let d = FakeDaemon::bind("att-re");
    let listener = d.listener.try_clone().unwrap();
    let h = std::thread::spawn(move || {
        use std::io::Write;
        for n in 1..=2 {
            let (mut s, _) = listener.accept().unwrap();
            assert_eq!(types(&read_chunk(&mut s)), vec![MessageType::Attach]);
            if n == 1 {
                s.write_all(&concat(&[encode_geometry(24, 80), encode_screen(b"one")]))
                    .unwrap();
            } else {
                s.write_all(&concat(&[
                    encode_geometry(24, 80),
                    encode_screen(b"two"),
                    encode_exit(3),
                ]))
                .unwrap();
            }
        }
    });
    let path = d.path.clone();
    let dial: Reconnect = Box::new(move || Ok(UnixStream::connect(&path).ok()));
    let (outcome, out, _) = start(d.connect(), Some(dial)).finish();
    assert_eq!(outcome, AttachOutcome::Exited(3));
    assert_eq!(
        out,
        format!(
            "\x1b[2J\x1b[Hone\r\n[reconnecting… — Ctrl-\\ or Ctrl-C to stop]\r\n\x1b[2J\x1b[Htwo{TERMINAL_SANITIZE}{CURSOR_TO_BOTTOM}\r\n[demo exited with code 3]\r\n"
        )
    );
    h.join().unwrap();
}

/// node: client.ts:672-681 — a missing socket is the not-found error before
/// anything is attached.
#[test]
fn connect_session_maps_a_missing_socket() {
    test_root();
    let err = connect_session("no-such-session").unwrap_err();
    assert_eq!(
        err,
        ClientError::NotReachable {
            name: "no-such-session".into(),
            remote: false
        }
    );
    assert_eq!(
        err.to_string(),
        "Session \"no-such-session\" not found or not running."
    );
    assert_eq!(
        ClientError::NotReachable {
            name: "r".into(),
            remote: true
        }
        .to_string(),
        "Remote session \"r\" not found or not running."
    );
    assert_eq!(
        ClientError::Connection("connect EACCES /x.sock".into()).to_string(),
        "Connection error: connect EACCES /x.sock"
    );
}
