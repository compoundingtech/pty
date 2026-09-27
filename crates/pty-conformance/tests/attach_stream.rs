//! Port of tests/attach-stream.test.ts: `pty attach --attach-stream-fd-v1 <fd>`
//! re-frames GEOMETRY/SCREEN/DATA/EXIT to an inherited descriptor and paints
//! nothing on stdout. The real-daemon cases run against `pty run -d`; the
//! fake-daemon cases bind a Unix listener at `<root>/<id>.sock` next to a
//! hand-written `<id>.json` so the CLI resolves the session and connects to
//! our listener (Node used the `attach()` library over TCP for those).
//!
//! Left out: the two reconnect cases (lines 495, 563) — reconnect only exists
//! for `attach --remote`, which is covered with the fabric stub in the remote
//! suites.

use pty_conformance::*;
use pty_core::protocol::{
    MessageType, Packet, PacketReader, encode_exit, encode_geometry, encode_packet, encode_screen,
};
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FAKE: &str = "fx";

/// The CLI running with fd 3 bound to a pipe we read.
struct StreamCli {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout_rx: mpsc::Receiver<Vec<u8>>,
    stderr_rx: mpsc::Receiver<Vec<u8>>,
    stream_rx: Option<mpsc::Receiver<Vec<u8>>>,
    /// The read end, kept unread when the test wants to break the stream.
    stream_fd: Arc<Mutex<Option<OwnedFd>>>,
    stream: Vec<u8>,
}

impl StreamCli {
    /// Spawn `pty attach --attach-stream-fd-v1 3 <args...>` with fd 3 a
    /// pipe. With `read_stream`, a thread drains the pipe into
    /// [`StreamCli::stream`]; otherwise the read end stays in `stream_fd`.
    fn spawn(rig: &Rig, args: &[&str], read_stream: bool) -> StreamCli {
        // Not `libc::pipe2` directly: Apple has no such call.
        let fds = pty_client::tty::cloexec_pipe(false).expect("pipe");
        let (r, w) = (fds[0], fds[1]);
        let mut all: Vec<&str> = vec!["attach", "--attach-stream-fd-v1", "3"];
        all.extend_from_slice(args);
        let mut cmd = rig.command(&all);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        // SAFETY: dup2 in the child only touches descriptors; w is valid.
        unsafe {
            cmd.pre_exec(move || {
                if libc::dup2(w, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn().expect("spawn attach");
        // SAFETY: w is our pipe end; the child has its own copy now.
        unsafe { libc::close(w) };
        // SAFETY: r is our pipe end.
        let read_fd = unsafe { OwnedFd::from_raw_fd(r) };
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (so_tx, stdout_rx) = mpsc::channel();
        let (se_tx, stderr_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut r = stdout;
            let mut buf = Vec::new();
            let _ = r.read_to_end(&mut buf);
            let _ = so_tx.send(buf);
        });
        std::thread::spawn(move || {
            let mut r = stderr;
            let mut buf = Vec::new();
            let _ = r.read_to_end(&mut buf);
            let _ = se_tx.send(buf);
        });
        let stream_fd = Arc::new(Mutex::new(None));
        let stream_rx = if read_stream {
            let (tx, rx) = mpsc::channel();
            let mut f = std::fs::File::from(read_fd);
            std::thread::spawn(move || {
                let mut buf = [0u8; 65536];
                loop {
                    match f.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
            Some(rx)
        } else {
            *stream_fd.lock().unwrap() = Some(read_fd);
            None
        };
        StreamCli {
            child,
            stdin,
            stdout_rx,
            stderr_rx,
            stream_rx,
            stream_fd,
            stream: Vec::new(),
        }
    }

    fn pump(&mut self) {
        if let Some(rx) = &self.stream_rx {
            while let Ok(c) = rx.try_recv() {
                self.stream.extend_from_slice(&c);
            }
        }
    }

    /// Wait until the framed stream contains a packet of `type_`.
    fn wait_for_type(&mut self, type_: MessageType, timeout: Duration) -> bool {
        let start = Instant::now();
        loop {
            self.pump();
            if PacketReader::new()
                .feed(&self.stream)
                .map(|p| p.iter().any(|x| x.type_ == type_))
                .unwrap_or(false)
            {
                return true;
            }
            if start.elapsed() > timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn write_stdin(&mut self, bytes: &[u8]) {
        if let Some(si) = &mut self.stdin {
            let _ = si.write_all(bytes);
            let _ = si.flush();
        }
    }

    /// Wait for exit, collect everything. Status `-1` on a signal death.
    fn finish(mut self, timeout: Duration) -> StreamRun {
        let start = Instant::now();
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(st)) => break st.code().unwrap_or(-1),
                _ => {}
            }
            if start.elapsed() > timeout {
                let _ = self.child.kill();
                let _ = self.child.wait();
                break -999;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        drop(self.stdin.take());
        let stdout = self.stdout_rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
        let stderr = self.stderr_rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
        if let Some(rx) = self.stream_rx.take() {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(c) => self.stream.extend_from_slice(&c),
                    Err(_) => break,
                }
            }
        }
        StreamRun {
            status,
            stdout,
            stderr,
            stream: self.stream,
        }
    }
}

struct StreamRun {
    status: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stream: Vec<u8>,
}

impl StreamRun {
    fn packets(&self) -> Vec<Packet> {
        PacketReader::new().feed(&self.stream).expect("well-framed stream")
    }

    fn types(&self) -> Vec<&'static str> {
        self.packets().iter().map(|p| type_name(p.type_)).collect()
    }

    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    fn summary(&self) -> String {
        format!(
            "status={} stdout={:?} stderr={:?} stream={:?}",
            self.status,
            String::from_utf8_lossy(&self.stdout),
            self.stderr(),
            self.types()
        )
    }
}

/// Peek one byte with a timeout: `Some(true)` when data is waiting,
/// `Some(false)` on EOF, `None` on timeout.
fn has_pending_data(s: &UnixStream, timeout: Duration) -> Option<bool> {
    use std::os::fd::AsRawFd;
    s.set_read_timeout(Some(timeout)).ok();
    let mut b = [0u8; 1];
    // SAFETY: recv with MSG_PEEK into a one-byte buffer.
    let n = unsafe { libc::recv(s.as_raw_fd(), b.as_mut_ptr() as *mut _, 1, libc::MSG_PEEK) };
    if n > 0 {
        Some(true)
    } else if n == 0 {
        Some(false)
    } else {
        None
    }
}

/// A fake daemon at `<root>/fx.sock` (+ metadata). Liveness probes connect
/// and hang up without sending; the first connection that sends bytes (the
/// ATTACH) is handed to `on_attach`, still unread.
fn fake_daemon(rig: &Rig, on_attach: impl FnOnce(UnixStream) + Send + 'static) {
    write_fake_metadata(rig.root(), FAKE, FakeMeta::created(0));
    let listener = UnixListener::bind(rig.socket_path(FAKE)).expect("bind fake socket");
    std::thread::spawn(move || {
        let mut handler = Some(on_attach);
        for conn in listener.incoming() {
            let Ok(conn) = conn else { break };
            match has_pending_data(&conn, Duration::from_secs(3)) {
                Some(true) => {
                    if let Some(h) = handler.take() {
                        conn.set_read_timeout(None).ok();
                        h(conn);
                        break;
                    }
                }
                _ => drop(conn),
            }
        }
    });
}

fn read_first_data(s: &mut UnixStream) -> Vec<u8> {
    let mut buf = [0u8; 4096];
    let n = s.read(&mut buf).unwrap_or(0);
    buf[..n].to_vec()
}

/// node: tests/attach-stream.test.ts:98
#[test]
fn help_documents_the_inherited_fd_contract() {
    let rig = Rig::new();
    let out = rig.pty(&["attach", "--help"]);
    expect_status(&out, 0);
    expect_contains(&out.stdout(), "--attach-stream-fd-v1 <fd>");
    expect_regex(&out.stdout(), "(?s)GEOMETRY.*SCREEN.*DATA.*EXIT");
}

/// node: tests/attach-stream.test.ts:105
#[test]
fn invalid_descriptor_is_rejected_before_resolving_the_session() {
    let rig = Rig::new();
    let out = rig.pty(&["attach", "--attach-stream-fd-v1", "999999", "missing"]);
    expect_failure(&out);
    expect_regex(&out.stderr(), "(?i)attach-stream-fd-v1.*999999.*not writable");
    expect_not_contains(&out.stderr(), "Session \"missing\"");
    assert_eq!(out.stdout(), "");
}

/// node: tests/attach-stream.test.ts:117
#[test]
fn missing_descriptor_value_is_rejected() {
    let rig = Rig::new();
    let out = rig.pty(&["attach", "--attach-stream-fd-v1"]);
    expect_failure(&out);
    expect_regex(&out.stderr(), "attach-stream-fd-v1 requires a file descriptor");
    assert_eq!(out.stdout(), "");
}

/// node: tests/attach-stream.test.ts:126
#[test]
fn frames_events_through_fd_3_from_a_real_daemon() {
    let rig = Rig::new();
    let id = "launcher";
    rig.daemon(id, &["sh", "-c", "printf LAUNCHER_READY; read value"], DaemonOpts::no_display_name());
    let mut cli = StreamCli::spawn(&rig, &[id], true);
    assert!(cli.wait_for_type(MessageType::Screen, Duration::from_secs(10)));
    cli.write_stdin(b"done\n");
    let run = cli.finish(Duration::from_secs(10));
    assert_eq!(run.status, 0, "{}", run.summary());
    assert!(run.stdout.is_empty(), "{}", run.summary());
    assert!(run.stderr.is_empty(), "{}", run.summary());
    let packets = run.packets();
    assert_eq!(packets[0].type_, MessageType::Geometry, "{}", run.summary());
    assert_eq!(packets[1].type_, MessageType::Screen, "{}", run.summary());
    assert!(
        String::from_utf8_lossy(&packets[1].payload).contains("LAUNCHER_READY"),
        "{}",
        run.summary()
    );
    assert_eq!(packets.last().unwrap().type_, MessageType::Exit, "{}", run.summary());
}

/// node: tests/attach-stream.test.ts:183
#[test]
fn frames_a_local_detach_before_closing_fd_3() {
    let rig = Rig::new();
    let id = "launcher-detach";
    rig.daemon(id, &["sh", "-c", "printf DETACH_READY; sleep 300"], DaemonOpts::no_display_name());
    let mut cli = StreamCli::spawn(&rig, &[id], true);
    assert!(cli.wait_for_type(MessageType::Screen, Duration::from_secs(10)));
    cli.write_stdin(&[0x1c]);
    let run = cli.finish(Duration::from_secs(10));
    let _ = rig.pty(&["kill", id]);
    assert_eq!(run.status, 0, "{}", run.summary());
    assert!(run.stdout.is_empty(), "{}", run.summary());
    assert!(run.stderr.is_empty(), "{}", run.summary());
    let packets = run.packets();
    assert_eq!(packets[0].type_, MessageType::Geometry, "{}", run.summary());
    assert_eq!(packets[1].type_, MessageType::Screen, "{}", run.summary());
    assert!(String::from_utf8_lossy(&packets[1].payload).contains("DETACH_READY"));
    assert_eq!(packets.last().unwrap().type_, MessageType::Detach, "{}", run.summary());
    assert!(!packets.iter().any(|p| p.type_ == MessageType::Exit), "{}", run.summary());
}

/// node: tests/attach-stream.test.ts:241
#[test]
fn frames_a_local_detach_before_the_daemon_baseline() {
    let rig = Rig::new();
    let (tx, rx) = mpsc::channel();
    fake_daemon(&rig, move |mut s| {
        let _ = read_first_data(&mut s);
        let _ = tx.send(());
        // Keep the connection open until the client goes away.
        let mut sink = [0u8; 64];
        while matches!(s.read(&mut sink), Ok(n) if n > 0) {}
    });
    let mut cli = StreamCli::spawn(&rig, &[FAKE], true);
    rx.recv_timeout(Duration::from_secs(10)).expect("client attached");
    cli.write_stdin(&[0x1c]);
    let run = cli.finish(Duration::from_secs(5));
    assert_eq!(run.status, 0, "{}", run.summary());
    assert!(run.stdout.is_empty(), "{}", run.summary());
    assert!(run.stderr.is_empty(), "{}", run.summary());
    assert_eq!(run.types(), vec!["DETACH"], "{}", run.summary());
}

/// node: tests/attach-stream.test.ts:271
#[test]
fn exit_inside_the_detach_key_window_wins() {
    let rig = Rig::new();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    fake_daemon(&rig, move |mut s| {
        let _ = read_first_data(&mut s);
        let mut b = encode_geometry(24, 80);
        b.extend(encode_screen(b"exit wins"));
        s.write_all(&b).unwrap();
        let _ = go_rx.recv_timeout(Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(50));
        let _ = s.write_all(&encode_exit(0));
        let _ = s.shutdown(std::net::Shutdown::Both);
    });
    let mut cli = StreamCli::spawn(&rig, &[FAKE], true);
    assert!(cli.wait_for_type(MessageType::Screen, Duration::from_secs(10)));
    cli.write_stdin(&[0x1c]);
    let _ = go_tx.send(());
    let run = cli.finish(Duration::from_secs(5));
    assert_eq!(run.status, 0, "{}", run.summary());
    assert!(run.stdout.is_empty(), "{}", run.summary());
    assert!(run.stderr.is_empty(), "{}", run.summary());
    assert_eq!(run.types(), vec!["GEOMETRY", "SCREEN", "EXIT"], "{}", run.summary());
}

/// node: tests/attach-stream.test.ts:327
#[test]
fn reframes_fragmented_and_coalesced_packets_in_order() {
    let rig = Rig::new();
    fake_daemon(&rig, move |mut s| {
        let _ = read_first_data(&mut s);
        let geometry = encode_geometry(31, 97);
        s.write_all(&geometry[..2]).unwrap();
        s.flush().unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let mut rest = geometry[2..].to_vec();
        rest.extend(encode_screen(b"\x1b[31mred\x1b[0m"));
        rest.extend(encode_packet(MessageType::Data, b"live"));
        rest.extend(encode_exit(0));
        s.write_all(&rest).unwrap();
        let _ = s.shutdown(std::net::Shutdown::Both);
    });
    let cli = StreamCli::spawn(&rig, &[FAKE], true);
    let run = cli.finish(Duration::from_secs(10));
    assert_eq!(run.status, 0, "{}", run.summary());
    assert!(run.stdout.is_empty(), "{}", run.summary());
    assert!(run.stderr.is_empty(), "{}", run.summary());
    let packets = run.packets();
    assert_eq!(run.types(), vec!["GEOMETRY", "SCREEN", "DATA", "EXIT"]);
    assert_eq!(pty_core::protocol::decode_size(&packets[0].payload), (31, 97));
    assert_eq!(packets[1].payload, b"\x1b[31mred\x1b[0m");
    assert_eq!(packets[2].payload, b"live");
}

/// node: tests/attach-stream.test.ts:353
#[test]
fn stdout_stays_the_controlling_tty_for_the_attach_geometry() {
    let rig = Rig::new();
    let (size_tx, size_rx) = mpsc::channel();
    fake_daemon(&rig, move |mut s| {
        let mut reader = PacketReader::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = match s.read(&mut buf) {
                Ok(n) if n > 0 => n,
                _ => break,
            };
            let mut attached = None;
            for p in reader.feed(&buf[..n]).unwrap_or_default() {
                if p.type_ == MessageType::Attach {
                    attached = Some(pty_core::protocol::decode_size(&p.payload));
                }
            }
            if let Some((rows, cols)) = attached {
                let _ = size_tx.send((rows, cols));
                let mut b = encode_geometry(rows, cols);
                b.extend(encode_screen(b"screen stays off stdout"));
                b.extend(encode_exit(0));
                let _ = s.write_all(&b);
                let _ = s.shutdown(std::net::Shutdown::Both);
                break;
            }
        }
    });
    let stream_path = rig.tmp().join("controlling-tty.stream");
    let script = format!(
        "exec '{}' attach --attach-stream-fd-v1 3 {FAKE} 3>'{}'",
        pty_bin().display(),
        stream_path.display()
    );
    let mut t = TtyProc::spawn(
        std::path::Path::new("/bin/sh"),
        &["-c", &script],
        &rig.base_env(),
        rig.tmp(),
        27,
        91,
    );
    let code = t.wait_exit(Duration::from_secs(10));
    let output = t.output();
    assert_eq!(code, Some(0), "{:?}", String::from_utf8_lossy(&output));
    assert!(output.is_empty(), "painted on the tty: {:?}", String::from_utf8_lossy(&output));
    assert_eq!(size_rx.recv_timeout(Duration::from_secs(5)).unwrap(), (27, 91));
    let packets = PacketReader::new().feed(&std::fs::read(&stream_path).unwrap()).unwrap();
    assert_eq!(sequence_names(&packets.iter().map(|p| p.type_).collect::<Vec<_>>()), vec!["GEOMETRY", "SCREEN", "EXIT"]);
}

/// node: tests/attach-stream.test.ts:404
#[test]
fn legacy_daemon_sending_screen_first_fails_clearly() {
    let rig = Rig::new();
    fake_daemon(&rig, move |mut s| {
        let _ = read_first_data(&mut s);
        s.write_all(&encode_screen(b"legacy screen")).unwrap();
        let mut sink = [0u8; 64];
        while matches!(s.read(&mut sink), Ok(n) if n > 0) {}
    });
    let cli = StreamCli::spawn(&rig, &[FAKE], true);
    let run = cli.finish(Duration::from_secs(10));
    assert_ne!(run.status, 0, "{}", run.summary());
    assert!(run.stdout.is_empty(), "{}", run.summary());
    assert!(run.stream.is_empty(), "{}", run.summary());
    expect_regex(&run.stderr(), "(?i)daemon does not support attach stream v1");
}

fn premature(kind: &str, packet: Vec<u8>) {
    let rig = Rig::new();
    fake_daemon(&rig, move |mut s| {
        let _ = read_first_data(&mut s);
        let mut b = encode_geometry(24, 80);
        b.extend(packet);
        s.write_all(&b).unwrap();
        let mut sink = [0u8; 64];
        while matches!(s.read(&mut sink), Ok(n) if n > 0) {}
    });
    let cli = StreamCli::spawn(&rig, &[FAKE], true);
    let run = cli.finish(Duration::from_secs(10));
    assert_eq!(run.status, 1, "{}", run.summary());
    assert!(run.stdout.is_empty(), "{}", run.summary());
    expect_regex(&run.stderr(), &format!("(?i)expected SCREEN before {kind}"));
    assert_eq!(run.types(), vec!["GEOMETRY"], "{}", run.summary());
}

/// node: tests/attach-stream.test.ts:420
#[test]
fn data_before_the_initial_screen_is_rejected() {
    premature("DATA", encode_packet(MessageType::Data, b"too early"));
}

/// node: tests/attach-stream.test.ts:420
#[test]
fn exit_before_the_initial_screen_is_rejected() {
    premature("EXIT", encode_exit(0));
}

/// node: tests/attach-stream.test.ts:434
#[test]
fn close_without_exit_is_a_truncated_stream() {
    let rig = Rig::new();
    fake_daemon(&rig, move |mut s| {
        let _ = read_first_data(&mut s);
        let mut b = encode_geometry(24, 80);
        b.extend(encode_screen(b"truncated"));
        s.write_all(&b).unwrap();
        let _ = s.shutdown(std::net::Shutdown::Both);
    });
    let cli = StreamCli::spawn(&rig, &[FAKE], true);
    let run = cli.finish(Duration::from_secs(10));
    assert_eq!(run.status, 1, "{}", run.summary());
    assert!(run.stdout.is_empty(), "{}", run.summary());
    expect_regex(&run.stderr(), "(?i)machine stream truncated before EXIT: connection closed");
    assert_eq!(run.types(), vec!["GEOMETRY", "SCREEN"], "{}", run.summary());
}

/// node: tests/attach-stream.test.ts:448
#[test]
fn transport_reset_is_a_truncated_stream_not_a_missing_session() {
    let rig = Rig::new();
    fake_daemon(&rig, move |mut s| {
        // Consume one byte only: the unread remainder of the ATTACH makes
        // the close a reset (ECONNRESET) rather than a clean FIN.
        let mut one = [0u8; 1];
        let _ = s.read(&mut one);
        let mut b = encode_geometry(24, 80);
        b.extend(encode_screen(b"partial"));
        s.write_all(&b).unwrap();
        drop(s);
    });
    let cli = StreamCli::spawn(&rig, &[FAKE], true);
    let run = cli.finish(Duration::from_secs(10));
    assert_eq!(run.status, 1, "{}", run.summary());
    assert!(run.stdout.is_empty(), "{}", run.summary());
    expect_regex(&run.stderr(), "(?i)machine stream truncated before EXIT");
    expect_not_regex(&run.stderr(), "(?i)session .* not found");
}

/// node: tests/attach-stream.test.ts:461
#[test]
fn broken_inherited_stream_fails_instead_of_hanging() {
    let rig = Rig::new();
    let (tx, rx) = mpsc::channel::<()>();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    fake_daemon(&rig, move |mut s| {
        let _ = read_first_data(&mut s);
        let _ = tx.send(());
        let _ = go_rx.recv_timeout(Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
        let _ = s.write_all(&encode_geometry(24, 80));
        let _ = s.write_all(&encode_screen(b"baseline"));
        let _ = s.write_all(&encode_packet(MessageType::Data, &vec![65u8; 1024 * 1024]));
        let mut sink = [0u8; 64];
        while matches!(s.read(&mut sink), Ok(n) if n > 0) {}
    });
    let cli = StreamCli::spawn(&rig, &[FAKE], false);
    rx.recv_timeout(Duration::from_secs(10)).expect("client attached");
    // Destroy the reader: close our end of the pipe.
    drop(cli.stream_fd.lock().unwrap().take());
    let _ = go_tx.send(());
    let run = cli.finish(Duration::from_secs(5));
    assert_eq!(run.status, 1, "{}", run.summary());
    assert!(run.stdout.is_empty(), "{}", run.summary());
    expect_regex(&run.stderr(), "(?i)machine stream descriptor 3 failed.*EPIPE");
}
