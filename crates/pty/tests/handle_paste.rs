//! Paste events through a handle attached over a socket pair that stands in
//! for the daemon: waiting on readiness never costs one, and a closed handle
//! refuses a paste write.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use pty::{
    AttachOptions, AttemptId, HandleEvent, Location, PastePacket, PasteRefusal, PasteTarget,
    TerminalHandle,
};
use pty_core::protocol::{MessageType, PacketReader, encode_data, encode_screen};

const T: Duration = Duration::from_secs(10);

/// `image/png text/plain`, base64.
const PNG_AND_TEXT: &str = "aW1hZ2UvcG5nIHRleHQvcGxhaW4=";

/// A handle and the daemon's end of its stream, past the ATTACH.
fn attached() -> (TerminalHandle, UnixStream) {
    let (client, mut daemon) = UnixStream::pair().unwrap();
    daemon.set_read_timeout(Some(T)).unwrap();
    let h =
        TerminalHandle::attach_with_connector(move || client.try_clone(), AttachOptions::default())
            .unwrap();
    let mut header = [0; 5];
    daemon.read_exact(&mut header).expect("ATTACH header");
    let length = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
    let mut bytes = header.to_vec();
    bytes.resize(5 + length, 0);
    daemon.read_exact(&mut bytes[5..]).expect("ATTACH payload");
    let packet = PacketReader::new().feed(&bytes).unwrap().remove(0);
    assert_eq!(packet.type_, MessageType::Attach);
    (h, daemon)
}

/// A consumer that waits on readiness, revisions and frames between
/// packets still hears every paste event, in order: each packet's mode on,
/// its read and its mode off, none merged into another.
#[test]
fn waiting_on_readiness_never_drops_or_merges_a_paste_event() {
    let (h, mut daemon) = attached();
    let events = h.subscribe();
    let _observer = h.observe_frames();
    daemon.write_all(&encode_screen(b"ready")).unwrap();
    assert!(h.wait_ready(T));
    for i in 0..20 {
        let bytes =
            format!("\x1b[?5522h\x1b]5522;type=read:id=r{i};{PNG_AND_TEXT}\x1b\\{i}\x1b[?5522l");
        daemon.write_all(&encode_data(bytes.as_bytes())).unwrap();
        let rev = h.rev();
        h.wait_rev(rev, Duration::from_millis(20));
        let _ = h.request_frame(0).recv_timeout(T);
        let _ = h.frame();
        assert!(h.wait_ready(T));
    }
    let expected: Vec<String> = (0..20)
        .flat_map(|i| ["on".to_string(), format!("read r{i}"), "off".to_string()])
        .collect();
    let mut seen = Vec::new();
    let deadline = Instant::now() + T;
    while seen.len() < expected.len() {
        let event = events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|e| panic!("{e} after {seen:?}"));
        match event {
            HandleEvent::EnhancedPasteMode { attempt, enabled } => {
                assert_eq!(attempt, AttemptId(1));
                seen.push(if enabled { "on" } else { "off" }.to_string());
            }
            HandleEvent::EnhancedPasteRequest {
                attempt, request, ..
            } => {
                assert_eq!(attempt, AttemptId(1));
                seen.push(format!("read {}", request.id.unwrap_or_default()));
            }
            _ => {}
        }
    }
    assert_eq!(seen, expected);
}

/// Once the handle is closed, a paste write is refused as closed.
#[test]
fn a_closed_handle_refuses_a_paste_write() {
    let (h, mut daemon) = attached();
    daemon.write_all(&encode_screen(b"\x1b[?5522h")).unwrap();
    assert!(h.wait_ready(T));
    assert_eq!(h.enhanced_paste(), (AttemptId(1), true));
    h.kill();
    let listing = PastePacket::Listing {
        location: Location::Clipboard,
        password: None,
        mimes: vec!["image/png".into()],
    };
    assert_eq!(
        h.write_enhanced_paste(AttemptId(1), PasteTarget::Paste, listing),
        Err(PasteRefusal::Closed)
    );
}
