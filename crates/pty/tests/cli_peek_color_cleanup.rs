mod cli_common;

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::time::{Duration, Instant};

use cli_common::Rig;
use pty_core::protocol::{MessageType, PacketReader, decode_peek, encode_screen};

#[test]
fn ansi_peek_and_wait_restore_host_colors() {
    let rig = Rig::new();
    let listener = UnixListener::bind(rig.path("demo.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let daemon = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut answered = 0;
        while answered < 3 && Instant::now() < deadline {
            let (mut socket, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("accept fake peek: {error}"),
            };
            socket.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
            let mut reader = PacketReader::new();
            let mut buf = [0; 256];
            let before = answered;
            loop {
                match socket.read(&mut buf) {
                    Ok(0) => break, // Registry reachability probe.
                    Ok(n) => {
                        let packets = reader.feed(&buf[..n]).unwrap();
                        for packet in packets {
                            if packet.type_ == MessageType::Peek {
                                let (plain, _) = decode_peek(&packet.payload);
                                let screen = if plain {
                                    &b"READY"[..]
                                } else {
                                    &b"\x1b]11;rgb:ff/00/00\x1b\\READY"[..]
                                };
                                socket.write_all(&encode_screen(screen)).unwrap();
                                answered += 1;
                            }
                        }
                        if answered > before {
                            break;
                        }
                    }
                    Err(error) => panic!("read fake peek: {error}"),
                }
            }
        }
        answered
    });

    let oneshot = rig.run(&["peek", "demo"]);
    let waiting = rig.run(&["peek", "--wait", "READY", "-t", "2", "demo"]);
    assert_eq!(daemon.join().unwrap(), 3);
    for out in [oneshot, waiting] {
        assert_eq!(out.code, 0, "{out:?}");
        let set = out.stdout.find("\x1b]11;rgb:ff/00/00\x1b\\")
            .unwrap_or_else(|| panic!("missing color setter: {out:?}"));
        let reset = out.stdout.rfind("\x1b]111\x1b\\")
            .unwrap_or_else(|| panic!("missing color reset: {out:?}"));
        assert!(set < reset, "host color was not restored: {out:?}");
    }
}
