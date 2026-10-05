//! A real session must recover a crashed program's modes without feeding reset bytes to it.
mod daemon_support;
use daemon_support::*;
use pty_core::protocol::{MessageType, encode_packet};
use std::time::Duration;
const T: Duration = Duration::from_secs(5);
#[test]
fn writable_surface_recovers_modes_for_live_and_later_clients() {
    skip_without_a_real_machine!();
    let root = short_root();
    let d = Daemon::start(
        &root,
        config(
            &unique_name("copper"),
            "sh",
            &[
                "-c",
                "printf 'copper history\\r\\n\\033[?1003h\\033[?2004h\\033[>27u'; stty -echo; while read value; do printf 'input:%s\\r\\n' \"$value\"; done",
            ],
        ),
    );
    let mut live = d.connect();
    live.attach(24, 80);
    assert!(live.wait_type(MessageType::Screen, T));
    assert!(live.wait_text("copper history", T));
    let mut reader = d.connect();
    reader.peek();
    assert!(reader.wait_type(MessageType::Screen, T));
    reader.send(&encode_packet(MessageType::ResetInputModes, &[]));
    reader.status();
    let mode_before = reader.wait_status(T).unwrap();
    assert!(
        mode_before["modes"]["kittyKeyboard"]
            .as_bool()
            .unwrap_or(false),
        "{mode_before}"
    );
    live.send(&encode_packet(MessageType::ResetInputModes, &[]));
    assert!(live.wait_text("\x1b[?1003l", T), "reset broadcast");
    live.data("after-recovery\n");
    assert!(
        live.wait_text("input:after-recovery", T),
        "reset is output, never child input"
    );
    let mut later = d.connect();
    later.attach(24, 80);
    assert!(later.wait_type(MessageType::Screen, T));
    let screen = later.screen().unwrap();
    assert!(screen.contains("copper history"), "{screen:?}");
    assert!(!screen.contains("\x1b[?1003h"), "reset survives reconnect");
    assert!(
        !screen.contains("\x1b[>27u"),
        "keyboard stack reset survives reconnect"
    );
}
