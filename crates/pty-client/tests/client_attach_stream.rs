//! `attach --attach-stream-fd-v1`: the machine stream, driven against a fake
//! daemon that scripts packet sequences. Port of `tests/attach-stream.test.ts`
//! (the parts that exercise `attach()` directly; the CLI-level cases live in
//! the conformance suite).

mod common;

use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::thread::JoinHandle;
use std::time::Duration;

use common::*;
use pty_client::ClientIo;
use pty_client::attach::{AttachOutcome, AttachParams, Reconnect, attach};
use pty_client::stream::{parse_attach_stream_fd_token, validate_attach_stream_fd};
use pty_core::protocol::{
    MessageType, decode_size, encode_data, encode_exit, encode_geometry, encode_screen,
};

const T: Duration = Duration::from_secs(5);

struct Run {
    stdin: Option<OwnedFd>,
    stdout: Collector,
    stderr: Collector,
    stream: Option<Collector>,
    handle: JoinHandle<AttachOutcome>,
}

struct Done {
    outcome: AttachOutcome,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stream: Vec<u8>,
}

/// Start `attach` on a thread with pipes for stdin/stdout/stderr and, in
/// machine mode, fd "3" (a pipe whose write end plays the inherited fd).
/// `break_stream` closes the read end at once so the first write gets EPIPE.
fn start(
    socket: UnixStream,
    machine: bool,
    reconnect: Option<Reconnect>,
    max_attempts: Option<usize>,
    break_stream: bool,
) -> Run {
    let stdin = pipe();
    let stdout = pipe();
    let stderr = pipe();
    let stream = machine.then(pipe);
    let io = ClientIo {
        stdin: stdin.r.as_raw_fd(),
        stdout: stdout.w.as_raw_fd(),
        stderr: stderr.w.as_raw_fd(),
    };
    let stream_fd = stream.as_ref().map(|p| p.w.as_raw_fd());
    let (stream_r, stream_w) = match stream {
        Some(p) => (Some(p.r), Some(p.w)),
        None => (None, None),
    };
    let keep = (stdin.r, stdout.w, stderr.w, stream_w);
    let handle = std::thread::spawn(move || {
        let _keep = keep;
        let params = AttachParams {
            name: "fixture",
            socket,
            reconnect,
            stream_fd,
            max_reconnect_attempts: max_attempts,
            peer: None,
            summary: None,
        };
        attach(params, &io)
    });
    let stream = match stream_r {
        Some(r) if break_stream => {
            drop(r);
            None
        }
        Some(r) => Some(collect(r)),
        None => None,
    };
    Run {
        stdin: Some(stdin.w),
        stdout: collect(stdout.r),
        stderr: collect(stderr.r),
        stream,
        handle,
    }
}

impl Run {
    fn type_stdin(&self, bytes: &[u8]) {
        pty_client::tty::write_all_fd(self.stdin.as_ref().unwrap().as_raw_fd(), bytes)
            .unwrap();
    }

    fn stream(&self) -> &Collector {
        self.stream.as_ref().expect("machine mode")
    }

    fn finish(mut self) -> Done {
        let outcome = join_within(self.handle, T, "attach");
        drop(self.stdin.take());
        Done {
            outcome,
            stdout: self.stdout.finish(),
            stderr: self.stderr.finish(),
            stream: self.stream.map(|c| c.finish()).unwrap_or_default(),
        }
    }
}

/// A daemon that waits for the client's ATTACH and then runs `script`.
fn daemon_after_attach(
    f: impl FnOnce(UnixStream) + Send + 'static,
) -> (FakeDaemon, JoinHandle<()>) {
    let d = FakeDaemon::bind("stream");
    let listener = d.listener.try_clone().unwrap();
    let h = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let first = read_chunk(&mut s);
        assert_eq!(types(&first), vec![MessageType::Attach]);
        f(s);
    });
    (d, h)
}

fn stderr_text(d: &Done) -> String {
    String::from_utf8_lossy(&d.stderr).into_owned()
}

/// node: tests/attach-stream.test.ts:241-269
#[test]
fn frames_an_intentional_detach_before_the_daemon_baseline() {
    let (d, h) = daemon_after_attach(|s| {
        // Hold the socket open until the client goes away.
        let mut s = s;
        let _ = read_packets_until_eof(&mut s, T);
    });
    let run = start(d.connect(), true, None, None, false);
    std::thread::sleep(Duration::from_millis(50));
    run.type_stdin(&[0x1c]);
    let done = run.finish();
    assert_eq!(done.outcome, AttachOutcome::Detached);
    assert!(done.stdout.is_empty());
    assert!(done.stderr.is_empty());
    assert_eq!(types(&done.stream), vec![MessageType::Detach]);
    h.join().unwrap();
}

/// node: tests/attach-stream.test.ts:183-239
#[test]
fn frames_a_local_detach_after_the_baseline_and_sends_detach_to_the_daemon() {
    let (d, h) = daemon_after_attach(|mut s| {
        use std::io::Write;
        s.write_all(&concat(&[
            encode_geometry(24, 80),
            encode_screen(b"DETACH_READY"),
        ]))
        .unwrap();
        let rest = read_packets_until_eof(&mut s, T);
        assert_eq!(rest.last().map(|p| p.type_), Some(MessageType::Detach));
    });
    let run = start(d.connect(), true, None, None, false);
    run.stream().wait_for_packet(MessageType::Screen, T);
    run.type_stdin(&[0x1c]);
    let done = run.finish();
    assert_eq!(done.outcome, AttachOutcome::Detached);
    assert!(done.stdout.is_empty());
    assert!(done.stderr.is_empty());
    let ps = packets(&done.stream);
    assert_eq!(ps[0].type_, MessageType::Geometry);
    assert_eq!(ps[1].type_, MessageType::Screen);
    assert_eq!(ps[1].payload, b"DETACH_READY");
    assert_eq!(ps.last().unwrap().type_, MessageType::Detach);
    assert!(!ps.iter().any(|p| p.type_ == MessageType::Exit));
    h.join().unwrap();
}

/// node: tests/attach-stream.test.ts:271-325
#[test]
fn exit_inside_the_detach_window_wins() {
    let (d, h) = daemon_after_attach(|mut s| {
        use std::io::Write;
        s.write_all(&concat(&[
            encode_geometry(24, 80),
            encode_screen(b"exit wins"),
        ]))
        .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        s.write_all(&encode_exit(0)).unwrap();
    });
    let run = start(d.connect(), true, None, None, false);
    run.stream().wait_for_packet(MessageType::Screen, T);
    run.type_stdin(&[0x1c]);
    let done = run.finish();
    assert_eq!(done.outcome, AttachOutcome::Exited(0));
    assert!(done.stdout.is_empty());
    assert!(done.stderr.is_empty());
    assert_eq!(
        types(&done.stream),
        vec![
            MessageType::Geometry,
            MessageType::Screen,
            MessageType::Exit
        ]
    );
    h.join().unwrap();
}

#[test]
fn typed_rejection_fails_the_machine_transport_without_fabricating_exit() {
    let (d, h) = daemon_after_attach(|mut socket| {
        use std::io::Write;
        socket.write_all(&concat(&[
            encode_geometry(24, 80),
            encode_screen(b"live"),
            pty_core::protocol::encode_packet(MessageType::Unknown(12), &[0]),
        ])).unwrap();
    });
    let done = start(d.connect(), true, None, None, false).finish();
    assert_eq!(done.outcome, AttachOutcome::Exited(1));
    assert!(done.stdout.is_empty());
    assert!(stderr_text(&done).contains("client too slow"));
    assert!(stderr_text(&done).contains("connection lost"));
    assert!(!stderr_text(&done).contains("session ended"));
    assert_eq!(types(&done.stream), vec![MessageType::Geometry, MessageType::Screen]);
    h.join().unwrap();
}

/// node: tests/attach-stream.test.ts:327-351
#[test]
fn reframes_fragmented_and_coalesced_packets_in_order() {
    let geometry = encode_geometry(31, 97);
    let screen = encode_screen(b"\x1b[31mred\x1b[0m");
    let data = encode_data(b"live");
    let exit = encode_exit(0);
    let (d, h) = daemon_after_attach(move |mut s| {
        use std::io::Write;
        s.write_all(&geometry[..2]).unwrap();
        s.flush().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let mut rest = geometry[2..].to_vec();
        rest.extend_from_slice(&screen);
        rest.extend_from_slice(&data);
        rest.extend_from_slice(&exit);
        s.write_all(&rest).unwrap();
    });
    let done = start(d.connect(), true, None, None, false).finish();
    assert_eq!(done.outcome, AttachOutcome::Exited(0));
    assert!(done.stdout.is_empty());
    assert!(done.stderr.is_empty());
    let ps = packets(&done.stream);
    assert_eq!(
        ps.iter().map(|p| p.type_).collect::<Vec<_>>(),
        vec![
            MessageType::Geometry,
            MessageType::Screen,
            MessageType::Data,
            MessageType::Exit
        ]
    );
    assert_eq!(decode_size(&ps[0].payload), (31, 97));
    assert_eq!(ps[1].payload, b"\x1b[31mred\x1b[0m");
    assert_eq!(ps[2].payload, b"live");
    h.join().unwrap();
}

/// node: tests/attach-stream.test.ts:353-402 — the ATTACH carries the stdout
/// size; a pipe is not a tty, so the 24×80 default goes out and nothing is
/// painted on stdout.
#[test]
fn attach_advertises_the_stdout_geometry_and_paints_nothing() {
    let d = FakeDaemon::bind("geom");
    let listener = d.listener.try_clone().unwrap();
    let h = std::thread::spawn(move || {
        use std::io::Write;
        let (mut s, _) = listener.accept().unwrap();
        let first = packets(&read_chunk(&mut s));
        assert_eq!(first[0].type_, MessageType::Attach);
        let (rows, cols) = decode_size(&first[0].payload);
        s.write_all(&concat(&[
            encode_geometry(rows, cols),
            encode_screen(b"screen stays off stdout"),
            encode_exit(0),
        ]))
        .unwrap();
        (rows, cols)
    });
    let done = start(d.connect(), true, None, None, false).finish();
    assert_eq!(h.join().unwrap(), (24, 80));
    assert_eq!(done.outcome, AttachOutcome::Exited(0));
    assert!(done.stdout.is_empty());
    assert_eq!(
        types(&done.stream),
        vec![
            MessageType::Geometry,
            MessageType::Screen,
            MessageType::Exit
        ]
    );
}

/// node: tests/attach-stream.test.ts:404-413
#[test]
fn legacy_daemon_that_sends_screen_first_is_refused_with_an_empty_stream() {
    let (d, h) = daemon_after_attach(|mut s| {
        use std::io::Write;
        s.write_all(&encode_screen(b"legacy screen")).unwrap();
        let _ = read_packets_until_eof(&mut s, T);
    });
    let done = start(d.connect(), true, None, None, false).finish();
    assert_eq!(done.outcome, AttachOutcome::Exited(1));
    assert!(done.stdout.is_empty());
    assert!(done.stream.is_empty());
    assert_eq!(
        stderr_text(&done),
        "pty attach: daemon does not support attach stream v1 (expected GEOMETRY before terminal events)\n"
    );
    h.join().unwrap();
}

/// node: tests/attach-stream.test.ts:415-432
#[test]
fn data_or_exit_before_the_initial_screen_is_rejected() {
    for (premature, what) in [
        (encode_data(b"too early"), "DATA"),
        (encode_exit(0), "EXIT"),
    ] {
        let (d, h) = daemon_after_attach(move |mut s| {
            use std::io::Write;
            s.write_all(&concat(&[encode_geometry(24, 80), premature]))
                .unwrap();
            let _ = read_packets_until_eof(&mut s, T);
        });
        let done = start(d.connect(), true, None, None, false).finish();
        assert_eq!(done.outcome, AttachOutcome::Exited(1));
        assert!(done.stdout.is_empty());
        assert_eq!(
            stderr_text(&done),
            format!(
                "pty attach: daemon does not support attach stream v1 (expected SCREEN before {what})\n"
            )
        );
        assert_eq!(types(&done.stream), vec![MessageType::Geometry]);
        h.join().unwrap();
    }
}

/// node: tests/attach-stream.test.ts:434-446
#[test]
fn close_without_exit_is_a_truncated_stream() {
    let (d, h) = daemon_after_attach(|mut s| {
        use std::io::Write;
        s.write_all(&concat(&[
            encode_geometry(24, 80),
            encode_screen(b"truncated"),
        ]))
        .unwrap();
    });
    let done = start(d.connect(), true, None, None, false).finish();
    assert_eq!(done.outcome, AttachOutcome::Exited(1));
    assert!(done.stdout.is_empty());
    assert_eq!(
        stderr_text(&done),
        "pty attach: machine stream truncated before EXIT: connection closed\n"
    );
    assert_eq!(
        types(&done.stream),
        vec![MessageType::Geometry, MessageType::Screen]
    );
    h.join().unwrap();
}

/// node: tests/attach-stream.test.ts:448-459 — a reset (the daemon closes with
/// our DATA unread) is a truncated stream, never "not found".
#[test]
fn transport_reset_is_a_truncated_stream_not_a_missing_session() {
    let (d, h) = daemon_after_attach(|mut s| {
        use std::io::Write;
        s.write_all(&concat(&[
            encode_geometry(24, 80),
            encode_screen(b"partial"),
        ]))
        .unwrap();
        // Close while the client's DATA sits unread → ECONNRESET on its read.
        wait_unread(&s, T);
        drop(s);
    });
    let run = start(d.connect(), true, None, None, false);
    run.stream().wait_for_packet(MessageType::Screen, T);
    run.type_stdin(b"typed");
    let done = run.finish();
    assert_eq!(done.outcome, AttachOutcome::Exited(1));
    assert!(done.stdout.is_empty());
    let err = stderr_text(&done);
    assert!(
        err.starts_with("pty attach: machine stream truncated before EXIT: "),
        "{err:?}"
    );
    assert!(!err.to_lowercase().contains("not found"), "{err:?}");
    h.join().unwrap();
}

/// node: tests/attach-stream.test.ts:461-493
#[test]
fn a_broken_stream_descriptor_fails_instead_of_hanging() {
    let (d, h) = daemon_after_attach(|mut s| {
        use std::io::Write;
        std::thread::sleep(Duration::from_millis(10));
        let _ = s.write_all(&encode_geometry(24, 80));
        let _ = s.write_all(&encode_screen(b"baseline"));
        let _ = s.write_all(&encode_data(&vec![65u8; 1024 * 1024]));
        let _ = read_packets_until_eof(&mut s, T);
    });
    let done = start(d.connect(), true, None, None, true).finish();
    assert_eq!(done.outcome, AttachOutcome::Exited(1));
    assert!(done.stdout.is_empty());
    let err = stderr_text(&done);
    assert!(
        err.starts_with("pty attach: machine stream descriptor "),
        "{err:?}"
    );
    assert!(err.contains(" failed: write EPIPE"), "{err:?}");
    h.join().unwrap();
}

fn serve_two_connections(second: Vec<u8>) -> (FakeDaemon, JoinHandle<()>) {
    let d = FakeDaemon::bind("reconn");
    let listener = d.listener.try_clone().unwrap();
    let h = std::thread::spawn(move || {
        use std::io::Write;
        for current in 1..=2 {
            let (mut s, _) = listener.accept().unwrap();
            let first = read_chunk(&mut s);
            assert_eq!(
                types(&first),
                vec![MessageType::Attach],
                "connection {current}"
            );
            if current == 1 {
                s.write_all(&concat(&[
                    encode_geometry(20, 70),
                    encode_screen(b"first"),
                    encode_data(b"before reconnect"),
                ]))
                .unwrap();
            } else {
                s.write_all(&second).unwrap();
            }
            // End the connection (Node `socket.end`).
            drop(s);
        }
    });
    (d, h)
}

/// node: tests/attach-stream.test.ts:495-561
#[test]
fn reconnect_keeps_one_stream_with_fresh_geometry() {
    let (d, h) = serve_two_connections(concat(&[
        encode_geometry(21, 71),
        encode_screen(b"second"),
        encode_data(b"after reconnect"),
        encode_exit(0),
    ]));
    let path = d.path.clone();
    let dial: Reconnect = Box::new(move || Ok(UnixStream::connect(&path).ok()));
    let done = start(d.connect(), true, Some(dial), None, false).finish();
    assert_eq!(done.outcome, AttachOutcome::Exited(0));
    assert!(done.stdout.is_empty());
    assert_eq!(
        stderr_text(&done),
        "\r\n[reconnecting… — Ctrl-\\ or Ctrl-C to stop]\r\n"
    );
    let ps = packets(&done.stream);
    assert_eq!(
        ps.iter().map(|p| p.type_).collect::<Vec<_>>(),
        vec![
            MessageType::Geometry,
            MessageType::Screen,
            MessageType::Data,
            MessageType::Geometry,
            MessageType::Screen,
            MessageType::Data,
            MessageType::Exit,
        ]
    );
    assert_eq!(decode_size(&ps[3].payload), (21, 71));
    h.join().unwrap();
}

/// node: tests/attach-stream.test.ts:563-618
#[test]
fn reconnect_requires_a_fresh_screen_after_geometry() {
    let (d, h) = serve_two_connections(concat(&[
        encode_geometry(21, 71),
        encode_data(b"too early"),
    ]));
    let path = d.path.clone();
    let dial: Reconnect = Box::new(move || Ok(UnixStream::connect(&path).ok()));
    let done = start(d.connect(), true, Some(dial), None, false).finish();
    assert_eq!(done.outcome, AttachOutcome::Exited(1));
    assert!(done.stdout.is_empty());
    assert!(stderr_text(&done).contains("expected SCREEN before DATA"));
    assert_eq!(
        types(&done.stream),
        vec![
            MessageType::Geometry,
            MessageType::Screen,
            MessageType::Data,
            MessageType::Geometry,
        ]
    );
    h.join().unwrap();
}

/// A refused route ends the machine stream as a transport failure.
#[test]
fn reconnect_refusal_ends_the_machine_stream_with_exit_1() {
    let (d, h) = daemon_after_attach(|mut s| {
        use std::io::Write;
        s.write_all(&concat(&[
            encode_geometry(24, 80),
            encode_screen(b"gone soon"),
        ]))
        .unwrap();
    });
    let dial: Reconnect = Box::new(|| {
        Err(pty_client::RouteRefusedError(
            "session \"fixture\" not found".into(),
        ))
    });
    let done = start(d.connect(), true, Some(dial), None, false).finish();
    assert_eq!(done.outcome, AttachOutcome::Exited(1));
    assert_eq!(
        stderr_text(&done),
        "\r\n[reconnecting… — Ctrl-\\ or Ctrl-C to stop]\r\n[connection lost to fixture]\n"
    );
    assert_eq!(
        types(&done.stream),
        vec![MessageType::Geometry, MessageType::Screen]
    );
    h.join().unwrap();
}

/// node: client.ts:736-746 — the attempt cap (`PTY_RECONNECT_MAX_ATTEMPTS`).
#[test]
fn reconnect_gives_up_after_the_attempt_cap() {
    let (d, h) = daemon_after_attach(|mut s| {
        use std::io::Write;
        s.write_all(&concat(&[encode_geometry(24, 80), encode_screen(b"x")]))
            .unwrap();
    });
    let dial: Reconnect = Box::new(|| Ok(None));
    let done = start(d.connect(), true, Some(dial), Some(2), false).finish();
    assert_eq!(done.outcome, AttachOutcome::Exited(1));
    assert_eq!(
        stderr_text(&done),
        "\r\n[reconnecting… — Ctrl-\\ or Ctrl-C to stop]\r\n[connection lost to fixture]\n"
    );
    h.join().unwrap();
}

/// node: tests/attach-stream.test.ts:105-124 (the messages `validateAttachStreamFdV1` throws)
#[test]
fn validates_the_inherited_descriptor() {
    assert_eq!(
        validate_attach_stream_fd(2).unwrap_err(),
        "--attach-stream-fd-v1 requires a dedicated inherited file descriptor >= 3 (got 2)"
    );
    let err = validate_attach_stream_fd(999999).unwrap_err();
    assert_eq!(
        err,
        "--attach-stream-fd-v1 descriptor 999999 is not writable: EBADF: bad file descriptor, fstat"
    );
    // A read-only descriptor fails the zero-length write probe.
    let p = pipe();
    let err = validate_attach_stream_fd(p.r.as_raw_fd() as i64).unwrap_err();
    assert!(err.ends_with("EBADF: bad file descriptor, write"), "{err}");
    assert_eq!(
        validate_attach_stream_fd(p.w.as_raw_fd() as i64).unwrap(),
        p.w.as_raw_fd()
    );

    assert_eq!(
        parse_attach_stream_fd_token("abc").unwrap_err(),
        "--attach-stream-fd-v1 requires a dedicated inherited file descriptor >= 3 (got NaN)"
    );
    assert_eq!(
        parse_attach_stream_fd_token("3.5").unwrap_err(),
        "--attach-stream-fd-v1 requires a dedicated inherited file descriptor >= 3 (got 3.5)"
    );
    assert_eq!(
        parse_attach_stream_fd_token("").unwrap_err(),
        "--attach-stream-fd-v1 requires a dedicated inherited file descriptor >= 3 (got 0)"
    );
    assert_eq!(
        parse_attach_stream_fd_token(&format!(" {} ", p.w.as_raw_fd())).unwrap(),
        p.w.as_raw_fd()
    );
}
