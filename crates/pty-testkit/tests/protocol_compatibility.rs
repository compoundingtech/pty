use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::time::Duration;

use pty_core::protocol::{MessageType, encode_packet};

/// A newer daemon may answer with a frame this client does not know. The
/// operator needs to know which side to upgrade before touching live sessions.
#[test]
#[ignore = "fails on main: an unknown daemon response becomes a generic stats timeout"]
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
