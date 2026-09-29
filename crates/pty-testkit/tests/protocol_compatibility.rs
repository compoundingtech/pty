use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::time::Duration;

use pty_core::protocol::{MessageType, encode_packet};

/// A newer daemon may answer with a frame this client does not know. The
/// operator needs to know which side to upgrade before touching live sessions.
#[test]
fn a_newer_daemon_response_identifies_the_outdated_client() {
    let root = std::env::temp_dir().join(format!(
        "pi-{}",
        pty_testkit::server::random_id()
    ));
    std::fs::create_dir_all(&root).expect("create registry");
    let listener = UnixListener::bind(root.join("newer.sock")).expect("bind daemon socket");
    let daemon = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept stats query");
        let mut request = [0; 5];
        socket.read_exact(&mut request).expect("read stats query");
        assert_eq!(request[0], MessageType::Status.as_u8());
        socket
            .write_all(&encode_packet(MessageType::Unknown(255), b"newer protocol"))
            .expect("answer with newer frame");
    });

    let error = pty_client::query_stats_in_with_timeout(
        &root,
        "newer",
        Duration::from_millis(500),
    )
    .expect_err("unknown response must be explained");
    daemon.join().expect("fake daemon");
    std::fs::remove_dir_all(root).expect("remove registry");
    let message = error.to_string().to_lowercase();
    assert!(
        message.contains("version") && message.contains("client"),
        "expected a clear outdated-client version error, got: {message}"
    );
}

#[test]
fn an_optional_newer_frame_does_not_hide_a_valid_status() {
    let (mut daemon, client) = std::os::unix::net::UnixStream::pair().unwrap();
    let sender = std::thread::spawn(move || {
        let mut request = [0; 5];
        daemon.read_exact(&mut request).unwrap();
        daemon
            .write_all(&encode_packet(MessageType::Unknown(255), b"optional"))
            .unwrap();
        daemon
            .write_all(&encode_packet(MessageType::Status, br#"{"ok":true}"#))
            .unwrap();
    });
    let response = pty_client::stats::query_status_json_over(client, "mixed", Duration::from_secs(1))
        .expect("known status remains usable");
    sender.join().unwrap();
    assert_eq!(response, r#"{"ok":true}"#);
}

#[test]
fn a_batched_newer_daemon_response_identifies_the_outdated_client() {
    let root = std::env::temp_dir().join(format!(
        "pi-{}",
        pty_testkit::server::random_id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let listener = UnixListener::bind(root.join("newer.sock")).unwrap();
    let daemon = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = [0; 5];
        socket.read_exact(&mut request).unwrap();
        socket
            .write_all(&encode_packet(MessageType::Unknown(255), b"newer protocol"))
            .unwrap();
    });
    let replies = pty_client::query_stats_batch_in(
        &root,
        &["newer".to_string()],
        Duration::from_millis(500),
    );
    daemon.join().unwrap();
    std::fs::remove_dir_all(root).unwrap();
    let error = replies.into_iter().next().unwrap().1.unwrap_err().to_string();
    assert!(error.contains("client is outdated"), "{error}");
}

#[test]
fn batch_keeps_valid_status_after_unknown_frame_without_masking_other_errors() {
    const STATUS: &[u8] = br#"{"name":"compatible","terminal":{"cols":80,"rows":24,"cursorX":0,"cursorY":0,"scrollbackUsed":0,"scrollbackCapacity":0},"process":{"alive":true,"exitCode":null,"pid":123,"resources":null},"daemon":{"pid":456,"resources":null},"clients":{"total":0,"attached":0,"readOnly":0},"modes":{"sgrMouse":false,"cursorHidden":false,"kittyKeyboard":false,"kittyKeyboardFlags":[]},"uptimeSeconds":0,"createdAt":null}"#;
    let root = std::env::temp_dir().join(format!("pi-{}", pty_testkit::server::random_id()));
    std::fs::create_dir_all(&root).unwrap();
    let compatible = UnixListener::bind(root.join("compatible.sock")).unwrap();
    let outdated = UnixListener::bind(root.join("outdated.sock")).unwrap();
    let good = std::thread::spawn(move || {
        let (mut socket, _) = compatible.accept().unwrap();
        let mut request = [0; 5];
        socket.read_exact(&mut request).unwrap();
        socket.write_all(&encode_packet(MessageType::Unknown(255), b"optional")).unwrap();
        socket.write_all(&encode_packet(MessageType::Status, STATUS)).unwrap();
    });
    let bad = std::thread::spawn(move || {
        let (mut socket, _) = outdated.accept().unwrap();
        let mut request = [0; 5];
        socket.read_exact(&mut request).unwrap();
        socket.write_all(&encode_packet(MessageType::Unknown(255), b"newer protocol")).unwrap();
    });
    let replies = pty_client::query_stats_batch_in(
        &root,
        &["compatible".to_string(), "outdated".to_string()],
        Duration::from_millis(500),
    );
    good.join().unwrap();
    bad.join().unwrap();
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(replies[0].1.as_ref().unwrap().name, "compatible");
    assert!(replies[1].1.as_ref().unwrap_err().to_string().contains("client is outdated"));
}
