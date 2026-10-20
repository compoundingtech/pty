//! Enhanced paste end to end through a real daemon: a program in a session
//! turns paste events on, a handle sends it a paste event listing a real
//! PNG, the program reads the PNG, and the handle answers with its bytes.
//! The program hashes what it received.
//!
//! The program is this test binary run again, as the ignored test
//! `enhanced_paste_program`, so the test needs no other tool.

mod daemon_support;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use daemon_support::*;
use pty::{
    AttachOptions, HandleEvent, Location, PastePacket, PasteTarget, SessionRef, TerminalHandle,
};
use sha2::{Digest, Sha256};

const T: Duration = Duration::from_secs(10);

/// A 1x1 RGBA PNG: IHDR, IDAT and IEND, with valid checksums.
const PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

/// The paste event's one-time password, base64.
const PASSWORD: &str = "dG9rZW4=";

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(data: &[u8]) -> String {
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = chunk.iter().fold(0u32, |n, &b| n << 8 | u32::from(b)) << (8 * (3 - chunk.len()));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn unbase64(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in text.chunks(4) {
        let mut n = 0u32;
        let mut bytes = 3;
        for (i, &c) in chunk.iter().enumerate() {
            let value = match ALPHABET.iter().position(|&a| a == c) {
                Some(v) => v as u32,
                None if c == b'=' => {
                    bytes = bytes.min(i - 1);
                    0
                }
                None => panic!("not base64: {c}"),
            };
            n = n << 6 | value;
        }
        out.extend_from_slice(&n.to_be_bytes()[1..1 + bytes]);
    }
    out
}

fn png() -> Vec<u8> {
    unbase64(PNG_BASE64.as_bytes())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// OSC 5522 sequences read from the program's terminal, as
/// `(metadata, payload)`.
struct Packets<R> {
    input: R,
    buf: Vec<u8>,
}

impl<R: Read> Packets<R> {
    fn next(&mut self) -> (String, Vec<u8>) {
        loop {
            if let Some(start) = find(&self.buf, b"\x1b]5522;")
                && let Some(len) = find(&self.buf[start..], b"\x1b\\")
            {
                let body = self.buf[start + 7..start + len].to_vec();
                self.buf.drain(..start + len + 2);
                let (metadata, payload) = match body.iter().position(|&b| b == b';') {
                    Some(i) => (&body[..i], body[i + 1..].to_vec()),
                    None => (&body[..], Vec::new()),
                };
                return (String::from_utf8(metadata.to_vec()).unwrap(), payload);
            }
            let mut chunk = [0; 4096];
            let n = self.input.read(&mut chunk).expect("read the terminal");
            assert!(n > 0, "the terminal closed");
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// The data of a read's answer, or of a paste event's listing: every
    /// DATA payload up to DONE.
    fn data_until_done(&mut self) -> Vec<u8> {
        let mut data = Vec::new();
        loop {
            let (metadata, payload) = self.next();
            if metadata.contains("status=DONE") {
                return data;
            }
            assert!(metadata.contains("status=DATA"), "{metadata}");
            data.extend(unbase64(&payload));
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The program: paste events on, then wait for a paste event, read the PNG
/// it lists, and record the listing and the PNG's hash in the fixture
/// directory.
#[test]
#[ignore = "run by the end-to-end test, as the program in its session"]
fn enhanced_paste_program() {
    let Some(dir) = std::env::var_os("PTY_PASTE_FIXTURE") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let mut out = std::io::stdout().lock();
    out.write_all(b"\x1b[?5522hREADY\r\n").unwrap();
    out.flush().unwrap();
    let mut packets = Packets {
        input: std::io::stdin().lock(),
        buf: Vec::new(),
    };
    let (metadata, _) = packets.next();
    assert!(metadata.contains("status=OK"), "{metadata}");
    let password = metadata
        .split(':')
        .find_map(|pair| pair.strip_prefix("pw="))
        .expect("a password")
        .to_string();
    let listing = String::from_utf8(packets.data_until_done()).unwrap();
    std::fs::write(dir.join("listing"), &listing).unwrap();
    assert!(
        listing.split_whitespace().any(|mime| mime == "image/png"),
        "{listing}"
    );
    write!(
        out,
        "\x1b]5522;type=read:id=e2e:pw={password};{}\x1b\\",
        base64(b"image/png")
    )
    .unwrap();
    out.flush().unwrap();
    let (metadata, _) = packets.next();
    assert_eq!(metadata, "type=read:status=OK:id=e2e");
    let png = packets.data_until_done();
    std::fs::write(dir.join("hash"), hex(&Sha256::digest(&png))).unwrap();
}

fn wait_file(path: &Path) -> String {
    let deadline = Instant::now() + T;
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.is_empty()
        {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "{} never written",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_png_pasted_through_the_handle_reaches_the_program_whole() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let fixture = root.join("fixture");
    std::fs::create_dir(&fixture).unwrap();
    let exe = std::env::current_exe().unwrap();
    let exe = exe.to_str().unwrap();
    let script = "stty raw -echo; exec \"$0\" --ignored --exact enhanced_paste_program --nocapture";
    let name = unique_name("paste");
    let _d = Daemon::start_env(
        &root,
        config(&name, "sh", &["-c", script, exe]),
        &[("PTY_PASTE_FIXTURE", fixture.to_str().unwrap())],
    );
    let h = TerminalHandle::attach(
        SessionRef {
            root: root.clone(),
            id: name,
        },
        AttachOptions::default(),
    )
    .expect("attach");
    let events = h.subscribe();
    let deadline = Instant::now() + T;
    while !h.enhanced_paste().1 {
        assert!(
            Instant::now() < deadline,
            "the program never turned paste events on"
        );
        h.wait_rev(h.rev(), Duration::from_millis(100));
    }
    let attempt = h.attempt();

    h.write_enhanced_paste(
        attempt,
        PasteTarget::Paste,
        PastePacket::Listing {
            location: Location::Clipboard,
            password: Some(PASSWORD.into()),
            mimes: vec!["image/png".into(), "text/plain".into()],
        },
    )
    .expect("the paste event");
    let read = loop {
        match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(HandleEvent::EnhancedPasteRequest {
                attempt: read_attempt,
                read,
                request,
            }) => {
                assert_eq!(read_attempt, attempt);
                assert_eq!(request.mimes, ["image/png"]);
                assert_eq!(request.password.as_deref(), Some(PASSWORD));
                assert_eq!(request.id.as_deref(), Some("e2e"));
                break read;
            }
            Ok(_) => {}
            Err(e) => panic!("the program's read was never published: {e}"),
        }
    };
    for packet in [
        PastePacket::ReadOk,
        PastePacket::ReadData {
            mime: "image/png".into(),
            data: png(),
        },
        PastePacket::ReadDone,
    ] {
        h.write_enhanced_paste(attempt, PasteTarget::Read(read), packet)
            .expect("the answer");
    }
    assert_eq!(wait_file(&fixture.join("listing")), "image/png text/plain");
    assert_eq!(
        wait_file(&fixture.join("hash")),
        hex(&Sha256::digest(png()))
    );
    h.kill();
}
