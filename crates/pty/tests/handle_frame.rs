//! Consumer-facing immutable frames, driven through the public socketpair connector.
//! Complete DATA packets are the applied batches: a transport may split their
//! bytes, but must not publish between the grid and mode changes inside one packet.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use pty::{AttachOptions, Frame, HandleEvent, TerminalHandle};
use pty_core::protocol::{MessageType, PacketReader, encode_data, encode_screen};
use pty_terminal::{CellSize, GraphicsOptions, PixelFormat, PlacementPosition};

// A deadlock bound, not a latency/performance assertion.
const TIMEOUT: Duration = Duration::from_secs(10);
const IMAGE_ID: u32 = 4242;
const OTHER_IMAGE_ID: u32 = 4243;
// The real PNG fixture from pty-terminal/tests/graphics.rs.
const PNG_16X8_BASE64: &str = concat!(
    "iVBORw0KGgoAAAANSUhEUgAAABAAAAAICAYAAADwdn+XAAAAqUlEQVR4nBXMwYBFIQAAwIewCCGEEEIIIYQQQggh",
    "hBBCCCFk8Hc6zHW+72u/PwKRRKZQaXQGk8XmcPm+ICAQSWQKlUZnMFlsDje8IAkIRBKZQqXRGUwWm8NNLygCApFE",
    "plBpdAaTxeZwywuagEAkkSlUGp3BZLE53PaCISAQSWQKlUZnMFlsDne8YAkIRBKZQqXRGUwWm8NdLzgCApFEplBp",
    "dAaTxeZw+Qf7mTOQwddl2QAAAABJRU5ErkJggg=="
);

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

fn attached(graphics: bool) -> (Arc<TerminalHandle>, UnixStream, mpsc::Receiver<HandleEvent>) {
    let (client, mut daemon) = UnixStream::pair().unwrap();
    daemon.set_read_timeout(Some(TIMEOUT)).unwrap();
    daemon.set_write_timeout(Some(TIMEOUT)).unwrap();
    let h = Arc::new(
        TerminalHandle::attach_with_connector(move || client.try_clone(), options(graphics))
            .expect("socketpair attach"),
    );
    read_attach(&mut daemon);
    let events = h.subscribe();
    let before = h.frame().rev;
    daemon.write_all(&encode_screen(b"\x1b[?1006lA")).unwrap();
    assert!(h.wait_ready(TIMEOUT));
    assert_batch(&h.frame());
    let rev = next_dirty(&events, before);
    assert_eq!(h.frame().rev, rev);
    (h, daemon, events)
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

fn transmit(id: u32) -> String {
    format!("\x1b_Ga=t,f=100,q=2,i={id};{PNG_16X8_BASE64}\x1b\\")
}

fn placement(id: u32) -> String {
    format!("\x1b_Ga=p,q=2,i={id},p=7,c=1,r=1\x1b\\")
}

fn assert_image_generations(frame: &Frame) {
    for desc in &frame.graphics.images {
        let pixels = frame.image_bytes(desc.id).expect("frame owns its image pixels");
        assert_eq!(pixels.desc, *desc);
        assert_eq!(pixels.data.len(), desc.len);
    }
    for placement in &frame.graphics.placements {
        let pixels = frame.image_bytes(placement.image_id).expect("placement image pixels");
        assert_eq!(pixels.desc.generation, placement.image_generation);
    }
}

#[test]
fn dirty_exposes_complete_frames_and_retained_frames_do_not_change() {
    let (h, mut daemon, events) = attached(false);
    let old = h.frame();
    assert_batch(&old);
    let old_grid = old.grid.clone();
    let old_modes = old.modes.clone();
    let old_graphics = old.graphics.clone();
    let changed = update(&h, &mut daemon, &events, b"\x1b[?1006h\x1b[HB");
    assert_batch(&changed);
    assert!(changed.rev > old.rev);
    assert!(!Arc::ptr_eq(&old, &changed));
    assert!(Arc::ptr_eq(&changed, &h.frame()), "reads reuse the published frame");
    assert_eq!(h.snapshot(0), changed.grid);
    assert_eq!(h.graphics(0), changed.graphics);
    let latest = update(&h, &mut daemon, &events, b"\x1b[?1006l\x1b[HA");
    assert_batch(&latest);
    assert!(latest.rev > changed.rev);
    assert_eq!(old.grid, old_grid);
    assert_eq!(old.modes, old_modes);
    assert_eq!(old.graphics, old_graphics);
    assert_batch(&changed);
    h.kill();
}

#[test]
fn concurrent_frame_reads_remain_consistent_under_continuous_output() {
    let (h, mut daemon, _) = attached(false);
    let stop = Arc::new(AtomicBool::new(false));
    let writer_stop = Arc::clone(&stop);
    let (writer_tx, writer_rx) = mpsc::channel();
    let readers: Vec<_> = (0..2)
        .map(|_| {
            let h = Arc::clone(&h);
            let events = h.subscribe();
            let (done_tx, done_rx) = mpsc::channel();
            let thread = std::thread::spawn(move || {
                let mut last_rev = h.frame().rev;
                let mut observed = 0;
                while observed < 64 {
                    let dirty = next_dirty(&events, last_rev);
                    for _ in 0..16 {
                        let frame = h.frame();
                        assert!(frame.rev >= dirty, "Dirty preceded publication");
                        assert!(frame.rev >= last_rev, "publication moved backwards");
                        assert_batch(&frame);
                        if frame.rev > last_rev {
                            observed += 1;
                            last_rev = frame.rev;
                        }
                    }
                }
                done_tx.send(()).unwrap();
            });
            (done_rx, thread)
        })
        .collect();
    let writer = std::thread::spawn(move || {
        let batches = [
            encode_data(b"\x1b[?1006h\x1b[HB"),
            encode_data(b"\x1b[?1006l\x1b[HA"),
        ];
        let result = (|| -> io::Result<()> {
            while !writer_stop.load(Ordering::Acquire) {
                for batch in &batches {
                    daemon.write_all(batch)?;
                }
            }
            Ok(())
        })();
        let _ = writer_tx.send(result);
    });
    // Always stop output before propagating a reader failure, including a
    // blocked actor-RPC implementation of frame().
    let results: Vec<_> = readers.iter().map(|(rx, _)| rx.recv_timeout(TIMEOUT)).collect();
    stop.store(true, Ordering::Release);
    let writer_result = writer_rx.recv_timeout(TIMEOUT);
    h.kill();
    for result in results {
        result.expect("frame reader must finish while output continues");
    }
    writer_result.expect("output writer stopped").expect("continuous output");
    writer.join().unwrap();
    for (_, reader) in readers {
        reader.join().unwrap();
    }
}

#[test]
fn published_reads_and_history_request_do_not_wait_for_a_blocked_actor() {
    let (client, mut daemon) = UnixStream::pair().unwrap();
    daemon.set_read_timeout(Some(TIMEOUT)).unwrap();
    daemon.set_write_timeout(Some(TIMEOUT)).unwrap();
    let attempts = AtomicUsize::new(0);
    let (blocked_tx, blocked_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let h = Arc::new(
        TerminalHandle::attach_with_connector(
            move || {
                if attempts.fetch_add(1, Ordering::Relaxed) == 0 {
                    return client.try_clone();
                }
                blocked_tx.send(()).map_err(io::Error::other)?;
                // Disconnecting release_tx on test failure also releases the actor.
                let _ = release_rx.recv();
                Err(io::Error::other("intentional reconnect failure"))
            },
            options(false),
        )
        .unwrap(),
    );
    read_attach(&mut daemon);
    let events = h.subscribe();
    let before = h.frame().rev;
    daemon.write_all(&encode_screen(b"L0\r\nL1\r\nL2\r\nL3\r\nL4\r\nL5")).unwrap();
    next_dirty(&events, before);
    let held = h.frame();
    assert_eq!(held.grid.base_y, 3);
    let (reconnected_tx, reconnected_rx) = mpsc::channel();
    let reconnect_handle = Arc::clone(&h);
    let reconnect = std::thread::spawn(move || {
        let _ = reconnected_tx.send(reconnect_handle.reconnect());
    });
    blocked_rx.recv_timeout(TIMEOUT).expect("actor entered connector");
    let (read_tx, read_rx) = mpsc::channel();
    let read_handle = Arc::clone(&h);
    let reader = std::thread::spawn(move || {
        let frame = read_handle.frame();
        let grid = read_handle.snapshot(0);
        let graphics = read_handle.graphics(0);
        let history = read_handle.request_frame(2);
        let _ = read_tx.send((frame, grid, graphics, history));
    });
    let reads = read_rx.recv_timeout(TIMEOUT);
    let history_was_pending = reads.as_ref().ok().map(|(_, _, _, history)| {
        matches!(history.try_recv(), Err(mpsc::TryRecvError::Empty))
    });
    // Release before any assertion that could fail, so a regression cannot
    // leave the connector thread permanently parked.
    release_tx.send(()).unwrap();
    let reconnected = reconnected_rx.recv_timeout(TIMEOUT);
    let (frame, grid, graphics, history) = reads.expect("published reads waited for the actor");
    assert!(history_was_pending.unwrap(), "history must be captured by the actor");
    assert!(Arc::ptr_eq(&frame, &held));
    assert_eq!(grid, held.grid);
    assert_eq!(graphics, held.graphics);
    assert!(reconnected.expect("reconnect returned").is_err());
    let history = history.recv_timeout(TIMEOUT).expect("queued history read completed");
    assert_eq!(history.rev, held.rev);
    assert_eq!(history.grid.start, 1);
    assert_eq!(history.grid.rows[0][0].text, "L");
    assert_eq!(history.grid.rows[0][1].text, "1");
    assert_eq!(history.modes, held.modes);
    h.kill();
    reader.join().unwrap();
    reconnect.join().unwrap();
}

#[test]
fn held_image_generations_survive_replacement_and_deletion_and_reuse_unchanged_pixels() {
    let (h, mut daemon, events) = attached(true);
    let initial = format!(
        "{}{}{}{}",
        transmit(IMAGE_ID),
        placement(IMAGE_ID),
        transmit(OTHER_IMAGE_ID),
        placement(OTHER_IMAGE_ID)
    );
    let old = update(&h, &mut daemon, &events, initial.as_bytes());
    assert_image_generations(&old);
    let old_pixels = Arc::clone(old.image_bytes(IMAGE_ID).unwrap());
    assert_eq!((old_pixels.desc.width, old_pixels.desc.height), (16, 8));
    assert_eq!(old_pixels.desc.format, PixelFormat::Rgba);
    let old_data = old_pixels.data.clone();
    let survivor = Arc::clone(old.image_bytes(OTHER_IMAGE_ID).unwrap());
    let text_only = update(&h, &mut daemon, &events, b"\x1b[Htext");
    assert!(Arc::ptr_eq(&old_pixels, text_only.image_bytes(IMAGE_ID).unwrap()));
    assert!(Arc::ptr_eq(&survivor, text_only.image_bytes(OTHER_IMAGE_ID).unwrap()));
    // The raw RGBA protocol form also exercised in graphics.rs: one red pixel.
    let replacement = format!(
        "\x1b_Ga=t,q=2,i={IMAGE_ID},f=32,s=1,v=1;/wAA/w==\x1b\\{}",
        placement(IMAGE_ID)
    );
    let replaced = update(&h, &mut daemon, &events, replacement.as_bytes());
    assert_image_generations(&replaced);
    let new_pixels = replaced.image_bytes(IMAGE_ID).unwrap();
    assert_eq!(new_pixels.data, [255, 0, 0, 255]);
    assert_eq!((new_pixels.desc.width, new_pixels.desc.height), (1, 1));
    assert_ne!(new_pixels.desc.generation, old_pixels.desc.generation);
    assert!(!Arc::ptr_eq(&old_pixels, new_pixels));
    assert!(Arc::ptr_eq(&survivor, replaced.image_bytes(OTHER_IMAGE_ID).unwrap()));
    // Same id, dimensions and byte length: only the generation fences pixels.
    let same_size = format!(
        "\x1b_Ga=t,q=2,i={IMAGE_ID},f=32,s=1,v=1;AAD//w==\x1b\\{}",
        placement(IMAGE_ID)
    );
    let blue = update(&h, &mut daemon, &events, same_size.as_bytes());
    assert_image_generations(&blue);
    let blue_pixels = blue.image_bytes(IMAGE_ID).unwrap();
    assert_eq!(blue_pixels.data, [0, 0, 255, 255]);
    assert_eq!((blue_pixels.desc.width, blue_pixels.desc.height), (1, 1));
    assert_ne!(blue_pixels.desc.generation, new_pixels.desc.generation);
    assert!(!Arc::ptr_eq(blue_pixels, new_pixels));
    assert!(Arc::ptr_eq(&survivor, blue.image_bytes(OTHER_IMAGE_ID).unwrap()));
    let delete = format!("\x1b_Ga=d,d=I,i={IMAGE_ID},q=2\x1b\\");
    let deleted = update(&h, &mut daemon, &events, delete.as_bytes());
    assert_image_generations(&deleted);
    assert!(deleted.graphics.image(IMAGE_ID).is_none());
    assert!(deleted.image_bytes(IMAGE_ID).is_none());
    assert!(h.image_bytes(IMAGE_ID).is_none());
    assert!(Arc::ptr_eq(&survivor, deleted.image_bytes(OTHER_IMAGE_ID).unwrap()));
    assert_eq!(old.image_bytes(IMAGE_ID).unwrap().data, old_data);
    assert_eq!(
        old.image_bytes(IMAGE_ID).unwrap().desc,
        *old.graphics.image(IMAGE_ID).unwrap()
    );
    assert_eq!(replaced.image_bytes(IMAGE_ID).unwrap().data, [255, 0, 0, 255]);
    assert_image_generations(&old);
    assert_image_generations(&replaced);
    assert_eq!(blue.image_bytes(IMAGE_ID).unwrap().data, [0, 0, 255, 255]);
    assert_image_generations(&blue);
    h.kill();
}

#[test]
fn explicit_history_frame_aligns_grid_graphics_modes_and_image_generations() {
    let (h, mut daemon, events) = attached(true);
    let bytes = format!(
        "\x1b[2J\x1b[H\x1b[?1006hL0\r\n{}{}\x1b[2;1HL1\r\nL2\r\nL3\r\nL4\r\nL5",
        transmit(IMAGE_ID),
        placement(IMAGE_ID)
    );
    let live = update(&h, &mut daemon, &events, bytes.as_bytes());
    assert_eq!(live.grid.base_y, 3);
    let history = h.request_frame(2).recv_timeout(TIMEOUT).expect("history frame");
    assert_eq!(history.rev, live.rev);
    assert_eq!(history.grid.start, 1);
    assert_eq!(history.grid.base_y, live.grid.base_y);
    assert_eq!(history.grid, h.snapshot(2));
    assert_eq!(history.graphics, h.graphics(2));
    assert_eq!(history.modes, live.modes);
    assert!(history.modes.sgr_mouse);
    let rows: Vec<_> = history.grid.rows.iter().map(|row| {
        row.iter().map(|cell| cell.text.as_str()).collect::<String>().trim_end().to_owned()
    }).collect();
    assert_eq!(rows, ["L1", "L2", "L3"]);
    let placement = history.graphics.placements.iter().find(|p| p.image_id == IMAGE_ID).unwrap();
    assert!(matches!(placement.position, PlacementPosition::Direct { row: 0, col: 0, .. }));
    assert_image_generations(&history);
    let next = update(&h, &mut daemon, &events, b"\x1b[?1006l\r\nL6");
    assert!(next.rev > history.rev);
    assert!(!next.modes.sgr_mouse);
    assert!(history.modes.sgr_mouse);
    assert_eq!(history.grid.rows[0][1].text, "1");
    assert_image_generations(&history);
    h.kill();
}
