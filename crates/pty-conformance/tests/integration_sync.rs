//! Port of the first half of tests/integration.test.ts (lines 257–1328):
//! attach basics, the attach-synchronization ordering (`GEOMETRY -> SCREEN ->
//! DATA* -> EXIT?`), role replacement on one socket (PEEK <-> ATTACH),
//! malformed ATTACH payloads, the redraw nudge, peekers, scrollback replay,
//! exit metadata, session-name validation and the creation lock.
//!
//! Node launches `PtyServer` in-process and fakes the 80 ms settle timer;
//! here every session is a real `pty run -d` daemon and the timers are real,
//! so the sync tests act inside the settle window (well under 80 ms) and
//! assert the same packet sequences. In-process hooks are replaced by what a
//! socket client can observe:
//!
//! - :473 (`holdTerminalWrites` parser backlog) is expressed as "DATA sent
//!   during synchronization is never lost: it lands in SCREEN or DATA".
//! - :618 (an EXIT injected through `server.broadcast` ahead of DATA) has no
//!   socket-level equivalent and is left out.
//! - :1021/:1045 use a `sh` trap instead of a Node SIGWINCH reporter.
//! - :1067 (`server.close()`) is SIGTERM to the daemon.
//! - :1291/:1303/:1315 (library `validateName`/`acquireLock`) go through
//!   `pty run -d --id`; an empty `--id` is falsy for the Node CLI and picks
//!   a random id, so the `/empty/` case is not reachable from the CLI.
//! - Sessions started with a 40×120 geometry in Node use the CLI default
//!   24×80 here (Node's `pty run` has no size flags).

use pty_conformance::*;
use pty_core::protocol::{MessageType, Packet, decode_exit, decode_geometry, encode_packet};
use std::time::Duration;

const T: Duration = Duration::from_secs(5);

fn cat(rig: &Rig, id: &str) {
    rig.daemon(id, &["cat"], DaemonOpts::no_display_name());
}

fn shell(rig: &Rig, id: &str, script: &str, opts: DaemonOpts) {
    rig.daemon(id, &["sh", "-c", script], opts);
}

/// SCREEN and DATA payloads joined, in order.
fn replay_text(packets: &[Packet]) -> String {
    let mut s = Vec::new();
    for p in packets {
        if p.type_ == MessageType::Screen || p.type_ == MessageType::Data {
            s.extend_from_slice(&p.payload);
        }
    }
    String::from_utf8_lossy(&s).into_owned()
}

fn payload_text(p: &Packet) -> String {
    String::from_utf8_lossy(&p.payload).into_owned()
}

/// Read packets until the concatenated DATA contains `needle`; returns the
/// packets read (all types) or panics on timeout.
fn wait_for_data(conn: &mut Conn, needle: &str, timeout: Duration) -> Vec<Packet> {
    let start = std::time::Instant::now();
    let mut got = Vec::new();
    let mut acc = String::new();
    loop {
        let remaining = timeout.saturating_sub(start.elapsed());
        let Some(p) = conn.next_packet(remaining) else {
            panic!("timed out waiting for {needle:?} in DATA; got {acc:?}");
        };
        if p.type_ == MessageType::Data {
            acc.push_str(&payload_text(&p));
        }
        got.push(p);
        if acc.contains(needle) {
            return got;
        }
    }
}

/// Read packets until `pred` holds over everything read so far.
fn read_until(conn: &mut Conn, timeout: Duration, mut pred: impl FnMut(&[Packet]) -> bool) -> Vec<Packet> {
    let start = std::time::Instant::now();
    let mut got: Vec<Packet> = Vec::new();
    loop {
        if pred(&got) {
            return got;
        }
        let remaining = timeout.saturating_sub(start.elapsed());
        match conn.next_packet(remaining) {
            Some(p) => got.push(p),
            None => panic!(
                "timed out waiting for packets; got {:?}",
                sequence_names(&got.iter().map(|p| p.type_).collect::<Vec<_>>())
            ),
        }
    }
}

fn types(packets: &[Packet]) -> Vec<MessageType> {
    packets.iter().map(|p| p.type_).collect()
}

fn count(packets: &[Packet], t: MessageType) -> usize {
    packets.iter().filter(|p| p.type_ == t).count()
}

fn has(packets: &[Packet], t: MessageType) -> bool {
    count(packets, t) > 0
}

fn geometry_of(p: &Packet) -> (u16, u16) {
    decode_geometry(&p.payload)
}

fn attach_and_wait_screen(conn: &mut Conn, rows: u16, cols: u16) -> Packet {
    conn.attach(rows, cols);
    conn.wait_for(MessageType::Screen, T).expect("SCREEN after ATTACH")
}

/// node: tests/integration.test.ts:257
#[test]
fn starts_a_session_and_receives_screen_on_attach() {
    let rig = Rig::new();
    shell(&rig, "s1", "echo hello; exec sleep 30", DaemonOpts::no_display_name());
    let mut c = rig.connect("s1");
    let screen = attach_and_wait_screen(&mut c, 24, 80);
    assert_eq!(screen.type_, MessageType::Screen);
}

/// node: tests/integration.test.ts:275
#[test]
fn receives_process_output_via_screen_replay() {
    let rig = Rig::new();
    shell(&rig, "s2", "echo 'hello world'; exec sleep 30", DaemonOpts::no_display_name());
    std::thread::sleep(Duration::from_millis(200));
    let mut c = rig.connect("s2");
    let screen = attach_and_wait_screen(&mut c, 24, 80);
    expect_contains(&payload_text(&screen), "hello world");
}

/// node: tests/integration.test.ts:295
#[test]
fn sends_input_to_the_pty_process() {
    let rig = Rig::new();
    cat(&rig, "s3");
    let mut c = rig.connect("s3");
    attach_and_wait_screen(&mut c, 24, 80);
    c.data(b"test input\n");
    wait_for_data(&mut c, "test input", Duration::from_secs(3));
}

/// node: tests/integration.test.ts:318
#[test]
fn detach_and_reattach_with_screen_replay() {
    let rig = Rig::new();
    shell(&rig, "s4", "echo 'persistent output'; exec sleep 30", DaemonOpts::no_display_name());
    std::thread::sleep(Duration::from_millis(200));

    let mut c1 = rig.connect("s4");
    c1.attach(24, 80);
    let packets = read_until(&mut c1, Duration::from_secs(3), |got| {
        has(got, MessageType::Screen)
    });
    expect_contains(&replay_text(&packets), "persistent output");

    // DETACH: the daemon closes the socket.
    c1.detach();
    let closed = poll_for(T, || {
        let _ = c1.next_packet(Duration::from_millis(20));
        c1.is_eof()
    });
    assert!(closed, "daemon did not close the socket after DETACH");

    let mut c2 = rig.connect("s4");
    let screen = attach_and_wait_screen(&mut c2, 24, 80);
    expect_contains(&payload_text(&screen), "persistent output");
}

/// node: tests/integration.test.ts:356
#[test]
fn receives_exit_when_process_terminates() {
    let rig = Rig::new();
    shell(&rig, "s5", "sleep 0.4; exit 42", DaemonOpts::no_display_name());
    let mut c = rig.connect("s5");
    c.attach(24, 80);
    let exit = c.wait_for(MessageType::Exit, Duration::from_secs(3)).expect("EXIT");
    assert_eq!(decode_exit(&exit.payload), 42);
}

/// node: tests/integration.test.ts:373
#[test]
fn handles_resize() {
    let rig = Rig::new();
    cat(&rig, "s6");
    let mut c = rig.connect("s6");
    attach_and_wait_screen(&mut c, 24, 80);
    c.resize(48, 120);
    c.data(b"after resize\n");
    wait_for_data(&mut c, "after resize", Duration::from_secs(3));
}

/// node: tests/integration.test.ts:395
#[test]
fn supports_multiple_simultaneous_clients() {
    let rig = Rig::new();
    cat(&rig, "s7");
    let mut c1 = rig.connect("s7");
    let mut c2 = rig.connect("s7");
    c1.attach(24, 80);
    c2.attach(24, 80);
    c1.wait_for(MessageType::Screen, T).expect("SCREEN 1");
    c2.wait_for(MessageType::Screen, T).expect("SCREEN 2");
    c1.data(b"shared input\n");
    wait_for_data(&mut c1, "shared input", Duration::from_secs(3));
    wait_for_data(&mut c2, "shared input", Duration::from_secs(3));
}

/// node: tests/integration.test.ts:423
#[test]
fn sends_screen_before_live_data_produced_during_attach_synchronization() {
    let rig = Rig::new();
    cat(&rig, "sync1");
    let mut live = rig.connect("sync1");
    attach_and_wait_screen(&mut live, 24, 80);

    // A different size: the daemon resizes and delays the SCREEN cut by the
    // 80 ms settle window.
    let mut attaching = rig.connect("sync1");
    attaching.attach(20, 70);
    let first = read_until(&mut attaching, T, |got| has(got, GEOMETRY));

    // Live output produced inside the window is folded into the SCREEN.
    live.data(b"during-initial-sync\n");
    wait_for_data(&mut live, "during-initial-sync", Duration::from_secs(3));

    let mut all = first;
    all.extend(read_until(&mut attaching, T, |got| has(got, MessageType::Screen)));
    all.extend(attaching.drain(Duration::from_millis(300)));

    assert_eq!(
        sequence_names(&types(&all)),
        vec!["GEOMETRY", "SCREEN"],
        "attaching client saw {:?}",
        sequence_names(&types(&all))
    );
    let screen = all.iter().find(|p| p.type_ == MessageType::Screen).unwrap();
    expect_contains(&payload_text(screen), "during-initial-sync");
}

/// node: tests/integration.test.ts:473
#[test]
fn does_not_lose_data_queued_during_attach_synchronization() {
    let rig = Rig::new();
    shell(&rig, "sync2", "stty -echo; exec cat", DaemonOpts::no_display_name());
    std::thread::sleep(Duration::from_millis(50));
    let mut live = rig.connect("sync2");
    attach_and_wait_screen(&mut live, 24, 80);

    let mut attaching = rig.connect("sync2");
    attaching.attach(20, 70);
    let mut all = read_until(&mut attaching, T, |got| has(got, GEOMETRY));

    live.data(b"parser-backlog\n");
    wait_for_data(&mut live, "parser-backlog", Duration::from_secs(3));

    all.extend(read_until(&mut attaching, T, |got| has(got, MessageType::Screen)));
    all.extend(attaching.drain(Duration::from_millis(300)));
    expect_contains(&replay_text(&all), "parser-backlog");
}

/// node: tests/integration.test.ts:531
#[test]
fn flushes_post_cut_data_before_a_post_cut_exit() {
    let rig = Rig::new();
    shell(
        &rig,
        "sync3",
        "stty -echo; read value; printf 'post-cut-data'; exit 7",
        DaemonOpts::no_display_name(),
    );
    std::thread::sleep(Duration::from_millis(50));
    let mut live = rig.connect("sync3");
    attach_and_wait_screen(&mut live, 24, 80);

    let mut attaching = rig.connect("sync3");
    attaching.attach(20, 70);
    let mut all = read_until(&mut attaching, T, |got| has(got, GEOMETRY));
    // Back to the live client's size: a second GEOMETRY before the cut.
    attaching.resize(24, 80);
    all.extend(read_until(&mut attaching, T, |got| count(got, GEOMETRY) >= 1));
    // Let the cut go live before the child produces its final output.
    all.extend(read_until(&mut attaching, T, |got| has(got, MessageType::Screen)));

    live.data(b"go\n");
    let live_packets = wait_for_data(&mut live, "post-cut-data", Duration::from_secs(3));
    let _ = live_packets;
    live.wait_for(MessageType::Exit, T).expect("live client EXIT");

    all.extend(read_until(&mut attaching, T, |got| has(got, MessageType::Exit)));
    all.extend(attaching.drain(Duration::from_millis(200)));

    assert_eq!(
        sequence_names(&types(&all)),
        vec!["GEOMETRY", "GEOMETRY", "SCREEN", "DATA", "EXIT"],
    );
    let data = all.iter().find(|p| p.type_ == MessageType::Data).unwrap();
    expect_contains(&payload_text(data), "post-cut-data");
    let exits: Vec<&Packet> = all.iter().filter(|p| p.type_ == MessageType::Exit).collect();
    assert_eq!(exits.len(), 1);
    assert_eq!(decode_exit(&exits[0].payload), 7);
}

/// node: tests/integration.test.ts:701
#[test]
fn sends_one_exit_after_screen_when_the_child_exits_during_attach_synchronization() {
    let rig = Rig::new();
    rig.daemon("sync4", &["sh"], DaemonOpts::no_display_name());
    let mut live = rig.connect("sync4");
    attach_and_wait_screen(&mut live, 24, 80);

    let mut attaching = rig.connect("sync4");
    attaching.attach(20, 70);
    let mut all = read_until(&mut attaching, T, |got| has(got, GEOMETRY));
    attaching.resize(24, 80);
    all.extend(read_until(&mut attaching, T, |got| count(got, GEOMETRY) >= 1));

    // Inside the settle window: the shell exits.
    live.data(b"exit 7\n");
    live.wait_for(MessageType::Exit, T).expect("live client EXIT");

    all.extend(read_until(&mut attaching, T, |got| {
        has(got, MessageType::Screen) && has(got, MessageType::Exit)
    }));
    all.extend(attaching.drain(Duration::from_millis(200)));

    assert_eq!(
        sequence_names(&types(&all)),
        vec!["GEOMETRY", "GEOMETRY", "SCREEN", "EXIT"],
    );
    let exits: Vec<&Packet> = all.iter().filter(|p| p.type_ == MessageType::Exit).collect();
    assert_eq!(exits.len(), 1);
    assert_eq!(decode_exit(&exits[0].payload), 7);
}

/// node: tests/integration.test.ts:764
#[test]
fn supersedes_an_earlier_pending_attach_on_the_same_client() {
    let rig = Rig::new();
    cat(&rig, "sync5");
    let mut live = rig.connect("sync5");
    attach_and_wait_screen(&mut live, 24, 80);

    let mut re = rig.connect("sync5");
    re.attach(20, 70);
    let mut all = read_until(&mut re, T, |got| !got.is_empty());
    re.attach(18, 60);
    all.extend(read_until(&mut re, T, |got| !got.is_empty()));
    all.extend(read_until(&mut re, T, |got| has(got, MessageType::Screen)));
    all.extend(re.drain(Duration::from_millis(300)));

    assert_eq!(
        sequence_names(&types(&all)),
        vec!["GEOMETRY", "GEOMETRY", "SCREEN"],
    );
}

/// node: tests/integration.test.ts:802
#[test]
fn cancels_pending_attach_synchronization_when_the_client_switches_to_peek() {
    let rig = Rig::new();
    cat(&rig, "sync6");
    let mut live = rig.connect("sync6");
    attach_and_wait_screen(&mut live, 24, 80);

    let mut peeker = rig.connect("sync6");
    peeker.attach(20, 70);
    let mut all = read_until(&mut peeker, T, |got| !got.is_empty());
    peeker.peek(false, false);
    all.extend(read_until(&mut peeker, T, |got| has(got, MessageType::Screen)));
    all.extend(peeker.drain(Duration::from_millis(300)));

    assert_eq!(
        sequence_names(&types(&all)),
        vec!["GEOMETRY", "GEOMETRY", "SCREEN"],
    );

    live.data(b"peek-is-live\n");
    wait_for_data(&mut live, "peek-is-live", Duration::from_secs(3));
    wait_for_data(&mut peeker, "peek-is-live", Duration::from_secs(3));
}

/// node: tests/integration.test.ts:854
#[test]
fn replaces_a_same_socket_peek_role_with_attach() {
    let rig = Rig::new();
    cat(&rig, "role1");
    let mut c = rig.connect("role1");
    c.peek(false, false);
    c.wait_for(MessageType::Screen, T).expect("first SCREEN");

    c.attach(20, 70);
    c.wait_for(MessageType::Screen, T).expect("second SCREEN");
    c.data(b"writable-again\n");
    wait_for_data(&mut c, "writable-again", T);

    let mut stats = rig.connect("role1");
    let st = stats.status_json(T);
    assert_eq!(st["terminal"]["rows"], 20, "{st}");
    assert_eq!(st["terminal"]["cols"], 70, "{st}");
    assert_eq!(st["clients"]["attached"], 1, "{st}");
    assert_eq!(st["clients"]["readOnly"], 0, "{st}");

    c.resize(18, 60);
    read_until(&mut c, T, |got| {
        got.iter().any(|p| p.type_ == GEOMETRY && geometry_of(p) == (18, 60))
    });
}

/// node: tests/integration.test.ts:901
#[test]
fn replaces_a_same_socket_attach_role_with_peek() {
    let rig = Rig::new();
    cat(&rig, "role2");
    let mut c = rig.connect("role2");
    attach_and_wait_screen(&mut c, 20, 70);
    c.peek(false, false);
    c.wait_for(MessageType::Screen, T).expect("second SCREEN");

    let mut bytes = pty_core::protocol::encode_resize(18, 60);
    bytes.extend(pty_core::protocol::encode_data(b"must-not-reach-cat\n"));
    bytes.extend(pty_core::protocol::encode_status());
    c.write_raw(&bytes).unwrap();
    let got = read_until(&mut c, T, |got| has(got, MessageType::Status));
    let status = got.iter().rev().find(|p| p.type_ == MessageType::Status).unwrap();
    let st: serde_json::Value = serde_json::from_slice(&status.payload).unwrap();
    assert_eq!(st["clients"]["attached"], 0, "{st}");
    assert_eq!(st["clients"]["readOnly"], 1, "{st}");
    assert_eq!(st["terminal"]["rows"], 20, "{st}");
    assert_eq!(st["terminal"]["cols"], 70, "{st}");

    let mut observer = rig.connect("role2");
    observer.attach(20, 70);
    let mut seen = read_until(&mut observer, T, |got| has(got, MessageType::Screen));
    observer.data(b"accepted-by-cat\n");
    seen.extend(wait_for_data(&mut observer, "accepted-by-cat", T));
    expect_not_contains(&replay_text(&seen), "must-not-reach-cat");
}

/// node: tests/integration.test.ts:961
#[test]
fn does_not_change_either_role_for_a_malformed_attach_payload() {
    let rig = Rig::new();
    cat(&rig, "role3");

    let mut peeker = rig.connect("role3");
    peeker.peek(false, false);
    read_until(&mut peeker, T, |got| has(got, GEOMETRY));
    let mut bytes = encode_packet(MessageType::Attach, &[0, 0]);
    bytes.extend(pty_core::protocol::encode_status());
    peeker.write_raw(&bytes).unwrap();
    let got = read_until(&mut peeker, T, |got| has(got, MessageType::Status));
    let status = got.iter().rev().find(|p| p.type_ == MessageType::Status).unwrap();
    let st: serde_json::Value = serde_json::from_slice(&status.payload).unwrap();
    assert_eq!(st["clients"]["attached"], 0, "{st}");
    assert_eq!(st["clients"]["readOnly"], 1, "{st}");
    if !has(&got, MessageType::Screen) {
        peeker.wait_for(MessageType::Screen, T).expect("peek SCREEN");
    }

    let mut attached = rig.connect("role3");
    attach_and_wait_screen(&mut attached, 20, 70);
    let mut bytes = encode_packet(MessageType::Attach, &[0, 0]);
    bytes.extend(pty_core::protocol::encode_status());
    attached.write_raw(&bytes).unwrap();
    let got = read_until(&mut attached, T, |got| has(got, MessageType::Status));
    let status = got.iter().rev().find(|p| p.type_ == MessageType::Status).unwrap();
    let st: serde_json::Value = serde_json::from_slice(&status.payload).unwrap();
    assert_eq!(st["clients"]["attached"], 1, "{st}");
    assert_eq!(st["clients"]["readOnly"], 1, "{st}");
    assert_eq!(st["terminal"]["rows"], 20, "{st}");
    assert_eq!(st["terminal"]["cols"], 70, "{st}");
}

/// A session whose shell appends `WINCH` to `marker` on every SIGWINCH.
fn winch_reporter(rig: &Rig, id: &str) -> std::path::PathBuf {
    let marker = rig.tmp().join(format!("{id}-winch"));
    let script = format!(
        "trap 'echo WINCH >> {m}' WINCH; echo READY; while :; do sleep 0.05; done",
        m = marker.display()
    );
    shell(rig, id, &script, DaemonOpts::no_display_name());
    std::thread::sleep(Duration::from_millis(100));
    marker
}

/// node: tests/integration.test.ts:1021
#[test]
fn skips_the_redraw_sigwinch_nudge_at_the_sessions_current_size() {
    let rig = Rig::new();
    let marker = winch_reporter(&rig, "winch1");
    let mut c = rig.connect("winch1");
    attach_and_wait_screen(&mut c, 24, 80);
    std::thread::sleep(Duration::from_millis(300));
    assert!(!marker.exists(), "child got SIGWINCH on a same-size attach");
}

/// node: tests/integration.test.ts:1045
#[test]
fn signals_the_child_when_the_attaching_clients_size_differs() {
    let rig = Rig::new();
    let marker = winch_reporter(&rig, "winch2");
    let mut c = rig.connect("winch2");
    attach_and_wait_screen(&mut c, 20, 70);
    wait_until("SIGWINCH marker", || {
        std::fs::read_to_string(&marker).map(|s| s.contains("WINCH")).unwrap_or(false)
    });
}

/// The attach's own resize waits out the redraw settle before the SCREEN
/// cut, so the child's redraw is already in it: no redraw nudge follows and
/// the child sees one SIGWINCH. Node always nudges a size-differing attach.
#[test]
fn a_resizing_attach_costs_the_child_one_sigwinch() {
    if is_node() {
        return;
    }
    let rig = Rig::new();
    let marker = winch_reporter(&rig, "winch3");
    let mut c = rig.connect("winch3");
    attach_and_wait_screen(&mut c, 20, 70);
    std::thread::sleep(Duration::from_millis(400));
    let winches = std::fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(winches.lines().count(), 1, "SIGWINCHes: {winches:?}");
}

/// A larger attacher behind a smaller writer leaves min-wins unchanged: the
/// child is not resized, and its screen is already current, so it is not
/// nudged either. Node nudges whenever the attach size differs.
#[test]
fn an_attach_that_min_wins_leaves_unchanged_sends_no_sigwinch() {
    if is_node() {
        return;
    }
    let rig = Rig::new();
    let marker = winch_reporter(&rig, "winch4");
    let mut small = rig.connect("winch4");
    attach_and_wait_screen(&mut small, 24, 80);
    let mut large = rig.connect("winch4");
    attach_and_wait_screen(&mut large, 82, 113);
    std::thread::sleep(Duration::from_millis(400));
    assert!(!marker.exists(), "SIGWINCH for an attach that changed nothing");
}

/// node: tests/integration.test.ts:1067
#[test]
fn cleans_up_socket_and_pid_files_on_close() {
    let rig = Rig::new();
    let d = rig.daemon("close1", &["cat"], DaemonOpts::no_display_name());
    assert!(d.socket_path().exists());
    let pid = d.pid();
    kill_pid(pid, libc::SIGTERM);
    wait_until("daemon exit", || process_exited(pid));
    wait_until("socket removed", || !d.socket_path().exists());
    assert!(!d.pid_path().exists(), "pid file left behind");
}

/// node: tests/integration.test.ts:1086
#[test]
fn peek_receives_screen_replay_without_affecting_the_session() {
    let rig = Rig::new();
    shell(&rig, "peek1", "echo 'peek test output'; exec sleep 30", DaemonOpts::no_display_name());
    std::thread::sleep(Duration::from_millis(200));
    let mut peeker = rig.connect("peek1");
    peeker.peek(false, false);
    let screen = peeker.wait_for(MessageType::Screen, T).expect("SCREEN");
    expect_contains(&payload_text(&screen), "peek test output");
}

/// node: tests/integration.test.ts:1103
#[test]
fn peek_client_input_is_ignored_by_server() {
    let rig = Rig::new();
    cat(&rig, "peek2");
    let mut watcher = rig.connect("peek2");
    attach_and_wait_screen(&mut watcher, 24, 80);

    let mut peeker = rig.connect("peek2");
    peeker.peek(false, false);
    peeker.wait_for(MessageType::Screen, T).expect("SCREEN");
    peeker.data(b"this should be ignored\n");

    let packets = watcher.drain(Duration::from_millis(500));
    expect_not_contains(&replay_text(&packets), "this should be ignored");
}

/// node: tests/integration.test.ts:1134
#[test]
fn peek_client_does_not_affect_terminal_size() {
    let rig = Rig::new();
    cat(&rig, "peek3");
    let mut c = rig.connect("peek3");
    attach_and_wait_screen(&mut c, 30, 100);

    let mut peeker = rig.connect("peek3");
    peeker.peek(false, false);
    peeker.wait_for(MessageType::Screen, T).expect("SCREEN");
    peeker.resize(10, 10);

    c.data(b"still works\n");
    wait_for_data(&mut c, "still works", Duration::from_secs(3));
    let st = peeker.status_json(T);
    assert_eq!(st["terminal"]["rows"], 30, "{st}");
    assert_eq!(st["terminal"]["cols"], 100, "{st}");
}

/// node: tests/integration.test.ts:1161
#[test]
fn peek_receives_live_data_when_following() {
    let rig = Rig::new();
    cat(&rig, "peek4");
    let mut peeker = rig.connect("peek4");
    peeker.peek(false, false);
    peeker.wait_for(MessageType::Screen, T).expect("SCREEN");

    let mut c = rig.connect("peek4");
    attach_and_wait_screen(&mut c, 24, 80);
    c.data(b"live data\n");
    wait_for_data(&mut peeker, "live data", Duration::from_secs(3));
}

/// node: tests/integration.test.ts:1186
#[test]
fn peek_captures_tui_app_running_in_alternate_screen_buffer() {
    let rig = Rig::new();
    let script = "printf '\\033[?1049h'; printf '\\033[?1000h'; printf '\\033[?1003h'; \
                  printf '\\033[?1h'; printf '\\033[H'; \
                  printf '\\033[32mTUI-PEEK-TEST\\033[0m\\n'; printf 'Status: running\\n'; exec sleep 30";
    shell(&rig, "peek5", script, DaemonOpts::no_display_name());
    std::thread::sleep(Duration::from_millis(300));
    let mut peeker = rig.connect("peek5");
    peeker.peek(false, false);
    let screen = payload_text(&peeker.wait_for(MessageType::Screen, T).expect("SCREEN"));
    expect_contains(&screen, "TUI-PEEK-TEST");
    expect_contains(&screen, "Status: running");
}

/// node: tests/integration.test.ts:1216
#[test]
fn writes_session_metadata_on_creation() {
    let rig = Rig::new();
    let d = rig.daemon("meta1", &["cat", "-u"], DaemonOpts::no_display_name());
    let meta = d.meta();
    let command = meta["command"].as_str().expect("command");
    assert!(
        command == "cat" || command.ends_with("/cat"),
        "command {command:?} is not cat"
    );
    assert_eq!(meta["args"], serde_json::json!(["-u"]));
    assert!(meta["createdAt"].as_str().map(|s| !s.is_empty()).unwrap_or(false), "{meta}");
    assert!(meta.get("exitCode").is_none(), "{meta}");
}

/// node: tests/integration.test.ts:1228
#[test]
fn screen_replay_includes_scrollback_content() {
    let rig = Rig::new();
    let script: String = (0..40)
        .map(|i| format!("echo 'scrollback-line-{i}'"))
        .collect::<Vec<_>>()
        .join("; ")
        + "; exec sleep 30";
    shell(&rig, "scroll1", &script, DaemonOpts::no_display_name());
    std::thread::sleep(Duration::from_millis(300));
    let mut c = rig.connect("scroll1");
    let screen = payload_text(&attach_and_wait_screen(&mut c, 24, 80));
    expect_contains(&screen, "scrollback-line-39");
    expect_contains(&screen, "scrollback-line-0");
}

/// node: tests/integration.test.ts:1252
#[test]
fn saves_last_lines_and_exit_code_on_process_exit() {
    let rig = Rig::new();
    shell(
        &rig,
        "exit1",
        "echo 'line one'; echo 'line two'; echo 'line three'; sleep 0.2; exit 7",
        DaemonOpts::keep(),
    );
    rig.wait_for_exit("exit1");
    let meta = rig.meta("exit1").unwrap();
    assert_eq!(meta["exitCode"], 7, "{meta}");
    assert!(meta["exitedAt"].as_str().map(|s| !s.is_empty()).unwrap_or(false), "{meta}");
    let lines = meta["lastLines"].as_array().expect("lastLines");
    let joined: Vec<&str> = lines.iter().filter_map(|l| l.as_str()).collect();
    assert!(joined.iter().any(|l| l.contains("line one")), "{joined:?}");
    assert!(joined.iter().any(|l| l.contains("line three")), "{joined:?}");
}

/// node: tests/integration.test.ts:1271
#[test]
fn metadata_persists_after_server_closes() {
    let rig = Rig::new();
    let d = rig.daemon(
        "exit2",
        &["sh", "-c", "echo 'persist me'; sleep 0.2; exit 0"],
        DaemonOpts::keep(),
    );
    rig.wait_for_exit("exit2");
    let pid = d.pid();
    wait_until("daemon exit", || process_exited(pid));
    wait_until("socket removed", || !d.socket_path().exists());
    assert!(d.meta_path().exists(), "metadata removed");
    let meta = d.meta();
    let lines = meta["lastLines"].as_array().expect("lastLines");
    assert!(
        lines.iter().any(|l| l.as_str().map(|s| s.contains("persist me")).unwrap_or(false)),
        "{meta}"
    );
}

/// node: tests/integration.test.ts:1291
#[test]
fn validates_session_names() {
    let rig = Rig::new();
    for good in ["good-name", "my.session_1"] {
        let out = rig.pty(&["run", "-d", "--id", good, "--no-display-name", "--", "cat"]);
        expect_status(&out, 0);
    }
    for bad in ["bad/name", "../traversal", ".", "..", "has spaces"] {
        let out = rig.pty(&["run", "-d", "--id", bad, "--no-display-name", "--", "cat"]);
        expect_failure(&out);
        expect_regex(&out.stderr(), "Invalid session name");
    }
    let long = "a".repeat(256);
    let out = rig.pty(&["run", "-d", "--id", &long, "--no-display-name", "--", "cat"]);
    expect_failure(&out);
    expect_regex(&out.stderr(), "too long");
    let names: Vec<String> = rig
        .list_json()
        .iter()
        .filter_map(|e| e["name"].as_str().map(String::from))
        .collect();
    assert_eq!(names, vec!["good-name", "my.session_1"]);
}

/// node: tests/integration.test.ts:1303
#[test]
fn lock_prevents_double_acquire_release_allows_reacquire() {
    let rig = Rig::new();
    let lock = rig.root().join("lk1.lock");
    // A live holder (this test process).
    std::fs::write(&lock, format!("{}\n", std::process::id())).unwrap();
    let out = rig.pty(&["run", "-d", "--id", "lk1", "--no-display-name", "--", "cat"]);
    expect_failure(&out);
    expect_contains(&out.stderr(), "being created by another process");
    assert!(!rig.meta_path("lk1").exists());
    // Released: creation goes through and the lock is ours to take again.
    std::fs::remove_file(&lock).unwrap();
    let out = rig.pty(&["run", "-d", "--id", "lk1", "--no-display-name", "--", "cat"]);
    expect_status(&out, 0);
    assert!(!lock.exists(), "creation left the lock behind");
}

/// node: tests/integration.test.ts:1315
#[test]
fn lock_with_garbage_content_is_treated_as_stale() {
    let rig = Rig::new();
    let lock = rig.root().join("lk2.lock");
    std::fs::write(&lock, "not-a-pid").unwrap();
    let out = rig.pty(&["run", "-d", "--id", "lk2", "--no-display-name", "--", "cat"]);
    expect_status(&out, 0);
    assert!(!lock.exists(), "stale lock was not released after creation");
}
