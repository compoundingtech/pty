//! `pty peek`: one-shot output, follow mode, and `--wait` diagnostics against
//! a fake daemon (`client.ts:76-194`, `cli.ts:1941-1990`).

mod common;

use std::os::fd::AsRawFd;
use std::thread::JoinHandle;
use std::time::Duration;

use common::*;
use pty_client::{
    CURSOR_TO_BOTTOM, ClientIo, PeekOutcome, PeekParams, PeekWaitError, TERMINAL_SANITIZE, follow,
    peek, peek_wait, strip_ansi,
};
use pty_core::protocol::{MessageType, decode_peek, encode_data, encode_exit, encode_screen};
use pty_core::registry;

const T: Duration = Duration::from_secs(5);

/// Run `op` with piped stdin/stdout/stderr; returns (result, stdout, stderr).
fn with_io<R: Send + 'static>(
    stdin_script: Option<Vec<u8>>,
    op: impl FnOnce(&ClientIo) -> R + Send + 'static,
) -> (R, Vec<u8>, Vec<u8>) {
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
        op(&io)
    });
    let out = collect(stdout.r);
    let err = collect(stderr.r);
    if let Some(bytes) = stdin_script {
        std::thread::sleep(Duration::from_millis(100));
        pty_client::tty::write_all_fd(stdin.w.as_raw_fd(), &bytes).unwrap();
    }
    let r = join_within(handle, T, "peek");
    drop(stdin.w);
    (r, out.finish(), err.finish())
}

fn daemon(
    f: impl FnOnce(std::os::unix::net::UnixStream, (bool, bool)) + Send + 'static,
) -> (FakeDaemon, JoinHandle<()>) {
    let d = FakeDaemon::bind("peek");
    let listener = d.listener.try_clone().unwrap();
    let h = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let first = packets(&read_chunk(&mut s));
        assert_eq!(first[0].type_, MessageType::Peek);
        f(s, decode_peek(&first[0].payload));
    });
    (d, h)
}

/// node: client.ts:127-137; tests/integration.test.ts:1812-1862
#[test]
fn one_shot_plain_prints_the_screen_and_one_newline() {
    let (d, h) = daemon(|mut s, flags| {
        use std::io::Write;
        assert_eq!(flags, (true, false));
        s.write_all(&encode_screen(b"READY> ")).unwrap();
        let _ = read_packets_until_eof(&mut s, T);
    });
    let name = d.name.clone();
    let (r, out, err) = with_io(None, move |io| {
        let mut p = PeekParams::new(&name);
        p.plain = true;
        peek(p, io)
    });
    assert_eq!(r.unwrap(), PeekOutcome::Printed);
    assert_eq!(out, b"READY> \n");
    assert!(err.is_empty());
    h.join().unwrap();
}

/// node: client.ts:131-134 — the ANSI peek sanitizes and homes the cursor
/// before the newline; `--full` sets bit 1.
#[test]
fn one_shot_ansi_appends_sanitize_and_cursor() {
    let (d, h) = daemon(|mut s, flags| {
        use std::io::Write;
        assert_eq!(flags, (false, true));
        s.write_all(&encode_screen(b"\x1b[31mred\x1b[0m")).unwrap();
        let _ = read_packets_until_eof(&mut s, T);
    });
    let name = d.name.clone();
    let (r, out, _) = with_io(None, move |io| {
        let mut p = PeekParams::new(&name);
        p.full = true;
        peek(p, io)
    });
    assert_eq!(r.unwrap(), PeekOutcome::Printed);
    assert_eq!(
        out,
        format!("\x1b[31mred\x1b[0m{TERMINAL_SANITIZE}{CURSOR_TO_BOTTOM}\n").as_bytes()
    );
    h.join().unwrap();
}

/// node: client.ts:179-193 — a close before any screen is "not found".
#[test]
fn one_shot_close_before_screen_is_not_found() {
    let (d, h) = daemon(|s, _| drop(s));
    let name = d.name.clone();
    let (r, out, _) = with_io(None, move |io| peek(PeekParams::new(&name), io));
    assert_eq!(
        r.unwrap_err().to_string(),
        format!("Session \"{}\" not found or not running.", d.name)
    );
    assert!(out.is_empty());
    h.join().unwrap();
}

/// node: client.ts:139-157 — follow streams DATA (ANSI-stripped when plain)
/// and prints the exit line.
#[test]
fn follow_streams_data_and_prints_the_exit_line() {
    let (d, h) = daemon(|mut s, flags| {
        use std::io::Write;
        assert_eq!(flags, (true, false));
        s.write_all(&concat(&[
            encode_screen(b"start"),
            encode_data(b"\x1b[1mbold\x1b[0m plain"),
            encode_exit(4),
        ]))
        .unwrap();
    });
    let name = d.name.clone();
    let (r, out, _) = with_io(None, move |io| {
        let mut p = PeekParams::new(&name);
        p.plain = true;
        follow(p, io)
    });
    assert_eq!(r.unwrap(), PeekOutcome::Exited(4));
    assert_eq!(
        String::from_utf8(out).unwrap(),
        format!("startbold plain\r\n[{} exited with code 4]\r\n", d.name)
    );
    h.join().unwrap();
}

#[test]
fn follow_reports_transport_loss_without_claiming_a_child_exit() {
    // Encode tag 12 generically so the same regression compiles against the
    // previous client, which preserves this as an unknown extension.
    let rejection = pty_core::protocol::encode_packet(MessageType::Unknown(12), &[0]);
    for (suffix, has_reason) in [
        (Vec::new(), false),
        (rejection[..3].to_vec(), false),
        (rejection, true),
    ] {
        for plain in [false, true] {
            let suffix = suffix.clone();
            let (d, h) = daemon(move |mut socket, _| {
                use std::io::Write;
                socket.write_all(&encode_screen(b"live")).unwrap();
                socket.write_all(&suffix).unwrap();
            });
            let name = d.name.clone();
            let (result, out, err) = with_io(None, move |io| {
                let mut params = PeekParams::new(&name);
                params.plain = plain;
                follow(params, io)
            });
            assert_eq!(result.unwrap(), PeekOutcome::Exited(1));
            let out = String::from_utf8_lossy(&out);
            assert!(out.contains("connection lost"), "{out:?}");
            assert!(!out.contains("session ended"), "{out:?}");
            assert_eq!(String::from_utf8_lossy(&err).contains("client too slow"), has_reason);
            h.join().unwrap();
        }
    }
}

/// node: client.ts:90-103 — Ctrl+\ (one tap) detaches follow mode.
#[test]
fn follow_detaches_on_ctrl_backslash() {
    let (d, h) = daemon(|mut s, _| {
        use std::io::Write;
        s.write_all(&encode_screen(b"live")).unwrap();
        let _ = read_packets_until_eof(&mut s, T);
    });
    let name = d.name.clone();
    let d_name = d.name.clone();
    let (r, out, _) = with_io(Some(b"ignored\x1c".to_vec()), move |io| {
        follow(PeekParams::new(&name), io)
    });
    assert_eq!(r.unwrap(), PeekOutcome::Detached);
    assert_eq!(
        String::from_utf8(out).unwrap(),
        format!(
            "live{TERMINAL_SANITIZE}{CURSOR_TO_BOTTOM}\r\n[detached from {0}]\r\n  reattach: pty peek -f {0}\r\n",
            d_name
        )
    );
    h.join().unwrap();
}

/// node: src/tui/colors.ts:29-30
#[test]
fn strip_ansi_removes_csi_sequences_only() {
    assert_eq!(
        strip_ansi("\x1b[1mbold\x1b[0m \x1b[38;5;200mx\x1b[K"),
        "bold x"
    );
    assert_eq!(strip_ansi("plain\x1b]0;title\x07"), "plain\x1b]0;title\x07");
    assert_eq!(strip_ansi("héllo\x1b[2J"), "héllo");
}

/// Serve `screens` (plain) to successive PEEKs; ANSI peeks get `ansi`.
fn wait_daemon(screens: Vec<&'static [u8]>, ansi: &'static [u8]) -> (FakeDaemon, JoinHandle<()>) {
    let d = FakeDaemon::bind("wait");
    let listener = d.listener.try_clone().unwrap();
    let h = std::thread::spawn(move || {
        use std::io::Write;
        let mut plain_iter = screens.into_iter();
        let mut last: &[u8] = b"";
        loop {
            let Ok((mut s, _)) = listener.accept() else {
                break;
            };
            let first = packets(&read_chunk(&mut s));
            let (plain, _) = decode_peek(&first[0].payload);
            let body: &[u8] = if plain {
                if let Some(next) = plain_iter.next() {
                    last = next;
                }
                last
            } else {
                ansi
            };
            let _ = s.write_all(&encode_screen(body));
            if body == b"STOP" {
                break;
            }
        }
    });
    (d, h)
}

/// node: tests/peek-wait.test.ts:111-160 — polls until any pattern matches;
/// an ANSI result is fetched fresh.
#[test]
fn peek_wait_matches_any_pattern() {
    let (d, _h) = wait_daemon(
        vec![b"nothing yet", b"still no", b"now SECOND here"],
        b"\x1b[1mANSI\x1b[0m",
    );
    let got = peek_wait(&d.name, &["FIRST".into(), "SECOND".into()], 5.0, true).unwrap();
    assert_eq!(got, "now SECOND here");
    let got = peek_wait(&d.name, &["SECOND".into()], 5.0, false).unwrap();
    assert_eq!(got, "\x1b[1mANSI\x1b[0m");
}

/// node: tests/peek-wait.test.ts:123-133
#[test]
fn peek_wait_times_out_with_the_exact_text() {
    let (d, _h) = wait_daemon(vec![b"nope"], b"");
    let err = peek_wait(&d.name, &["NEVER".into()], 0.5, true).unwrap_err();
    assert_eq!(
        err.to_string(),
        "Timed out after 0.5s waiting for \"NEVER\"."
    );
    assert_eq!(
        PeekWaitError::TimedOut {
            timeout_secs: 5.0,
            patterns: "\"a\" or \"b\"".into()
        }
        .to_string(),
        "Timed out after 5s waiting for \"a\" or \"b\"."
    );
}

fn write_exited_metadata(name: &str, last_lines: &[&str], exit_code: Option<i32>) {
    let json = serde_json::json!({
        "command": "sh",
        "args": [],
        "displayCommand": "sh",
        "cwd": "/",
        "createdAt": "2026-07-31T00:00:00.000Z",
        "exitCode": exit_code,
        "exitedAt": "2026-07-31T00:00:01.000Z",
        "lastLines": last_lines,
    });
    std::fs::write(registry::metadata_path(name), json.to_string()).unwrap();
}

/// node: tests/peek-wait.test.ts:162-188; cli.ts:1966-1985 — with no live
/// socket, `lastLines` is matched, else the exited diagnostic lists it.
#[test]
fn peek_wait_falls_back_to_last_lines_for_an_exited_session() {
    test_root();
    let name = unique_name("exited");
    write_exited_metadata(&name, &["line one", "TEST_PASSED here"], Some(0));
    let got = peek_wait(&name, &["TEST_PASSED".into()], 5.0, true).unwrap();
    assert_eq!(got, "line one\nTEST_PASSED here");

    let err = peek_wait(&name, &["MISSING".into()], 5.0, true).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "Session \"{name}\" exited (code 0) without matching \"MISSING\".\nLast output:\n  line one\n  TEST_PASSED here"
        )
    );

    let name2 = unique_name("exited");
    write_exited_metadata(&name2, &[], None);
    let err = peek_wait(&name2, &["X".into(), "Y".into()], 5.0, true).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("Session \"{name2}\" exited (code ?) without matching \"X\" or \"Y\".")
    );
}
