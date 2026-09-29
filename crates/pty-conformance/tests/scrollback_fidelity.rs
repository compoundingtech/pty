//! Port of tests/scrollback-fidelity.test.ts: what the daemon replays to a
//! client that reconnects (the ATTACH SCREEN payload) keeps the whole
//! scrollback, 24-bit colours, output produced while nobody was attached,
//! wide and combining characters, alt-screen content and cursor placement,
//! across resizes.
//!
//! Node drives its in-process testing Session (xterm fed by the socket) and
//! inspects the buffer. Here a raw connection attaches, the SCREEN/DATA
//! payload text stands in for the buffer text, `pty peek --plain` stands in
//! for the visible rows of the active buffer, and the Node scripts become
//! `sh` loops with the same output.

use pty_conformance::*;
use pty_core::protocol::{MessageType, Packet};
use std::time::{Duration, Instant};

fn text_of(packets: &[Packet]) -> String {
    let mut s = String::new();
    for p in packets {
        if matches!(p.type_, MessageType::Screen | MessageType::Data) {
            s.push_str(&String::from_utf8_lossy(&p.payload));
        }
    }
    s
}

/// A client attached at `rows`×`cols` that accumulates what it receives.
struct Client {
    conn: Conn,
    packets: Vec<Packet>,
}

impl Client {
    fn attach(rig: &Rig, id: &str, rows: u16, cols: u16) -> Client {
        let mut conn = rig.connect(id);
        conn.attach(rows, cols);
        Client { conn, packets: Vec::new() }
    }

    /// Read until the accumulated text contains `needle`; returns the text.
    ///
    /// **The budget has to cover the child script, not just the daemon.** The
    /// scripts here print a line and run an external `sleep` a hundred or two
    /// hundred times, which takes at least 10 s for the ticks, and each
    /// `sleep` is a fork and exec that slows down on a loaded machine. With
    /// `deadline() * 2` (20 s), a Mac at load average ~34 on 2026-09-28 ran
    /// out of time with the ticks script at about tick 89 of 100 (984 bytes)
    /// and the rapid script at about line 190 of 200 (2085 bytes). Both were
    /// still printing. What these tests check does not depend on how fast the
    /// script runs, so the budget is generous.
    fn wait_for_text(&mut self, needle: &str) -> String {
        let start = Instant::now();
        let timeout = deadline() * 6;
        loop {
            let text = text_of(&self.packets);
            if text.contains(needle) {
                return text;
            }
            let remaining = timeout.saturating_sub(start.elapsed());
            match self.conn.next_packet(remaining) {
                Some(p) => self.packets.push(p),
                None => panic!("timed out waiting for {needle:?}; saw {} bytes", text.len()),
            }
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.conn.resize(rows, cols);
    }

    fn status(&mut self) -> serde_json::Value {
        self.conn.status_json(deadline())
    }

    /// Drop the socket without a DETACH (a network-style disconnect).
    fn drop_socket(self) {}
}

/// `ansiContainsSGR`: `params` appears inside some CSI…m, possibly merged
/// with other parameters.
fn ansi_contains_sgr(ansi: &str, params: &str) -> bool {
    let re = regex::Regex::new(&format!(r"\x1b\[(?:[0-9;]*;)?{}(?:;[0-9;]*)?m", regex::escape(params))).unwrap();
    re.is_match(ansi)
}

fn plain_peek(rig: &Rig, id: &str) -> String {
    let out = rig.pty(&["peek", "--plain", id]);
    expect_status(&out, 0);
    out.stdout()
}

/// node: tests/scrollback-fidelity.test.ts:107
#[test]
fn large_mixed_scrollback_survives_reconnect() {
    let rig = Rig::new();
    let script = "i=0; while [ $i -lt 600 ]; do if [ $((i % 2)) -eq 0 ]; then printf 'LINE-%04d\\n' $i; else printf '\\033[31mLINE-%04d\\033[0m\\n' $i; fi; i=$((i + 1)); done; sleep 300";
    rig.daemon("big", &["sh", "-c", script], DaemonOpts::no_display_name());
    let mut c = Client::attach(&rig, "big", 24, 80);
    let text = c.wait_for_text("LINE-0599");
    expect_contains(&text, "LINE-0000");
    expect_contains(&text, "LINE-0599");
    c.drop_socket();

    let mut c2 = Client::attach(&rig, "big", 24, 80);
    let text = c2.wait_for_text("LINE-0599");
    expect_contains(&text, "LINE-0000");
    expect_contains(&text, "LINE-0599");
    expect_contains(&text, "LINE-0300");
}

/// node: tests/scrollback-fidelity.test.ts:156
#[test]
fn twenty_four_bit_colors_survive_reconnect() {
    let rig = Rig::new();
    let script = "i=0; while [ $i -lt 40 ]; do \
        printf '\\033[38;2;255;0;0mRED-24BIT\\033[0m\\n'; \
        printf '\\033[38;2;0;255;0mGREEN-24BIT\\033[0m\\n'; \
        printf '\\033[38;2;0;0;255mBLUE-24BIT\\033[0m\\n'; \
        printf '\\033[38;2;128;64;32mBROWN-24BIT\\033[0m\\n'; \
        printf '\\033[38;2;255;128;255mPINK-24BIT\\033[0m\\n'; \
        i=$((i + 1)); done; echo COLOR-DONE; sleep 300";
    rig.daemon("col", &["sh", "-c", script], DaemonOpts::no_display_name());
    let mut c = Client::attach(&rig, "col", 24, 80);
    c.wait_for_text("COLOR-DONE");
    c.drop_socket();

    let mut c2 = Client::attach(&rig, "col", 24, 80);
    let ansi = c2.wait_for_text("COLOR-DONE");
    for label in ["RED-24BIT", "GREEN-24BIT", "BLUE-24BIT", "BROWN-24BIT", "PINK-24BIT"] {
        expect_contains(&ansi, label);
    }
    for params in ["38;2;255;0;0", "38;2;0;255;0", "38;2;0;0;255", "38;2;128;64;32", "38;2;255;128;255"] {
        assert!(ansi_contains_sgr(&ansi, params), "SGR {params} missing from replay");
    }
}

/// node: tests/scrollback-fidelity.test.ts:214
#[test]
fn output_while_disconnected_is_present_after_reconnect() {
    let rig = Rig::new();
    let script = "c=0; while [ $c -lt 100 ]; do printf 'TICK-%04d\\n' $c; c=$((c + 1)); sleep 0.1; done; echo TICKS-DONE; sleep 300";
    rig.daemon("tick", &["sh", "-c", script], DaemonOpts::no_display_name());
    let mut c = Client::attach(&rig, "tick", 24, 80);
    c.wait_for_text("TICK-0005");
    c.drop_socket();
    std::thread::sleep(Duration::from_millis(500));

    let mut c2 = Client::attach(&rig, "tick", 24, 80);
    let text = c2.wait_for_text("TICKS-DONE");
    expect_contains(&text, "TICK-0005");
    expect_contains(&text, "TICK-0010");
}

/// node: tests/scrollback-fidelity.test.ts:271
#[test]
fn resize_during_rapid_output_does_not_corrupt_content() {
    let rig = Rig::new();
    let script = "i=0; while [ $i -lt 200 ]; do printf 'NUM-%05d\\n' $i; i=$((i + 1)); sleep 0.01; done; echo RAPID-DONE; sleep 300";
    rig.daemon("rapid", &["sh", "-c", script], DaemonOpts::no_display_name());
    let mut c = Client::attach(&rig, "rapid", 24, 80);
    c.wait_for_text("NUM-00010");
    c.resize(30, 120);
    let text = c.wait_for_text("RAPID-DONE");
    let st = c.status();
    assert_eq!(st["terminal"]["cols"], 120, "{st}");
    assert_eq!(st["terminal"]["rows"], 30, "{st}");
    expect_contains(&text, "NUM-00001");
    expect_contains(&text, "RAPID-DONE");

    let full = rig.pty(&["peek", "--plain", "--full", "rapid"]);
    expect_status(&full, 0);
    let re = regex::Regex::new(r"^NUM-\d{5}$").unwrap();
    let mut num_lines = 0;
    for line in full.stdout().lines().filter(|l| l.starts_with("NUM-")) {
        assert!(re.is_match(line), "corrupted line {line:?}");
        num_lines += 1;
    }
    assert!(num_lines > 0, "no NUM- lines in {}", full.stdout());
}

/// node: tests/scrollback-fidelity.test.ts:328
#[test]
fn alt_screen_and_normal_scrollback_both_survive_reconnect() {
    let rig = Rig::new();
    let script = "i=0; while [ $i -lt 50 ]; do printf 'NORMAL-LINE-%03d\\n' $i; i=$((i + 1)); done; \
        printf '\\033[?1049h\\033[H\\033[2J'; echo ALT-SCREEN-MARKER; echo ALT-CONTENT-ROW2; sleep 300";
    rig.daemon("alt", &["sh", "-c", script], DaemonOpts::no_display_name());
    let mut c = Client::attach(&rig, "alt", 24, 80);
    c.wait_for_text("ALT-SCREEN-MARKER");
    c.drop_socket();

    let mut c2 = Client::attach(&rig, "alt", 24, 80);
    let replay = c2.wait_for_text("ALT-SCREEN-MARKER");
    expect_contains(&replay, "ALT-CONTENT-ROW2");
    // The replay carries the normal buffer too (so leaving the alt screen
    // restores it), but the visible screen after replay is the alt screen.
    expect_contains(&replay, "NORMAL-LINE-000");
    let alt_at = replay.rfind("\x1b[?1049h").expect("alt-screen switch in replay");
    assert!(replay[alt_at..].contains("ALT-SCREEN-MARKER"));
    assert!(!replay[alt_at..].contains("NORMAL-LINE-000"), "normal buffer content after the alt switch");
    let visible = plain_peek(&rig, "alt");
    expect_contains(&visible, "ALT-SCREEN-MARKER");
    expect_contains(&visible, "ALT-CONTENT-ROW2");
    expect_not_contains(&visible, "NORMAL-LINE-000");
}

/// node: tests/scrollback-fidelity.test.ts:378
#[test]
fn unicode_and_wide_characters_survive_reconnect() {
    let rig = Rig::new();
    let script = "printf 'CJK:世界你好\\n'; printf 'EMOJI:😀🚀❤\\n'; printf 'BOX:┌──┐\\n'; printf 'BOX:│  │\\n'; printf 'BOX:└──┘\\n'; printf 'COMBINING:e\\314\\201 o\\314\\210\\n'; echo UNICODE-DONE; sleep 300";
    rig.daemon("uni", &["sh", "-c", script], DaemonOpts::no_display_name());
    let mut c = Client::attach(&rig, "uni", 24, 80);
    let text = c.wait_for_text("UNICODE-DONE");
    for needle in ["CJK:", "EMOJI:", "BOX:", "COMBINING:"] {
        expect_contains(&text, needle);
    }
    c.drop_socket();

    let mut c2 = Client::attach(&rig, "uni", 24, 80);
    let text = c2.wait_for_text("UNICODE-DONE");
    expect_contains(&text, "世界你好");
    expect_contains(&text, "EMOJI:");
    expect_contains(&text, "┌──┐");
    expect_contains(&text, "└──┘");
    expect_contains(&text, "COMBINING:");
    // The visible rows keep the combined forms.
    let visible = plain_peek(&rig, "uni");
    expect_contains(&visible, "COMBINING:e\u{301} o\u{308}");
}

/// node: tests/scrollback-fidelity.test.ts:435
#[test]
fn cursor_position_set_via_cup_is_restored_after_reconnect() {
    let rig = Rig::new();
    let script = "printf '\\033[?1049h\\033[2J\\033[10;20HCURSOR-HERE\\033[15;5H'; sleep 300";
    rig.daemon("cur", &["sh", "-c", script], DaemonOpts::no_display_name());
    let mut c = Client::attach(&rig, "cur", 24, 80);
    c.wait_for_text("CURSOR-HERE");
    let visible = plain_peek(&rig, "cur");
    let lines: Vec<&str> = visible.lines().collect();
    assert!(lines.len() >= 10, "{visible:?}");
    assert_eq!(lines[9].find("CURSOR-HERE"), Some(19), "{visible:?}");
    c.drop_socket();

    let mut c2 = Client::attach(&rig, "cur", 24, 80);
    let replay = c2.wait_for_text("CURSOR-HERE");
    let visible = plain_peek(&rig, "cur");
    let lines: Vec<&str> = visible.lines().collect();
    assert!(lines.len() >= 10, "{visible:?}");
    assert_eq!(lines[9].find("CURSOR-HERE"), Some(19), "{visible:?}");
    // The replay ends with the cursor at the child's resting position
    // (row 15, col 5) — serialized as relative moves after the marker.
    let st = c2.status();
    assert_eq!(st["terminal"]["cursorY"], 14, "{st}");
    assert_eq!(st["terminal"]["cursorX"], 4, "{st}");
    let after_marker = &replay[replay.rfind("CURSOR-HERE").unwrap()..];
    assert!(
        after_marker.contains("\x1b[5B") || after_marker.contains("\x1b[15;5H"),
        "replay must move the cursor to its resting row: {replay:?}"
    );
}

/// node: tests/scrollback-fidelity.test.ts:487
#[test]
fn scrollback_survives_resize_and_reconnect_cycles() {
    let rig = Rig::new();
    let script = "for i in $(seq -w 1 60); do echo \"SCROLL-LINE-$i\"; done; sleep 300";
    rig.daemon("cyc", &["sh", "-c", script], DaemonOpts::no_display_name());
    let mut c = Client::attach(&rig, "cyc", 24, 80);
    let text = c.wait_for_text("SCROLL-LINE-60");
    expect_contains(&text, "SCROLL-LINE-01");

    c.resize(10, 40);
    std::thread::sleep(Duration::from_millis(200));
    c.drop_socket();
    let mut c2 = Client::attach(&rig, "cyc", 10, 40);
    let text = c2.wait_for_text("SCROLL-LINE-60");
    expect_contains(&text, "SCROLL-LINE-01");
    assert_eq!(c2.status()["terminal"]["cols"], 40);

    c2.resize(40, 132);
    std::thread::sleep(Duration::from_millis(200));
    c2.drop_socket();
    let mut c3 = Client::attach(&rig, "cyc", 40, 132);
    let text = c3.wait_for_text("SCROLL-LINE-60");
    expect_contains(&text, "SCROLL-LINE-01");
    expect_contains(&text, "SCROLL-LINE-30");
    expect_contains(&text, "SCROLL-LINE-60");
    assert_eq!(c3.status()["terminal"]["cols"], 132);
}
