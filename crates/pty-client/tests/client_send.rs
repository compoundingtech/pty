//! `pty send` framing as seen by the daemon: bracketed-paste markers are their
//! own DATA packets around the whole payload; no ATTACH; no implicit newline.
//! Port of `tests/send-paste.test.ts:121-219` and `tests/connection.test.ts:257-314`.

mod common;

use std::time::Duration;

use common::*;
use pty_client::{
    DEFAULT_SEQ_DELAY_MS, SendDataOptions, SendOptions, resolve_seq_delay_ms, send, send_data,
};
use pty_core::protocol::MessageType;

const T: Duration = Duration::from_secs(5);

/// Run one `send` and return the DATA payloads the daemon received, in order.
fn capture(items: &[&str], opts: SendOptions) -> Vec<Vec<u8>> {
    let d = FakeDaemon::bind("send");
    let listener = d.listener.try_clone().unwrap();
    let h = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        read_packets_until_eof(&mut s, T)
    });
    send(&d.name, items, opts).unwrap();
    let packets = h.join().unwrap();
    assert!(
        packets.iter().all(|p| p.type_ == MessageType::Data),
        "{packets:?}"
    );
    packets.into_iter().map(|p| p.payload).collect()
}

fn joined(parts: &[Vec<u8>]) -> String {
    String::from_utf8(parts.concat()).unwrap()
}

/// node: tests/send-paste.test.ts:121-145
#[test]
fn paste_wraps_a_single_positional_in_markers() {
    let got = capture(
        &["hello-paste"],
        SendOptions {
            delay_ms: 0,
            paste: true,
        },
    );
    assert_eq!(
        got,
        vec![
            b"\x1b[200~".to_vec(),
            b"hello-paste".to_vec(),
            b"\x1b[201~".to_vec()
        ]
    );
    assert_eq!(joined(&got), "\x1b[200~hello-paste\x1b[201~");
}

/// node: tests/send-paste.test.ts:147-186
#[test]
fn paste_wraps_the_whole_seq_payload_in_one_pair() {
    let got = capture(
        &["first ", "second ", "third"],
        SendOptions {
            delay_ms: 0,
            paste: true,
        },
    );
    assert_eq!(joined(&got), "\x1b[200~first second third\x1b[201~");
    assert_eq!(got.len(), 5);
    let got = capture(
        &["A", "B"],
        SendOptions {
            delay_ms: 50,
            paste: true,
        },
    );
    assert_eq!(joined(&got), "\x1b[200~AB\x1b[201~");
}

/// node: tests/send-paste.test.ts:188-219
#[test]
fn no_markers_without_paste_and_literal_newlines_survive() {
    let got = capture(&["plain-text"], SendOptions::default());
    assert_eq!(got, vec![b"plain-text".to_vec()]);
    let got = capture(
        &["line-one\nline-two\n"],
        SendOptions {
            delay_ms: 0,
            paste: true,
        },
    );
    assert_eq!(joined(&got), "\x1b[200~line-one\nline-two\n\x1b[201~");
}

/// node: tests/send-paste.test.ts:267-290 — each `--seq` item is its own DATA
/// packet, streamed with no gap at delay 0.
#[test]
fn seq_items_are_separate_packets() {
    let got = capture(
        &["\x15", "\x15", "\x15", "\x15"],
        SendOptions {
            delay_ms: 0,
            paste: false,
        },
    );
    assert_eq!(got.len(), 4);
    assert_eq!(joined(&got), "\x15\x15\x15\x15");
}

/// node: tests/connection.test.ts:257-314 — `sendData paste:true` writes exactly
/// one bracket pair, and `data:[]` writes nothing.
#[test]
fn send_data_paste_pair_and_empty_payload() {
    let d = FakeDaemon::bind("sd");
    let listener = d.listener.try_clone().unwrap();
    let h = std::thread::spawn(move || {
        let mut out = Vec::new();
        for _ in 0..2 {
            let (mut s, _) = listener.accept().unwrap();
            out.push(read_packets_until_eof(&mut s, T));
        }
        out
    });
    send_data(
        &d.name,
        &["x", "y"],
        SendDataOptions {
            delay_ms: 0,
            paste: true,
        },
    )
    .unwrap();
    send_data::<&str>(
        &d.name,
        &[],
        SendDataOptions {
            delay_ms: 0,
            paste: true,
        },
    )
    .unwrap();
    let got = h.join().unwrap();
    let first: Vec<Vec<u8>> = got[0].iter().map(|p| p.payload.clone()).collect();
    assert_eq!(joined(&first), "\x1b[200~xy\x1b[201~");
    assert_eq!(
        first
            .iter()
            .filter(|p| p.as_slice() == b"\x1b[200~")
            .count(),
        1
    );
    assert_eq!(
        first
            .iter()
            .filter(|p| p.as_slice() == b"\x1b[201~")
            .count(),
        1
    );
    assert!(got[1].is_empty());
}

/// node: tests/seq-delay.test.ts:15-28, :83-106 — the pacing default and the
/// gap BETWEEN items.
#[test]
fn delay_resolution_and_pacing() {
    assert_eq!(DEFAULT_SEQ_DELAY_MS, 300);
    assert_eq!(resolve_seq_delay_ms(None), 300);
    assert_eq!(resolve_seq_delay_ms(Some(0.0)), 0);
    assert_eq!(resolve_seq_delay_ms(Some(0.1)), 100);
    assert_eq!(resolve_seq_delay_ms(Some(0.4285)), 429);
    let start = std::time::Instant::now();
    let got = capture(
        &["a", "b", "c"],
        SendOptions {
            delay_ms: 100,
            paste: false,
        },
    );
    let elapsed = start.elapsed();
    assert_eq!(got.len(), 3);
    assert!(elapsed >= Duration::from_millis(200), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
}

/// node: client.ts:262-271 — a missing session is the not-found text.
#[test]
fn missing_session_is_not_found() {
    test_root();
    let err = send("nope", &["x"], SendOptions::default()).unwrap_err();
    assert_eq!(
        err.to_string(),
        "Session \"nope\" not found or not running."
    );
}
