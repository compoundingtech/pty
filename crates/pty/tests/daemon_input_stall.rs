//! Input the child has not read yet must not stall the daemon.
mod daemon_support;
use daemon_support::*;
use pty_core::protocol::MessageType;
use std::path::Path;
use std::time::Duration;
const T: Duration = Duration::from_secs(5);
/// A STATUS reply normally takes milliseconds.
const ANSWER: Duration = Duration::from_secs(2);

/// A daemon whose child puts the terminal in raw mode, prints "ready" and
/// execs `program`, with a writable client attached.
fn raw_session(root: &Path, program: &str) -> (Daemon, Conn) {
    let script = format!("stty raw -echo; printf ready; exec {program}");
    let d = Daemon::start(root, config(&unique_name("input"), "sh", &["-c", &script]));
    let mut writer = d.connect();
    writer.attach(24, 80);
    assert!(writer.wait_type(MessageType::Screen, T));
    assert!(writer.wait_text("ready", T));
    (d, writer)
}

#[test]
fn input_the_child_never_reads_does_not_stall_the_daemon() {
    skip_without_a_real_machine!();
    let root = short_root();
    let (d, mut writer) = raw_session(&root, "sleep 30");
    let child = d.child_pid();
    // Far more than a PTY holds for a child that is not reading: Linux
    // buffers about 640 KiB, macOS much less.
    let flood = 4 << 20;
    writer.data(&"x".repeat(flood));
    // The daemon takes this connection's packets in order, so the STATUS
    // reaches it after the DATA.
    writer.status();
    assert!(
        writer.wait_status(ANSWER).is_some(),
        "no STATUS within {ANSWER:?} while the child left {flood} bytes unread"
    );
    let mut other = d.connect();
    other.status();
    assert!(
        other.wait_status(ANSWER).is_some(),
        "another client got no STATUS"
    );
    assert!(
        pid_alive(child),
        "the answers must not depend on the child exiting"
    );
}

#[test]
fn a_paste_arrives_whole_and_in_order_while_the_daemon_answers() {
    skip_without_a_real_machine!();
    let root = short_root();
    let copy = root.join("copy");
    let (_daemon, mut writer) = raw_session(&root, &format!("cat > '{}'", copy.display()));
    let paste = "a".repeat(2 << 20);
    writer.data(&paste);
    writer.status();
    assert!(
        writer.wait_status(ANSWER).is_some(),
        "no STATUS within {ANSWER:?} while the child was reading a paste"
    );
    writer.data("tail");
    let expected = paste.len() + "tail".len();
    let size = || std::fs::metadata(&copy).map_or(0, |m| m.len() as usize);
    assert!(
        wait_until(Duration::from_secs(60), || size() >= expected),
        "the child read {} of {expected} bytes",
        size()
    );
    let got = std::fs::read(&copy).unwrap();
    assert!(
        got.len() == expected && got.starts_with(paste.as_bytes()) && got.ends_with(b"tail"),
        "the child read {} bytes, not the paste then the tail",
        got.len()
    );
}
