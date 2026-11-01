//! Input the child has not read yet must not stall the daemon, and the
//! daemon holds only a budget's worth of it.
mod daemon_support;
use daemon_support::*;
use pty_core::protocol::{MessageType, encode_data};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
const T: Duration = Duration::from_secs(5);
/// A STATUS reply normally takes milliseconds.
const ANSWER: Duration = Duration::from_secs(2);
/// The daemon's `INPUT_BUDGET` (`crates/pty/src/daemon/input_budget.rs`).
const BUDGET: usize = 1 << 20;
/// What a sender gets past the budget besides it: the packet its reader
/// waits to hold, the reader's partial packet, and the socket's buffers.
const SLACK: usize = 2 << 20;
const PACKET: usize = 64 << 10;

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

/// Write `total` bytes of `byte` to the session as DATA packets from a
/// thread of its own, counting what the socket has accepted.
fn flood(d: &Daemon, byte: u8, total: usize) -> Arc<AtomicUsize> {
    let mut stream = UnixStream::connect(d.socket_path()).expect("connect");
    let accepted = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&accepted);
    std::thread::spawn(move || {
        let packet = encode_data(&vec![byte; PACKET]);
        while counted.load(Ordering::SeqCst) < total {
            if stream.write_all(&packet).is_err() {
                return;
            }
            counted.fetch_add(PACKET, Ordering::SeqCst);
        }
    });
    accepted
}

/// `count` once it has not moved for a second, or after `max`.
fn settled(count: &AtomicUsize, max: Duration) -> usize {
    let deadline = Instant::now() + max;
    let mut last = count.load(Ordering::SeqCst);
    let mut since = Instant::now();
    while Instant::now() < deadline && since.elapsed() < Duration::from_secs(1) {
        std::thread::sleep(Duration::from_millis(50));
        let now = count.load(Ordering::SeqCst);
        if now != last {
            last = now;
            since = Instant::now();
        }
    }
    last
}

/// The daemon's resident memory, where the kernel reports it.
fn resident_bytes(pid: i32) -> Option<usize> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let kib: usize = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

#[test]
fn input_past_the_budget_waits_in_the_sender_while_others_are_served() {
    skip_without_a_real_machine!();
    let root = short_root();
    let go = root.join("go");
    let copy = root.join("copy");
    // The child reads nothing until the test creates `go`.
    let program = format!(
        "sh -c 'while [ ! -e \"{}\" ]; do sleep 0.05; done; exec cat > \"{}\"'",
        go.display(),
        copy.display()
    );
    let (d, _writer) = raw_session(&root, &program);
    let before = resident_bytes(d.pid);
    let total = 16 << 20;
    let accepted = flood(&d, b'x', total);
    let held = settled(&accepted, Duration::from_secs(30));
    assert!(
        held <= BUDGET + SLACK,
        "the daemon took {held} of {total} bytes for a child that reads nothing; the budget is {BUDGET}"
    );
    if let (Some(before), Some(after)) = (before, resident_bytes(d.pid)) {
        let grown = after.saturating_sub(before);
        assert!(
            grown <= BUDGET + SLACK + (4 << 20),
            "the daemon grew by {grown} bytes while holding {held}"
        );
    }

    // Another client's STATUS does not wait for the budget.
    let mut other = d.connect();
    other.status();
    assert!(
        other.wait_status(ANSWER).is_some(),
        "another client got no STATUS while the budget was spent"
    );
    // Its DATA does, and so does the STATUS it sends after that DATA.
    other.send(&encode_data(&vec![b'y'; PACKET]));
    other.status();
    assert!(
        other.wait_status(Duration::from_millis(500)).is_none(),
        "the second client's DATA did not wait for the spent budget"
    );

    // Once the child reads, all of it arrives: nothing was dropped.
    std::fs::write(&go, b"").unwrap();
    assert!(
        other.wait_status(Duration::from_secs(60)).is_some(),
        "the second client's STATUS never came after the child started reading"
    );
    let expected = total + PACKET;
    let size = || std::fs::metadata(&copy).map_or(0, |m| m.len() as usize);
    assert!(
        wait_until(Duration::from_secs(120), || size() >= expected),
        "the child read {} of {expected} bytes",
        size()
    );
    let got = std::fs::read(&copy).unwrap();
    let count = |byte: u8| got.iter().filter(|&&b| b == byte).count();
    assert!(
        got.len() == expected && count(b'x') == total && count(b'y') == PACKET,
        "the child read {} bytes: {} from the first client, {} from the second",
        got.len(),
        count(b'x'),
        count(b'y')
    );
}

#[test]
fn a_paste_past_the_budget_arrives_whole_and_in_order() {
    skip_without_a_real_machine!();
    let root = short_root();
    let copy = root.join("copy");
    let (_daemon, mut writer) = raw_session(&root, &format!("cat > '{}'", copy.display()));
    // Each packet has a letter of its own, so a lost or reordered one shows.
    let mut paste = Vec::new();
    for i in 0..4 * BUDGET / PACKET {
        let chunk = vec![b'a' + (i % 26) as u8; PACKET];
        writer.send(&encode_data(&chunk));
        paste.extend_from_slice(&chunk);
    }
    let size = || std::fs::metadata(&copy).map_or(0, |m| m.len() as usize);
    assert!(
        wait_until(Duration::from_secs(60), || size() >= paste.len()),
        "the child read {} of {} bytes",
        size(),
        paste.len()
    );
    assert!(
        std::fs::read(&copy).unwrap() == paste,
        "the child did not read the paste whole and in order"
    );
}
