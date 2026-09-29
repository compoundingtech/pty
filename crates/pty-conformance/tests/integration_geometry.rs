//! Port of the second half of tests/integration.test.ts (from line 1330):
//! effective geometry observed through `stty size`, malformed packets, the
//! post-resize redraw cursor, terminal-mode replay for late and re-attaching
//! clients, `send`-style command sockets, peek flags, restart metadata, and
//! the STATUS JSON shapes.
//!
//! Node hosts `PtyServer` in-process; here every session is a real daemon
//! started with `pty run -d` and spoken to over its socket. Where Node reads
//! the xterm buffer directly (the cursor test) the SCREEN payload is walked
//! with a small cursor tracker instead. The two "restart" cases, which drive
//! `server.close()` and a fresh `PtyServer` on the same metadata, are
//! re-expressed as `pty kill` followed by `pty run -a -d --id`. Node's
//! `sendData({delayMs})` library call is `pty send --with-delay`. Nothing in
//! this span is left out.

use pty_conformance::*;
use pty_core::protocol::{MessageType, encode_data, encode_packet, encode_resize, encode_status};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// Read packets until the concatenated DATA payloads contain `pattern`.
/// Returns the accumulated text. Panics on timeout.
fn wait_for_content(conn: &mut Conn, pattern: &str, timeout: Duration) -> String {
    let start = Instant::now();
    let mut acc = String::new();
    loop {
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            panic!("timed out waiting for {pattern:?} in DATA (got: {acc:?})");
        }
        match conn.next_packet(remaining) {
            Some(p) if p.type_ == MessageType::Data => {
                acc.push_str(&String::from_utf8_lossy(&p.payload));
                if acc.contains(pattern) {
                    return acc;
                }
            }
            Some(_) => {}
            None => panic!("timed out waiting for {pattern:?} in DATA (got: {acc:?})"),
        }
    }
}

/// Concatenated SCREEN and DATA payloads that arrive within `quiet` of silence.
fn screen_and_data_text(conn: &mut Conn, quiet: Duration) -> String {
    let mut text = String::new();
    for p in conn.drain(quiet) {
        if p.type_ == MessageType::Screen || p.type_ == MessageType::Data {
            text.push_str(&String::from_utf8_lossy(&p.payload));
        }
    }
    text
}

fn attach_and_wait_screen(rig: &Rig, id: &str, rows: u16, cols: u16) -> Conn {
    let mut c = rig.connect(id);
    c.attach(rows, cols);
    c.wait_for(MessageType::Screen, Duration::from_secs(5))
        .unwrap_or_else(|| panic!("no SCREEN for {id} at {rows}x{cols}"));
    c
}

fn stty_size(conn: &mut Conn, expect: &str) {
    conn.data(b"stty size\n");
    wait_for_content(conn, expect, Duration::from_secs(5));
}

/// Walk a SCREEN payload and return the final cursor position (0-indexed
/// row, col) the way a terminal of `cols` columns would place it. Handles
/// what the serializer emits: CUP, relative cursor moves, CR/LF, printable
/// text; every other CSI/OSC sequence is skipped.
fn cursor_after(payload: &[u8], cols: usize) -> (usize, usize) {
    let s = String::from_utf8_lossy(payload);
    let chars: Vec<char> = s.chars().collect();
    let (mut row, mut col) = (0usize, 0usize);
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\x1b' {
            if i + 1 < chars.len() && chars[i + 1] == '[' {
                let mut j = i + 2;
                let mut params = String::new();
                while j < chars.len() && !chars[j].is_ascii_alphabetic() && chars[j] != '@' && chars[j] != '`' {
                    params.push(chars[j]);
                    j += 1;
                }
                let fin = chars.get(j).copied().unwrap_or('\0');
                let nums: Vec<usize> = params
                    .trim_start_matches('?')
                    .trim_start_matches('>')
                    .trim_start_matches('<')
                    .split(';')
                    .map(|p| p.parse().unwrap_or(0))
                    .collect();
                let n = |k: usize, d: usize| nums.get(k).copied().filter(|v| *v > 0).unwrap_or(d);
                if !params.starts_with('?') && !params.starts_with('>') && !params.starts_with('<') {
                    match fin {
                        'H' | 'f' => {
                            row = n(0, 1) - 1;
                            col = n(1, 1) - 1;
                        }
                        'A' => row = row.saturating_sub(n(0, 1)),
                        'B' => row += n(0, 1),
                        'C' => col = (col + n(0, 1)).min(cols - 1),
                        'D' => col = col.saturating_sub(n(0, 1)),
                        'G' => col = n(0, 1) - 1,
                        'd' => row = n(0, 1) - 1,
                        _ => {}
                    }
                }
                i = j + 1;
                continue;
            }
            if i + 1 < chars.len() && chars[i + 1] == ']' {
                // OSC ... BEL or ST
                let mut j = i + 2;
                while j < chars.len() {
                    if chars[j] == '\x07' {
                        j += 1;
                        break;
                    }
                    if chars[j] == '\x1b' && chars.get(j + 1) == Some(&'\\') {
                        j += 2;
                        break;
                    }
                    j += 1;
                }
                i = j;
                continue;
            }
            // Two-character escape (ESC =, ESC >, ESC ( B, ...).
            i += if i + 1 < chars.len() && chars[i + 1] == '(' { 3 } else { 2 };
            continue;
        }
        match ch {
            '\r' => col = 0,
            '\n' => row += 1,
            c if !c.is_control() => {
                if col >= cols {
                    col = 0;
                    row += 1;
                }
                col += 1;
            }
            _ => {}
        }
        i += 1;
    }
    (row, col)
}

// ── Effective geometry via `stty size` ──

/// node: tests/integration.test.ts:1330
#[test]
fn uses_smallest_connected_client_size() {
    let rig = Rig::new();
    rig.daemon("g1", &["sh"], DaemonOpts::no_display_name());
    let mut c1 = attach_and_wait_screen(&rig, "g1", 50, 200);
    stty_size(&mut c1, "50 200");
    let _c2 = attach_and_wait_screen(&rig, "g1", 30, 100);
    stty_size(&mut c1, "30 100");
}

/// node: tests/integration.test.ts:1356
#[test]
fn uses_minimum_of_each_dimension_independently() {
    let rig = Rig::new();
    rig.daemon("g2", &["sh"], DaemonOpts::no_display_name());
    let mut c1 = attach_and_wait_screen(&rig, "g2", 60, 80);
    let _c2 = attach_and_wait_screen(&rig, "g2", 30, 200);
    stty_size(&mut c1, "30 80");
}

/// node: tests/integration.test.ts:1379
#[test]
fn recalculates_size_when_a_client_disconnects() {
    let rig = Rig::new();
    rig.daemon("g3", &["sh"], DaemonOpts::no_display_name());
    let mut c1 = attach_and_wait_screen(&rig, "g3", 50, 200);
    let c2 = attach_and_wait_screen(&rig, "g3", 30, 80);
    stty_size(&mut c1, "30 80");
    drop(c2);
    std::thread::sleep(Duration::from_millis(100));
    stty_size(&mut c1, "50 200");
}

/// node: tests/integration.test.ts:1408
#[test]
fn recalculates_size_on_clean_detach() {
    let rig = Rig::new();
    rig.daemon("g4", &["sh"], DaemonOpts::no_display_name());
    let mut c1 = attach_and_wait_screen(&rig, "g4", 50, 200);
    let mut c2 = attach_and_wait_screen(&rig, "g4", 25, 90);
    stty_size(&mut c1, "25 90");
    c2.detach();
    std::thread::sleep(Duration::from_millis(100));
    stty_size(&mut c1, "50 200");
}

/// node: tests/integration.test.ts:1437
#[test]
fn resize_message_updates_size_negotiation() {
    let rig = Rig::new();
    rig.daemon("g5", &["sh"], DaemonOpts::no_display_name());
    let mut c1 = attach_and_wait_screen(&rig, "g5", 50, 200);
    let mut c2 = attach_and_wait_screen(&rig, "g5", 30, 100);
    stty_size(&mut c1, "30 100");
    c2.resize(60, 250);
    std::thread::sleep(Duration::from_millis(100));
    stty_size(&mut c1, "50 200");
}

// ── Malformed packets ──

/// node: tests/integration.test.ts:1466
#[test]
fn server_handles_truncated_attach_payload_gracefully() {
    let rig = Rig::new();
    rig.daemon("m1", &["cat"], DaemonOpts::no_display_name());
    let mut c = rig.connect("m1");
    c.write_raw(&encode_packet(MessageType::Attach, &[0, 0])).unwrap();
    c.attach(24, 80);
    let screen = c.wait_for(MessageType::Screen, Duration::from_secs(5)).expect("SCREEN after a bad ATTACH");
    assert_eq!(screen.type_, MessageType::Screen);
}

/// node: tests/integration.test.ts:1486
#[test]
fn server_handles_truncated_resize_payload_gracefully() {
    let rig = Rig::new();
    rig.daemon("m2", &["cat"], DaemonOpts::no_display_name());
    let mut c = attach_and_wait_screen(&rig, "m2", 24, 80);
    c.write_raw(&encode_packet(MessageType::Resize, &[0])).unwrap();
    c.data(b"after-bad-resize\n");
    let text = wait_for_content(&mut c, "after-bad-resize", Duration::from_secs(3));
    assert!(text.contains("after-bad-resize"));
}

/// node: tests/integration.test.ts:1506
#[test]
fn server_ignores_unknown_message_types() {
    let rig = Rig::new();
    rig.daemon("m3", &["cat"], DaemonOpts::no_display_name());
    let mut c = attach_and_wait_screen(&rig, "m3", 24, 80);
    let mut raw = vec![99u8, 0, 0, 0, 3];
    raw.extend_from_slice(b"abc");
    c.write_raw(&raw).unwrap();
    c.data(b"after-unknown\n");
    let text = wait_for_content(&mut c, "after-unknown", Duration::from_secs(3));
    assert!(text.contains("after-unknown"));
}

// ── Cursor after resize ──

/// node: tests/integration.test.ts:1545
#[test]
fn screen_cursor_position_matches_process_intent_after_resize() {
    let rig = Rig::new();
    let script = "printf '\\033[?1049h\\033[2J\\033[1;1HTitle\\033[5;60H'; \
                  trap 'printf \"\\033[2J\\033[1;1HTitle\\033[5;10H\"' WINCH; \
                  sleep 300 & wait; sleep 300";
    rig.daemon("cur", &["bash", "-c", script], DaemonOpts::no_display_name());
    std::thread::sleep(Duration::from_millis(500));

    let mut c1 = rig.connect("cur");
    c1.attach(24, 80);
    let s1 = c1.wait_for(MessageType::Screen, Duration::from_secs(5)).expect("first SCREEN");
    assert_eq!(cursor_after(&s1.payload, 80), (4, 59), "payload: {:?}", String::from_utf8_lossy(&s1.payload));
    drop(c1);
    std::thread::sleep(Duration::from_millis(200));

    let mut c2 = rig.connect("cur");
    c2.attach(24, 40);
    let s2 = c2.wait_for(MessageType::Screen, Duration::from_secs(5)).expect("second SCREEN");
    // The process's SIGWINCH redraw (cursor at 5;10) must be in the SCREEN,
    // not the clamped column from the narrower terminal.
    assert_eq!(cursor_after(&s2.payload, 40), (4, 9), "payload: {:?}", String::from_utf8_lossy(&s2.payload));
}

// ── Terminal modes reach a late attacher ──

const TERMINAL_MODES: &[(&str, &str, &str)] = &[
    ("Kitty keyboard protocol", "\x1b[>1u", "\\033[>1u"),
    ("SGR mouse mode", "\x1b[?1006h", "\\033[?1006h"),
    ("cursor hidden", "\x1b[?25l", "\\033[?25l"),
];

fn mode_late_attach(idx: usize) {
    let (name, enable, printf) = TERMINAL_MODES[idx];
    let rig = Rig::new();
    let script = format!("printf '{printf}'; echo 'ready'; cat");
    rig.daemon("late", &["sh", "-c", &script], DaemonOpts::no_display_name());
    std::thread::sleep(Duration::from_millis(300));
    let mut c = rig.connect("late");
    c.attach(24, 80);
    let text = screen_and_data_text(&mut c, Duration::from_millis(500));
    assert!(text.contains(enable), "{name}: {enable:?} missing from {text:?}");
}

/// node: tests/integration.test.ts:1642
#[test]
fn kitty_keyboard_mode_reaches_client_on_late_attach() {
    mode_late_attach(0);
}

/// node: tests/integration.test.ts:1642
#[test]
fn sgr_mouse_mode_reaches_client_on_late_attach() {
    mode_late_attach(1);
}

/// node: tests/integration.test.ts:1642
#[test]
fn cursor_hidden_mode_reaches_client_on_late_attach() {
    mode_late_attach(2);
}

// ── send (no ATTACH) ──

/// node: tests/integration.test.ts:1676
#[test]
fn send_sends_text_to_a_running_session() {
    let rig = Rig::new();
    rig.daemon("s1", &["cat"], DaemonOpts::no_display_name());
    let mut watcher = attach_and_wait_screen(&rig, "s1", 24, 80);
    let mut sender = rig.connect("s1");
    sender.data(b"hello from send");
    sender.shutdown_write();
    let text = wait_for_content(&mut watcher, "hello from send", Duration::from_secs(3));
    assert!(text.contains("hello from send"));
}

/// node: tests/integration.test.ts:1697
#[test]
fn send_sends_multiple_data_packets_in_sequence() {
    let rig = Rig::new();
    rig.daemon("s2", &["cat"], DaemonOpts::no_display_name());
    let mut watcher = attach_and_wait_screen(&rig, "s2", 24, 80);
    let mut sender = rig.connect("s2");
    sender.data(b"one");
    sender.data(b"two");
    sender.data(b"three\n");
    sender.shutdown_write();
    let mut output = String::new();
    for p in watcher.drain(Duration::from_millis(500)) {
        if p.type_ == MessageType::Data {
            output.push_str(&String::from_utf8_lossy(&p.payload));
        }
    }
    assert!(output.contains("one"), "{output:?}");
    assert!(output.contains("two"), "{output:?}");
    assert!(output.contains("three"), "{output:?}");
}

/// node: tests/integration.test.ts:1725
#[test]
fn send_does_not_trigger_screen_replay() {
    let rig = Rig::new();
    rig.daemon("s3", &["sh", "-c", "echo 'initial output'; cat"], DaemonOpts::no_display_name());
    std::thread::sleep(Duration::from_millis(200));
    let mut sender = rig.connect("s3");
    sender.data(b"sent text");
    let mut got = sender.drain(Duration::from_millis(300));
    sender.shutdown_write();
    // Read until the daemon closes the socket.
    got.extend(sender.collect_until_exit(Duration::from_secs(3)));
    let screens = got.iter().filter(|p| p.type_ == MessageType::Screen).count();
    assert_eq!(screens, 0, "sender received SCREEN: {:?}", sequence_names(&sender.sequence()));
}

/// node: tests/integration.test.ts:1748
#[test]
fn send_sends_items_with_delay_between_them() {
    let rig = Rig::new();
    rig.daemon("s4", &["cat"], DaemonOpts::no_display_name());
    let mut watcher = attach_and_wait_screen(&rig, "s4", 24, 80);

    // `pty send --with-delay 0.2` is the CLI face of sendData({delayMs: 200}).
    let mut cmd = rig.command(&["send", "s4", "--with-delay", "0.2", "--seq", "A", "--seq", "B", "--seq", "C"]);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().expect("spawn pty send");

    let start = Instant::now();
    let mut timestamps: Vec<Instant> = Vec::new();
    let mut output = String::new();
    while start.elapsed() < Duration::from_millis(1000) {
        if let Some(p) = watcher.next_packet(Duration::from_millis(50))
            && p.type_ == MessageType::Data
        {
            timestamps.push(Instant::now());
            output.push_str(&String::from_utf8_lossy(&p.payload));
        }
    }
    let _ = child.wait();

    assert!(output.contains('A'), "{output:?}");
    assert!(output.contains('B'), "{output:?}");
    assert!(output.contains('C'), "{output:?}");
    assert!(timestamps.len() >= 2, "only {} DATA packets", timestamps.len());
    let first_gap = timestamps[1].duration_since(timestamps[0]);
    assert!(first_gap >= Duration::from_millis(100), "first gap {first_gap:?}");
}

/// node: tests/integration.test.ts:1796
#[test]
fn send_connection_to_non_existent_session_produces_error() {
    let rig = Rig::new();
    let path = rig.socket_path("never-started");
    assert!(Conn::try_open(&path).is_none(), "connected to {}", path.display());
}

// ── peek flags ──

/// node: tests/integration.test.ts:1812
#[test]
fn peek_with_plain_flag_returns_text_without_ansi_codes() {
    let rig = Rig::new();
    rig.daemon(
        "pk1",
        &["sh", "-c", "printf '\\033[32mGREEN TEXT\\033[0m'; printf '\\033[1;31mBOLD RED\\033[0m'; sleep 30"],
        DaemonOpts::no_display_name(),
    );
    std::thread::sleep(Duration::from_millis(200));

    let mut normal = rig.connect("pk1");
    normal.peek(false, false);
    let screen = normal.wait_for(MessageType::Screen, Duration::from_secs(5)).expect("SCREEN");
    let text = String::from_utf8_lossy(&screen.payload).into_owned();
    assert!(text.contains("GREEN TEXT"), "{text:?}");
    assert!(text.contains("\x1b["), "{text:?}");
    drop(normal);

    let mut plain = rig.connect("pk1");
    plain.peek(true, false);
    let screen = plain.wait_for(MessageType::Screen, Duration::from_secs(5)).expect("SCREEN");
    let text = String::from_utf8_lossy(&screen.payload).into_owned();
    assert!(text.contains("GREEN TEXT"), "{text:?}");
    assert!(text.contains("BOLD RED"), "{text:?}");
    assert!(!text.contains("\x1b["), "{text:?}");
}

/// node: tests/integration.test.ts:1843
#[test]
fn peek_plain_trims_trailing_blank_lines() {
    let rig = Rig::new();
    rig.daemon("pk2", &["sh", "-c", "echo 'only line'; sleep 30"], DaemonOpts::no_display_name());
    rig.wait_for_screen("pk2", "only line");
    let mut peeker = rig.connect("pk2");
    peeker.peek(true, false);
    let screen = peeker.wait_for(MessageType::Screen, Duration::from_secs(5)).expect("SCREEN");
    let text = String::from_utf8_lossy(&screen.payload).into_owned();
    assert!(text.contains("only line"), "{text:?}");
    assert!(text.split('\n').count() < 10, "{text:?}");
}

// ── restart ──

/// node: tests/integration.test.ts:1866
#[test]
fn restart_preserves_command_and_cwd_after_killing_a_running_session() {
    let rig = Rig::new();
    let cwd = rig.make_dir("restart-cwd");
    let opts = DaemonOpts {
        no_display_name: true,
        cwd: Some(cwd.clone()),
        ..Default::default()
    };
    rig.daemon("rs1", &["sh", "-c", "echo 'original'; sleep 60"], opts);
    rig.wait_for_screen("rs1", "original");

    let meta1 = rig.meta("rs1").expect("metadata");
    let cmd1 = meta1["command"].as_str().unwrap().to_string();
    assert!(cmd1 == "sh" || cmd1.ends_with("/sh"), "{cmd1}");
    assert_eq!(meta1["cwd"], json!(cwd.to_string_lossy()));

    let mut c = rig.connect("rs1");
    c.attach(24, 80);
    let screen = c.wait_for(MessageType::Screen, Duration::from_secs(5)).expect("SCREEN");
    assert!(String::from_utf8_lossy(&screen.payload).contains("original"));
    drop(c);

    // Kill the daemon (what cmdRestart does). Metadata survives; only the
    // socket/pid go.
    expect_status(&rig.pty(&["kill", "rs1"]), 0);
    let meta2 = rig.meta("rs1").expect("metadata after kill");
    assert_eq!(meta2["command"], meta1["command"]);
    assert_eq!(meta2["cwd"], json!(cwd.to_string_lossy()));
    assert!(!rig.socket_path("rs1").exists());

    // Relaunch on the same metadata.
    let out = rig.pty(&["run", "-a", "-d", "--id", "rs1", "--no-display-name", "--", "sh", "-c", "echo 'original'; sleep 60"]);
    expect_status(&out, 0);
    wait_until("rs1 socket", || rig.socket_path("rs1").exists());
    std::thread::sleep(Duration::from_millis(200));
    let mut c2 = rig.connect("rs1");
    c2.attach(24, 80);
    let screen2 = c2.wait_for(MessageType::Screen, Duration::from_secs(5)).expect("SCREEN after restart");
    assert!(String::from_utf8_lossy(&screen2.payload).contains("original"));
}

/// node: tests/integration.test.ts:1908
#[test]
fn metadata_persists_through_kill_for_restart() {
    let rig = Rig::new();
    let cwd = rig.make_dir("kill-cwd");
    let opts = DaemonOpts {
        no_display_name: true,
        cwd: Some(cwd.clone()),
        ..Default::default()
    };
    rig.daemon("rs2", &["cat", "-u"], opts);
    let meta = rig.meta("rs2").expect("metadata");
    let cmd = meta["command"].as_str().unwrap().to_string();
    assert!(cmd == "cat" || cmd.ends_with("/cat"), "{cmd}");
    assert_eq!(meta["args"], json!(["-u"]));
    assert_eq!(meta["cwd"], json!(cwd.to_string_lossy()));

    expect_status(&rig.pty(&["kill", "rs2"]), 0);

    let after = rig.meta("rs2").expect("metadata after kill");
    assert_eq!(after["command"], meta["command"]);
    assert_eq!(after["args"], json!(["-u"]));
    assert_eq!(after["cwd"], json!(cwd.to_string_lossy()));
}

// ── Terminal modes survive detach/reattach ──

fn mode_survives_reattach(idx: usize) {
    let (name, enable, printf) = TERMINAL_MODES[idx];
    let rig = Rig::new();
    let script = format!("printf '{printf}'; echo 'ready'; cat");
    rig.daemon("re", &["sh", "-c", &script], DaemonOpts::no_display_name());

    let mut c1 = rig.connect("re");
    c1.attach(24, 80);
    let _ = c1.drain(Duration::from_millis(300));
    c1.detach();
    std::thread::sleep(Duration::from_millis(200));

    let mut c2 = rig.connect("re");
    c2.attach(24, 80);
    let text = screen_and_data_text(&mut c2, Duration::from_millis(500));
    assert!(text.contains(enable), "{name}: {enable:?} missing from {text:?}");
}

/// node: tests/integration.test.ts:1941
#[test]
fn kitty_keyboard_mode_survives_detach_reattach() {
    mode_survives_reattach(0);
}

/// node: tests/integration.test.ts:1941
#[test]
fn sgr_mouse_mode_survives_detach_reattach() {
    mode_survives_reattach(1);
}

/// node: tests/integration.test.ts:1941
#[test]
fn cursor_hidden_mode_survives_detach_reattach() {
    mode_survives_reattach(2);
}

/// node: tests/integration.test.ts:1980
#[test]
fn send_style_data_socket_finishes_promptly_without_waiting_for_server_fin() {
    let rig = Rig::new();
    rig.daemon("fin", &["cat"], DaemonOpts::no_display_name());
    let start = Instant::now();
    let mut sender = rig.connect("fin");
    sender.write_raw(&encode_data(b"hello from send\n")).expect("DATA");
    sender.shutdown_write();
    let written = start.elapsed();
    assert!(written < Duration::from_secs(2), "write+end took {written:?}");
    // The daemon ends its side once the sender has; the socket must not
    // linger half-open.
    let _ = sender.collect_until_exit(Duration::from_secs(3));
    assert!(sender.is_eof(), "daemon never closed the command socket");
    assert!(start.elapsed() < Duration::from_secs(2), "took {:?}", start.elapsed());
}

// ── STATUS message ──

fn status_via_new_conn(rig: &Rig, id: &str) -> Value {
    let mut c = rig.connect(id);
    c.status_json(Duration::from_secs(5))
}

/// node: tests/integration.test.ts:2010
#[test]
fn status_responds_with_valid_json_containing_all_expected_fields() {
    let rig = Rig::new();
    rig.daemon("st1", &["sh", "-c", "echo hello; sleep 30"], DaemonOpts::no_display_name());
    std::thread::sleep(Duration::from_millis(300));
    let stats = status_via_new_conn(&rig, "st1");
    assert_eq!(stats["name"], "st1");
    assert_eq!(stats["terminal"]["cols"], 80);
    assert_eq!(stats["terminal"]["rows"], 24);
    assert!(stats["terminal"]["cursorX"].as_i64().unwrap() >= 0);
    assert!(stats["terminal"]["cursorY"].as_i64().unwrap() >= 0);
    assert!(stats["terminal"]["scrollbackUsed"].as_i64().unwrap() > 0, "{stats}");
    assert_eq!(stats["terminal"]["scrollbackCapacity"], 24 + 10000);
    assert_eq!(stats["process"]["alive"], true);
    assert!(stats["process"]["exitCode"].is_null(), "{stats}");
    assert!(stats["clients"]["total"].as_i64().unwrap() >= 0);
    assert!(!stats["modes"].is_null(), "{stats}");
    assert!(stats["uptimeSeconds"].as_i64().unwrap() >= 0, "{stats}");
}

/// node: tests/integration.test.ts:2037
#[test]
fn status_reports_correct_client_counts() {
    let rig = Rig::new();
    rig.daemon("st2", &["cat"], DaemonOpts::no_display_name());
    let _c1 = attach_and_wait_screen(&rig, "st2", 24, 80);
    let mut peeker = rig.connect("st2");
    peeker.peek(false, false);
    peeker.wait_for(MessageType::Screen, Duration::from_secs(5)).expect("peek SCREEN");
    let stats = status_via_new_conn(&rig, "st2");
    assert_eq!(stats["clients"]["total"], 2, "{stats}");
    assert_eq!(stats["clients"]["attached"], 1, "{stats}");
    assert_eq!(stats["clients"]["readOnly"], 1, "{stats}");
}

/// node: tests/integration.test.ts:2068
#[test]
fn status_ignores_resize_from_a_command_socket_until_it_attaches() {
    let rig = Rig::new();
    rig.daemon("st3", &["cat"], DaemonOpts::no_display_name());
    let mut attached = attach_and_wait_screen(&rig, "st3", 24, 80);

    let mut command = rig.connect("st3");
    let mut raw = encode_data(b"command-input-remains-valid\n");
    raw.extend_from_slice(&encode_resize(13, 37));
    raw.extend_from_slice(&encode_status());
    command.write_raw(&raw).unwrap();
    let status = command.wait_for(MessageType::Status, Duration::from_secs(3)).expect("STATUS on the command socket");
    wait_for_content(&mut attached, "command-input-remains-valid", Duration::from_secs(3));

    let stats: Value = serde_json::from_slice(&status.payload).unwrap();
    assert_eq!(stats["clients"]["total"], 1, "{stats}");
    assert_eq!(stats["clients"]["attached"], 1, "{stats}");
    assert_eq!(stats["clients"]["readOnly"], 0, "{stats}");
    assert_eq!(stats["terminal"]["rows"], 24, "{stats}");
    assert_eq!(stats["terminal"]["cols"], 80, "{stats}");
    let seq = command.sequence();
    assert!(!seq.contains(&MessageType::Geometry), "{:?}", sequence_names(&seq));
    assert!(!seq.contains(&MessageType::Screen), "{:?}", sequence_names(&seq));
}

fn assert_contains_connection(stats: &Value, expected: Value) {
    let conns = stats["clients"]["connections"].as_array().unwrap_or_else(|| panic!("no connections: {stats}"));
    assert!(conns.contains(&expected), "expected {expected} in {conns:?}");
}

/// node: tests/integration.test.ts:2120
#[test]
fn status_reports_anonymous_client_geometry_and_per_axis_constraints() {
    let rig = Rig::new();
    rig.daemon("st4", &["cat"], DaemonOpts::no_display_name());
    let _tall = attach_and_wait_screen(&rig, "st4", 50, 80);
    let _wide = attach_and_wait_screen(&rig, "st4", 30, 120);
    let mut peeker = rig.connect("st4");
    peeker.peek(false, false);
    peeker.wait_for(MessageType::Screen, Duration::from_secs(5)).expect("peek SCREEN");

    let stats = status_via_new_conn(&rig, "st4");
    assert_eq!(stats["terminal"]["rows"], 30, "{stats}");
    assert_eq!(stats["terminal"]["cols"], 80, "{stats}");
    assert_eq!(stats["clients"]["connections"].as_array().map(|a| a.len()), Some(3), "{stats}");
    assert_contains_connection(
        &stats,
        json!({"role": "writable", "rows": 50, "cols": 80, "lastRequestSequence": 1, "constrains": {"rows": false, "cols": true}}),
    );
    assert_contains_connection(
        &stats,
        json!({"role": "writable", "rows": 30, "cols": 120, "lastRequestSequence": 2, "constrains": {"rows": true, "cols": false}}),
    );
    assert_contains_connection(&stats, json!({"role": "readonly", "constrains": {"rows": false, "cols": false}}));
}

/// node: tests/integration.test.ts:2175
#[test]
fn status_relinquishes_a_writable_clients_geometry_constraints_when_it_peeks() {
    let rig = Rig::new();
    rig.daemon("st5", &["cat"], DaemonOpts::no_display_name());
    let mut smaller = attach_and_wait_screen(&rig, "st5", 30, 80);
    let mut larger = attach_and_wait_screen(&rig, "st5", 50, 120);

    smaller.peek(false, false);
    smaller.wait_for(MessageType::Screen, Duration::from_secs(5)).expect("SCREEN for the new peeker");
    let geometry = larger.wait_for(MessageType::Geometry, Duration::from_secs(5)).expect("GEOMETRY for the remaining writable");
    assert_eq!(u16::from_be_bytes([geometry.payload[0], geometry.payload[1]]), 50);
    assert_eq!(u16::from_be_bytes([geometry.payload[2], geometry.payload[3]]), 120);

    let stats = status_via_new_conn(&rig, "st5");
    assert_eq!(stats["terminal"]["rows"], 50, "{stats}");
    assert_eq!(stats["terminal"]["cols"], 120, "{stats}");
    assert_contains_connection(&stats, json!({"role": "readonly", "constrains": {"rows": false, "cols": false}}));
    assert_contains_connection(
        &stats,
        json!({"role": "writable", "rows": 50, "cols": 120, "lastRequestSequence": 2, "constrains": {"rows": true, "cols": true}}),
    );
}

/// node: tests/integration.test.ts:2222
#[test]
fn status_reports_exited_process() {
    let rig = Rig::new();
    // The daemon lingers 500 ms after the child exits; ask it on a socket
    // that is already open when the EXIT arrives.
    rig.daemon("st6", &["sh", "-c", "sleep 0.4; exit 7"], DaemonOpts::keep());
    let mut c = rig.connect("st6");
    c.attach(24, 80);
    let exit = c.wait_for(MessageType::Exit, Duration::from_secs(5)).expect("EXIT");
    assert_eq!(pty_core::protocol::decode_exit(&exit.payload), 7);
    let stats = c.status_json(Duration::from_secs(2));
    assert_eq!(stats["process"]["alive"], false, "{stats}");
    assert_eq!(stats["process"]["exitCode"], 7, "{stats}");
}

/// node: tests/integration.test.ts:2240
#[test]
fn status_reports_terminal_modes() {
    let rig = Rig::new();
    rig.daemon(
        "st7",
        &["sh", "-c", "printf '\\033[?1006h'; printf '\\033[?25l'; sleep 30"],
        DaemonOpts::no_display_name(),
    );
    std::thread::sleep(Duration::from_millis(500));
    let stats = status_via_new_conn(&rig, "st7");
    assert_eq!(stats["modes"]["sgrMouse"], true, "{stats}");
    assert_eq!(stats["modes"]["cursorHidden"], true, "{stats}");
}
