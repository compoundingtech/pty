//! Consumer-facing immutable frames, driven through the public socketpair connector.
//! Complete DATA packets are the applied batches: a transport may split their
//! bytes, but must not publish between the grid and mode changes inside one packet.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use pty::{AttachOptions, Frame, FrameObserver, HandleEvent, TerminalHandle};
use pty_core::protocol::{MessageType, PacketReader, encode_data, encode_screen};
use pty_terminal::{CellSize, GraphicsOptions, Range};

// A deadlock bound, not a latency/performance assertion.
const TIMEOUT: Duration = Duration::from_secs(10);

fn options(graphics: bool) -> AttachOptions {
    AttachOptions {
        rows: 3,
        cols: 20,
        scrollback: 100,
        graphics: graphics.then_some(GraphicsOptions {
            cell: CellSize { width: 8, height: 16 },
            ..GraphicsOptions::DEFAULT
        }),
        ..Default::default()
    }
}

fn read_attach(daemon: &mut UnixStream) {
    let mut header = [0; 5];
    daemon.read_exact(&mut header).expect("ATTACH header");
    let length = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
    let mut bytes = header.to_vec();
    bytes.resize(5 + length, 0);
    daemon.read_exact(&mut bytes[5..]).expect("ATTACH payload");
    let packet = PacketReader::new().feed(&bytes).unwrap().remove(0);
    assert_eq!(packet.type_, MessageType::Attach);
}

fn next_dirty(events: &mpsc::Receiver<HandleEvent>, after: u64) -> u64 {
    loop {
        if let HandleEvent::Dirty(rev) = events.recv_timeout(TIMEOUT).expect("published Dirty")
            && rev > after
        {
            return rev;
        }
    }
}

fn unobserved(graphics: bool) -> (Arc<TerminalHandle>, UnixStream, mpsc::Receiver<HandleEvent>) {
    let (client, mut daemon) = UnixStream::pair().unwrap();
    daemon.set_read_timeout(Some(TIMEOUT)).unwrap();
    daemon.set_write_timeout(Some(TIMEOUT)).unwrap();
    let h = Arc::new(
        TerminalHandle::attach_with_connector(move || client.try_clone(), options(graphics))
            .expect("socketpair attach"),
    );
    read_attach(&mut daemon);
    let initial = h.frame();
    assert_eq!(initial.rev, 0);
    assert!(initial.grid.rows.is_empty());
    let events = h.subscribe();
    let before = h.rev();
    daemon.write_all(&encode_screen(b"\x1b[?1006lA")).unwrap();
    let rev = next_dirty(&events, before);
    assert!(h.wait_ready(TIMEOUT));
    // The cheap actor RPC is a barrier, not an implicit frame capture.
    assert_eq!(h.plain(Range::Viewport), "A");
    assert_eq!(h.rev(), rev);
    assert!(Arc::ptr_eq(&initial, &h.frame()));
    (h, daemon, events)
}

fn attached(
    graphics: bool,
) -> (Arc<TerminalHandle>, UnixStream, mpsc::Receiver<HandleEvent>, FrameObserver) {
    let (h, daemon, events) = unobserved(graphics);
    let before = h.frame().rev;
    let observer = h.observe_frames();
    let rev = next_dirty(&events, before);
    assert_eq!(h.frame().rev, rev);
    assert_batch(&h.frame());
    (h, daemon, events, observer)
}

fn update_unobserved(
    h: &TerminalHandle,
    daemon: &mut UnixStream,
    events: &mpsc::Receiver<HandleEvent>,
    bytes: &[u8],
) -> u64 {
    let before = h.rev();
    daemon.write_all(&encode_data(bytes)).unwrap();
    let rev = next_dirty(events, before);
    // Wait until the actor has applied the output without forcing a capture.
    let _ = h.plain(Range::Viewport);
    assert_eq!(h.rev(), rev);
    rev
}

fn update(
    h: &TerminalHandle,
    daemon: &mut UnixStream,
    events: &mpsc::Receiver<HandleEvent>,
    bytes: &[u8],
) -> Arc<Frame> {
    let before = h.frame().rev;
    daemon.write_all(&encode_data(bytes)).unwrap();
    let dirty = next_dirty(events, before);
    let frame = h.frame();
    // There is no subsequent output in these staged tests. Dirty must expose
    // exactly the frame it announces, not the previous publication.
    assert_eq!(frame.rev, dirty, "Dirty arrived before its frame was stored");
    frame
}

fn assert_batch(frame: &Frame) {
    let label = frame.grid.rows[0][0].text.as_str();
    assert_eq!(label, if frame.modes.sgr_mouse { "B" } else { "A" });
    assert_eq!(frame.grid.cursor, (0, 1, true));
    assert_eq!((frame.grid.rows_n, frame.grid.cols), (3, 20));
}

#[test]
fn unobserved_writes_update_state_but_leave_the_frame_stale() {
    let (h, mut daemon, events) = unobserved(false);
    let held = h.frame();
    let rev = update_unobserved(&h, &mut daemon, &events, b"\x1b[?1006h\x1b[HB");
    assert!(rev > held.rev);
    assert!(h.modes().sgr_mouse);
    assert_eq!(h.cursor(), (0, 1, true));
    assert_eq!((h.rows(), h.cols()), (3, 20));
    assert_eq!(h.plain(Range::Viewport), "B");
    // Legacy reads remain explicit actor RPCs, not reads of the stale frame.
    assert_eq!(h.snapshot(0).rows[0][0].text, "B");
    assert!(Arc::ptr_eq(&held, &h.frame()));
    h.kill();
}

#[test]
fn acquiring_first_observer_wakes_idle_actor_without_new_output() {
    let (h, _daemon, events) = unobserved(false);
    let before = h.frame();
    let applied = h.rev();
    let observer = h.observe_frames();
    let dirty = next_dirty(&events, before.rev);
    let frame = h.frame();
    assert_eq!(dirty, applied);
    assert_eq!(frame.rev, applied);
    assert!(!Arc::ptr_eq(&before, &frame));
    assert_batch(&frame);
    // Observation captures the existing revision; it does not apply output.
    assert_eq!(h.rev(), applied);
    drop(observer);
    h.kill();
}

#[test]
fn dropping_last_observer_stops_capture_even_with_a_live_subscription() {
    let (h, mut daemon, events, observer) = attached(false);
    let held = update(&h, &mut daemon, &events, b"\x1b[?1006h\x1b[HB");
    drop(observer);
    let rev = update_unobserved(&h, &mut daemon, &events, b"\x1b[?1006l\x1b[HA");
    assert!(rev > held.rev);
    assert!(!h.modes().sgr_mouse);
    assert!(held.modes.sgr_mouse);
    assert!(Arc::ptr_eq(&held, &h.frame()));
    let before = held.rev;
    let _observer = h.observe_frames();
    assert_eq!(next_dirty(&events, before), rev);
    assert_eq!(h.frame().rev, rev);
    assert_batch(&h.frame());
    h.kill();
}

#[test]
fn multiple_observers_keep_capturing_until_the_last_guard_drops() {
    let (h, mut daemon, events, first) = attached(false);
    let second = h.observe_frames();
    drop(first);
    let held = update(&h, &mut daemon, &events, b"\x1b[?1006h\x1b[HB");
    assert_batch(&held);
    drop(second);
    let rev = update_unobserved(&h, &mut daemon, &events, b"\x1b[?1006l\x1b[HA");
    assert!(rev > held.rev);
    assert!(!h.modes().sgr_mouse);
    assert!(Arc::ptr_eq(&held, &h.frame()));
    h.kill();
}

#[test]
fn live_frame_request_forces_capture_without_an_observer() {
    let (h, mut daemon, events) = unobserved(false);
    let stale = h.frame();
    let rev = update_unobserved(&h, &mut daemon, &events, b"\x1b[?1006h\x1b[HB");
    assert!(Arc::ptr_eq(&stale, &h.frame()));
    let frame = h.request_frame(0).recv_timeout(TIMEOUT).expect("explicit live capture");
    assert_eq!(frame.rev, rev);
    assert_batch(&frame);
    assert!(frame.modes.sgr_mouse);
    assert!(Arc::ptr_eq(&frame, &h.frame()));
    assert_eq!(next_dirty(&events, stale.rev), rev);
    // An explicit request does not leave automatic observation enabled.
    let next = update_unobserved(&h, &mut daemon, &events, b"\x1b[?1006l\x1b[HA");
    assert!(next > frame.rev);
    assert!(Arc::ptr_eq(&frame, &h.frame()));
    h.kill();
}
