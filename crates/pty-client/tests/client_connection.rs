//! `SessionConnection` / `peek_screen` against a fake daemon (port of the
//! parts of `tests/connection.test.ts` that pin the wire behavior), plus the
//! tokio flavour when the `tokio` feature is on.

mod common;

use std::time::Duration;

use common::*;
use pty_client::{
    ClientError, PeekScreenOptions, SessionConnection, SessionEvent, peek_screen,
};
use pty_core::protocol::{
    MessageType, PacketReader, decode_size, encode_data, encode_exit, encode_geometry,
    encode_screen,
};

const T: Duration = Duration::from_secs(5);

/// A DATA frame ahead of the first SCREEN used to spin `connect` at full
/// CPU forever. The event was queued on the connection, and the next read
/// drained that queue before it touched the socket, so the loop handed
/// itself the same event and queued it again. With no timeout it never
/// ended. The events must be kept, in order, and handed over after the
/// screen arrives.
#[test]
fn output_before_the_first_screen_is_kept_without_spinning() {
    let d = FakeDaemon::bind("early");
    let listener = d.listener.try_clone().unwrap();
    let h = std::thread::spawn(move || {
        use std::io::Write;
        let (mut s, _) = listener.accept().unwrap();
        let mut reader = PacketReader::new();
        read_until(&mut s, &mut reader, MessageType::Attach, T);
        s.write_all(&concat(&[encode_data(b"first"), encode_data(b"second")]))
            .unwrap();
        std::thread::sleep(Duration::from_millis(150));
        s.write_all(&encode_screen(b"$ ")).unwrap();
        let _ = read_until(&mut s, &mut reader, MessageType::Detach, T);
    });

    // `connect` has no deadline of its own, so a spin would hang the test
    // run rather than fail it. Run it on a thread and give the answer a
    // budget here.
    let name = d.name.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(SessionConnection::connect(&name, 24, 80).map(|mut c| {
            let screen = c.screen().to_vec();
            let a = c.next_event(Some(T)).unwrap();
            let b = c.next_event(Some(T)).unwrap();
            (screen, a, b)
        }));
    });
    let (screen, a, b) = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("connect must not spin on output that arrives before the screen")
        .expect("connect");
    assert_eq!(screen, b"$ ");
    assert_eq!(a, Some(SessionEvent::Data(b"first".to_vec())));
    assert_eq!(b, Some(SessionEvent::Data(b"second".to_vec())));
    h.join().unwrap();
}

/// The same frames with a deadline: the deadline is what ends it, and it
/// ends near the deadline rather than after a long spin.
#[test]
fn output_before_a_screen_that_never_comes_ends_at_the_deadline() {
    let d = FakeDaemon::bind("early-to");
    let listener = d.listener.try_clone().unwrap();
    let h = std::thread::spawn(move || {
        use std::io::Write;
        let (mut s, _) = listener.accept().unwrap();
        let mut reader = PacketReader::new();
        read_until(&mut s, &mut reader, MessageType::Attach, T);
        s.write_all(&encode_data(b"only data")).unwrap();
        std::thread::sleep(Duration::from_millis(600));
    });
    let started = std::time::Instant::now();
    let err = SessionConnection::connect_with_timeout(
        &d.name,
        24,
        80,
        Some(Duration::from_millis(200)),
    )
    .expect_err("no screen ever arrives");
    assert!(matches!(err, ClientError::ClosedBeforeScreen(_)), "{err:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "gave up after {:?}",
        started.elapsed()
    );
    h.join().unwrap();
}

/// node: tests/connection.test.ts:102-202
#[test]
fn connect_attaches_resolves_on_screen_and_tracks_geometry() {
    let d = FakeDaemon::bind("conn");
    let listener = d.listener.try_clone().unwrap();
    let h = std::thread::spawn(move || {
        use std::io::Write;
        let (mut s, _) = listener.accept().unwrap();
        let mut reader = PacketReader::new();
        let first = read_until(&mut s, &mut reader, MessageType::Attach, T);
        assert_eq!(decode_size(&first[0].payload), (30, 100));
        s.write_all(&concat(&[
            encode_geometry(24, 80),
            encode_screen(b"$ "),
            encode_data(b"hello"),
        ]))
        .unwrap();
        let got = read_until(&mut s, &mut reader, MessageType::Resize, T);
        assert_eq!(got[0].type_, MessageType::Data);
        assert_eq!(got[0].payload, b"typed");
        assert_eq!(got[1].payload, b"\r");
        assert_eq!(decode_size(&got[2].payload), (40, 120));
        s.write_all(&encode_exit(9)).unwrap();
        let rest = read_until(&mut s, &mut reader, MessageType::Detach, T);
        assert_eq!(rest.last().unwrap().type_, MessageType::Detach);
    });
    let mut c = SessionConnection::connect(&d.name, 30, 100).unwrap();
    assert!(c.connected());
    assert_eq!(c.screen(), b"$ ");
    assert_eq!((c.effective_rows(), c.effective_cols()), (24, 80));
    assert_eq!((c.rows(), c.cols()), (30, 100));
    assert_eq!(
        c.next_event(Some(T)).unwrap(),
        Some(SessionEvent::Data(b"hello".to_vec()))
    );
    c.write(b"typed");
    c.press("return").unwrap();
    c.resize(40, 120);
    assert_eq!((c.rows(), c.cols()), (40, 120));
    assert_eq!(c.next_event(Some(T)).unwrap(), Some(SessionEvent::Exit(9)));
    assert_eq!(c.next_event(Some(Duration::from_millis(50))).unwrap(), None);
    c.disconnect();
    assert!(!c.connected());
    assert_eq!(c.next_event(Some(T)).unwrap(), Some(SessionEvent::Closed));
    h.join().unwrap();
}

/// node: connection.ts:157-165
#[test]
fn close_before_screen_and_missing_session() {
    let d = FakeDaemon::bind("conn-close");
    let listener = d.listener.try_clone().unwrap();
    let h = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let _ = read_chunk(&mut s);
        drop(s);
    });
    let err = SessionConnection::connect(&d.name, 24, 80).unwrap_err();
    assert_eq!(err, ClientError::ClosedBeforeScreen(d.name.clone()));
    assert_eq!(
        err.to_string(),
        format!(
            "Connection to \"{}\" closed before screen received.",
            d.name
        )
    );
    h.join().unwrap();
    test_root();
    let err = SessionConnection::connect("absent", 24, 80).unwrap_err();
    assert_eq!(
        err.to_string(),
        "Session \"absent\" not found or not running."
    );
}

/// node: tests/connection.test.ts:318-349
#[test]
fn peek_screen_returns_the_first_screen() {
    let d = FakeDaemon::bind("ps");
    let listener = d.listener.try_clone().unwrap();
    let h = std::thread::spawn(move || {
        use std::io::Write;
        let (mut s, _) = listener.accept().unwrap();
        let first = packets(&read_chunk(&mut s));
        assert_eq!(first[0].type_, MessageType::Peek);
        assert_eq!(first[0].payload, vec![1]);
        s.write_all(&encode_screen(b"plain text")).unwrap();
        let _ = read_packets_until_eof(&mut s, T);
    });
    let got = peek_screen(
        &d.name,
        PeekScreenOptions {
            plain: true,
            full: false,
        },
    )
    .unwrap();
    assert_eq!(got, "plain text");
    assert!(!got.contains("\x1b["));
    h.join().unwrap();
}

#[cfg(feature = "tokio")]
mod tokio_flavour {
    use super::*;
    use pty_client::AsyncConnection;

    /// The async connection speaks the same protocol as the sync one.
    #[tokio::test]
    async fn async_connection_round_trip() {
        let d = FakeDaemon::bind("aconn");
        let listener = d.listener.try_clone().unwrap();
        let h = std::thread::spawn(move || {
            use std::io::Write;
            let (mut s, _) = listener.accept().unwrap();
            let mut reader = PacketReader::new();
            let first = read_until(&mut s, &mut reader, MessageType::Attach, T);
            assert_eq!(decode_size(&first[0].payload), (10, 20));
            s.write_all(&concat(&[encode_geometry(10, 20), encode_screen(b"async")]))
                .unwrap();
            let got = read_until(&mut s, &mut reader, MessageType::Data, T);
            assert_eq!(got.last().unwrap().payload, b"\x03");
            s.write_all(&encode_exit(1)).unwrap();
            let _ = read_until(&mut s, &mut reader, MessageType::Detach, T);
        });
        let name = d.name.clone();
        let mut c = AsyncConnection::connect(&name, 10, 20).await.unwrap();
        assert_eq!(c.screen(), b"async");
        assert_eq!((c.effective_rows(), c.effective_cols()), (10, 20));
        c.press("ctrl+c").await.unwrap();
        assert_eq!(c.next_event().await.unwrap(), SessionEvent::Exit(1));
        c.disconnect().await;
        assert_eq!(c.next_event().await.unwrap(), SessionEvent::Closed);
        tokio::task::spawn_blocking(move || h.join().unwrap())
            .await
            .unwrap();
    }

    /// The async attach had the same early-frame spin as the synchronous
    /// one, and no timeout of its own to end it. This path is reachable
    /// from any peer that sends output before the first SCREEN.
    ///
    /// The budget is an ordinary channel on an ordinary thread, and that is
    /// deliberate. A spin that never awaits anything pending never returns
    /// to the scheduler, so NOTHING inside the runtime can interrupt it:
    /// `tokio::time::timeout` around it is never polled, and moving the
    /// work to `tokio::spawn` on a two-worker runtime does not help either.
    /// Both were tried against the defect and both hung the test run
    /// instead of reporting. Only a real thread outside the runtime can
    /// call time on it.
    #[test]
    fn output_before_the_first_screen_is_kept_without_spinning() {
        let d = FakeDaemon::bind("aearly");
        let listener = d.listener.try_clone().unwrap();
        let h = std::thread::spawn(move || {
            use std::io::Write;
            let (mut s, _) = listener.accept().unwrap();
            let mut reader = PacketReader::new();
            read_until(&mut s, &mut reader, MessageType::Attach, T);
            s.write_all(&concat(&[encode_data(b"first"), encode_data(b"second")]))
                .unwrap();
            std::thread::sleep(Duration::from_millis(150));
            s.write_all(&encode_screen(b"$ ")).unwrap();
            let _ = read_until(&mut s, &mut reader, MessageType::Detach, T);
        });

        let name = d.name.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let out = rt.block_on(async move {
                let mut c = AsyncConnection::connect(&name, 24, 80).await?;
                let screen = c.screen().to_vec();
                let a = c.next_event().await?;
                let b = c.next_event().await?;
                c.disconnect().await;
                Ok::<_, ClientError>((screen, a, b))
            });
            let _ = tx.send(out);
        });

        let (screen, a, b) = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the async connect must not spin on output that arrives before the screen")
            .expect("connect");
        assert_eq!(screen, b"$ ");
        assert_eq!(a, SessionEvent::Data(b"first".to_vec()));
        assert_eq!(b, SessionEvent::Data(b"second".to_vec()));
        h.join().unwrap();
    }
}
