//! Enhanced paste through the handle: attempt-tagged events, the checked
//! write, and a read's answer held until it is whole.

use super::*;
use pty_core::protocol::encode_screen;
use pty_terminal::enhanced_paste::{Location, MAX_ANSWER, MAX_ID, MAX_PACKET};

/// `image/png text/plain`, base64.
const PNG_AND_TEXT: &str = "aW1hZ2UvcG5nIHRleHQvcGxhaW4=";

struct Rig {
    core: Core,
    events: Receiver<HandleEvent>,
    /// The packets the daemon's end received, read on their own thread so
    /// a large write never waits on this one.
    sent: Receiver<Packet>,
}

/// The packets arriving at the daemon's end of a stream, read on their own
/// thread.
fn read_packets(mut daemon: UnixStream) -> Receiver<Packet> {
    let (packets, sent) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = PacketReader::new();
        let mut buf = vec![0; 1 << 16];
        while let Ok(n @ 1..) = daemon.read(&mut buf) {
            for packet in reader.feed(&buf[..n]).expect("whole packets") {
                if packets.send(packet).is_err() {
                    return;
                }
            }
        }
    });
    sent
}

fn rig(readonly: bool) -> Rig {
    let (tx, _rx) = mpsc::channel::<Msg>();
    let (ours, daemon) = UnixStream::pair().expect("socket pair");
    let sent = read_packets(daemon);
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            attempt: 1,
            connected: true,
            ..State::default()
        }),
        cv: Condvar::new(),
        subs: Mutex::new(Vec::new()),
        frame: ArcSwap::from_pointee(Frame::default()),
        observers: AtomicUsize::new(0),
    });
    let core = Core {
        actor: TerminalActor::new(5, 20, 0),
        attempt: AttemptId(1),
        shared,
        backend: Backend::Attach {
            connector: Box::new(|| Err(io::Error::other("no test connection"))),
            opts: AttachOptions {
                readonly,
                ..AttachOptions::default()
            },
            stream: Some(ours),
            tx,
        },
        dirty: false,
        last_publish: Instant::now(),
        paste: PasteRead::default(),
        paste_reads: 0,
        paste_mode: false,
    };
    let (sub, events) = mpsc::channel();
    core.shared.subs.lock().unwrap().push(sub);
    Rig { core, events, sent }
}

impl Rig {
    fn output(&mut self, attempt: u64, bytes: &[u8]) {
        self.core.dispatch(Msg::Output {
            attempt: AttemptId(attempt),
            bytes: bytes.to_vec(),
        });
    }

    /// Write `packet` to its natural target: a listing to the paste, any
    /// other packet to whichever read is open now (or to a read never
    /// handed out, when none is).
    fn write(&mut self, attempt: u64, packet: PastePacket) -> Result<(), PasteRefusal> {
        let target = match packet {
            PastePacket::Listing { .. } => PasteTarget::Paste,
            _ => PasteTarget::Read(self.open_read().unwrap_or(PasteReadId(0))),
        };
        self.write_to(attempt, target, packet)
    }

    fn write_to(
        &mut self,
        attempt: u64,
        target: PasteTarget,
        packet: PastePacket,
    ) -> Result<(), PasteRefusal> {
        let (reply, answer) = mpsc::channel();
        self.core.dispatch(Msg::EnhancedPaste {
            attempt: AttemptId(attempt),
            target,
            packet,
            reply,
        });
        answer.recv().unwrap()
    }

    fn open_read(&self) -> Option<PasteReadId> {
        self.core.paste.open.as_ref().map(|open| open.read)
    }

    /// What the child has been sent: every write is finished by the time
    /// dispatch returns, so this is what arrives without a pause.
    fn sent(&mut self) -> Vec<u8> {
        self.sent_until(|_| false, Duration::from_millis(100))
    }

    /// Wait for at least `len` bytes to have been sent.
    fn sent_at_least(&mut self, len: usize) -> Vec<u8> {
        self.sent_until(|out| out.len() >= len, Duration::from_secs(10))
    }

    fn sent_until(&mut self, done: impl Fn(&[u8]) -> bool, quiet: Duration) -> Vec<u8> {
        let mut out = Vec::new();
        while !done(&out) {
            let Ok(packet) = self.sent.recv_timeout(quiet) else {
                break;
            };
            assert_eq!(packet.type_, MessageType::Data);
            assert!(packet.payload.len() <= INPUT_PACKET);
            out.extend(packet.payload);
        }
        out
    }

    fn paste_events(&self) -> Vec<HandleEvent> {
        self.events
            .try_iter()
            .filter(|event| {
                matches!(
                    event,
                    HandleEvent::EnhancedPasteMode { .. }
                        | HandleEvent::EnhancedPasteRequest { .. }
                )
            })
            .collect()
    }

    fn request(&mut self, id: &str) {
        let bytes = format!("\x1b]5522;type=read:id={id};{PNG_AND_TEXT}\x1b\\");
        self.output(1, bytes.as_bytes());
    }
}

fn data(bytes: usize) -> PastePacket {
    PastePacket::ReadData {
        mime: "image/png".into(),
        data: vec![7; bytes],
    }
}

#[test]
fn events_carry_their_attempt_and_stale_output_publishes_none() {
    let mut r = rig(false);
    r.output(1, b"\x1b[?5522h");
    r.request("a");
    let events = r.paste_events();
    assert_eq!(
        events[0],
        HandleEvent::EnhancedPasteMode {
            attempt: AttemptId(1),
            enabled: true
        }
    );
    let HandleEvent::EnhancedPasteRequest {
        attempt, request, ..
    } = &events[1]
    else {
        panic!("{events:?}");
    };
    assert_eq!(*attempt, AttemptId(1));
    assert_eq!(request.mimes, ["image/png", "text/plain"]);
    assert_eq!(request.id.as_deref(), Some("a"));
    assert_eq!(events.len(), 2);

    // A stream that is no longer current never reaches the terminal.
    r.output(0, b"\x1b[?5522l\x1b]5522;type=read;Lg==\x1b\\");
    assert_eq!(r.paste_events(), []);
    assert!(r.core.actor.modes().enhanced_paste);
}

#[test]
fn a_write_is_checked_in_dispatch() {
    let mut r = rig(false);
    assert_eq!(
        r.write(1, PastePacket::ReadOk),
        Err(PasteRefusal::ModeDisabled)
    );
    r.output(1, b"\x1b[?5522h");
    assert_eq!(
        r.write(2, PastePacket::ReadOk),
        Err(PasteRefusal::StaleAttempt)
    );
    assert_eq!(
        r.write(0, PastePacket::ReadOk),
        Err(PasteRefusal::StaleAttempt)
    );
    for packet in [
        PastePacket::ReadOk,
        data(1),
        PastePacket::ReadDone,
        PastePacket::ReadError(ReadError::Denied),
    ] {
        assert_eq!(
            r.write(1, packet),
            Err(PasteRefusal::StaleRead),
            "no read is open"
        );
    }
    // Within a read, data or DONE before OK is out of order, and a packet
    // aimed at the wrong kind of target is invalid.
    r.request("x");
    let read = r.open_read().unwrap();
    assert_eq!(r.write(1, data(1)), Err(PasteRefusal::OutOfOrder));
    assert_eq!(
        r.write(1, PastePacket::ReadDone),
        Err(PasteRefusal::OutOfOrder)
    );
    let listing = PastePacket::Listing {
        location: Location::Clipboard,
        password: None,
        mimes: vec!["image/png".into()],
    };
    assert_eq!(
        r.write_to(1, PasteTarget::Read(read), listing.clone()),
        Err(PasteRefusal::Invalid)
    );
    assert_eq!(
        r.write_to(1, PasteTarget::Paste, PastePacket::ReadOk),
        Err(PasteRefusal::Invalid)
    );
    assert_eq!(r.sent(), b"", "a refusal sends nothing");

    let mut readonly = rig(true);
    readonly.output(1, b"\x1b[?5522h");
    assert_eq!(
        readonly.write(1, listing.clone()),
        Err(PasteRefusal::ReadOnly)
    );

    r.core.dispatch(Msg::Disconnected {
        attempt: AttemptId(1),
    });
    assert_eq!(r.write(1, listing), Err(PasteRefusal::Disconnected));
}

#[test]
fn a_listing_is_sent_at_once() {
    let mut r = rig(false);
    r.output(1, b"\x1b[?5522h");
    let listing = PastePacket::Listing {
        location: Location::Clipboard,
        password: Some("dG9rZW4=".into()),
        mimes: vec!["image/png".into(), "text/plain".into()],
    };
    r.write(1, listing.clone()).unwrap();
    assert_eq!(r.sent(), listing.encode(None).unwrap());
}

/// The answer reaches the child only when it is whole, echoing the
/// request's id.
#[test]
fn an_answer_is_held_until_done() {
    let mut r = rig(false);
    r.output(1, b"\x1b[?5522h");
    r.request("w7");
    r.write(1, PastePacket::ReadOk).unwrap();
    r.write(1, data(10_000)).unwrap();
    r.write(1, data(MAX_PACKET)).unwrap();
    assert_eq!(r.sent(), b"", "nothing before DONE");
    r.write(1, PastePacket::ReadDone).unwrap();
    let mut expected = PastePacket::ReadOk.encode(Some("w7")).unwrap();
    expected.extend(data(10_000).encode(Some("w7")).unwrap());
    expected.extend(data(MAX_PACKET).encode(Some("w7")).unwrap());
    expected.extend(PastePacket::ReadDone.encode(Some("w7")).unwrap());
    assert_eq!(r.sent_at_least(expected.len()), expected);
    // The read is closed: the next request is a fresh one.
    r.request("w8");
    assert_eq!(r.paste_events().len(), 3);
}

/// One read at a time: another is answered EBUSY at once, never published.
#[test]
fn a_second_read_while_one_is_open_is_busy() {
    let mut r = rig(false);
    r.output(1, b"\x1b[?5522h");
    r.request("first");
    r.request("second");
    assert_eq!(
        r.sent(),
        b"\x1b]5522;type=read:status=EBUSY:id=second\x1b\\"
    );
    assert_eq!(r.paste_events().len(), 2, "the mode and the first request");
    r.write(1, PastePacket::ReadError(ReadError::Denied))
        .unwrap();
    assert_eq!(r.sent(), b"\x1b]5522;type=read:status=EPERM:id=first\x1b\\");
}

#[test]
fn an_oversized_packet_or_answer_is_dropped_whole() {
    let mut r = rig(false);
    r.output(1, b"\x1b[?5522h");
    r.request("big");
    r.write(1, PastePacket::ReadOk).unwrap();
    assert_eq!(
        r.write(1, data(MAX_PACKET + 1)),
        Err(PasteRefusal::PacketTooLarge)
    );
    assert_eq!(
        r.sent(),
        b"\x1b]5522;type=read:status=EBUSY:id=big\x1b\\",
        "an error in place of OK, and none of the answer"
    );
    assert_eq!(
        r.write(1, PastePacket::ReadDone),
        Err(PasteRefusal::StaleRead)
    );

    r.request("huge");
    r.write(1, PastePacket::ReadOk).unwrap();
    for _ in 0..MAX_TRANSACTION / MAX_PACKET {
        r.write(1, data(MAX_PACKET)).unwrap();
    }
    assert_eq!(r.write(1, data(1)), Err(PasteRefusal::TransactionTooLarge));
    assert_eq!(r.sent(), b"\x1b]5522;type=read:status=EBUSY:id=huge\x1b\\");
}

/// Every DATA sequence repeats the id and MIME type, so empty or tiny
/// packets under the longest id must still meet a bound: the held answer
/// is capped encoded, and dropped whole with EBUSY when it passes.
#[test]
fn empty_packets_cannot_grow_an_answer_without_bound() {
    let mut r = rig(false);
    r.output(1, b"\x1b[?5522h");
    let id = "i".repeat(MAX_ID);
    r.request(&id);
    r.write(1, PastePacket::ReadOk).unwrap();
    let empty = PastePacket::ReadData {
        mime: "image/png".into(),
        data: Vec::new(),
    };
    let per_packet = empty.encode(Some(&id)).unwrap().len();
    let mut accepted = 0;
    let refusal = loop {
        match r.write(1, empty.clone()) {
            Ok(()) => accepted += 1,
            Err(refusal) => break refusal,
        }
        assert!(
            accepted <= MAX_ANSWER / per_packet,
            "no bound after {accepted}"
        );
    };
    assert_eq!(refusal, PasteRefusal::TransactionTooLarge);
    assert!(r.core.paste.answer.is_none(), "dropped whole");
    let busy = PastePacket::ReadError(ReadError::Busy)
        .encode(Some(&id))
        .unwrap();
    assert_eq!(r.sent(), busy);
}

/// A whole 32 MiB answer outgrows one protocol packet; it arrives split
/// into DATA packets, in order.
#[test]
fn the_largest_answer_arrives_whole() {
    let mut r = rig(false);
    r.output(1, b"\x1b[?5522h");
    r.request("all");
    r.write(1, PastePacket::ReadOk).unwrap();
    let mut expected = PastePacket::ReadOk.encode(Some("all")).unwrap();
    for _ in 0..MAX_TRANSACTION / MAX_PACKET {
        r.write(1, data(MAX_PACKET)).unwrap();
        expected.extend(data(MAX_PACKET).encode(Some("all")).unwrap());
    }
    expected.extend(PastePacket::ReadDone.encode(Some("all")).unwrap());
    assert!(expected.len() > pty_core::protocol::MAX_PACKET_LENGTH);
    r.write(1, PastePacket::ReadDone).unwrap();
    let sent = r.sent_at_least(expected.len());
    assert!(sent == expected, "the answer arrived whole and in order");
}

/// A read made while paste events are off is not ours to answer and holds
/// nothing: once the mode is on, the next read is published, not refused.
#[test]
fn a_read_with_the_mode_off_holds_no_slot() {
    let mut r = rig(false);
    r.request("early");
    assert_eq!(r.open_read(), None);
    assert_eq!(r.paste_events(), []);
    assert_eq!(
        r.write(1, PastePacket::ReadError(ReadError::NotAvailable)),
        Err(PasteRefusal::ModeDisabled)
    );
    // In stream order: off, a read, on, all in one chunk.
    r.output(
        1,
        b"\x1b[?5522h\x1b[?5522l\x1b]5522;type=read;Lg==\x1b\\\x1b[?5522h",
    );
    assert_eq!(r.open_read(), None, "the read came while the mode was off");
    r.request("later");
    let events = r.paste_events();
    assert!(
        matches!(events.last(), Some(HandleEvent::EnhancedPasteRequest { request, .. }) if request.id.as_deref() == Some("later")),
        "{events:?}"
    );
    assert_eq!(r.sent(), b"", "no EBUSY for the later read");
}

/// A read is answered only under its own id. Read A's late answer, after
/// the mode was turned off and on and read B opened on the same attempt,
/// is refused; it can neither carry A's data under B's id nor close B.
#[test]
fn a_late_answer_never_lands_on_a_later_read() {
    let mut r = rig(false);
    r.output(1, b"\x1b[?5522h");
    r.request("old");
    let a = r.open_read().unwrap();
    r.write_to(1, PasteTarget::Read(a), PastePacket::ReadOk)
        .unwrap();
    r.output(1, b"\x1b[?5522l\x1b[?5522h");
    assert_eq!(r.open_read(), None, "turning the mode off closed A");
    r.request("new");
    let b = r.open_read().unwrap();
    assert_ne!(a, b);
    let secret = PastePacket::ReadData {
        mime: "text/plain".into(),
        data: b"old-secret".to_vec(),
    };
    for packet in [
        PastePacket::ReadOk,
        secret,
        PastePacket::ReadDone,
        PastePacket::ReadError(ReadError::Denied),
    ] {
        assert_eq!(
            r.write_to(1, PasteTarget::Read(a), packet),
            Err(PasteRefusal::StaleRead)
        );
    }
    assert_eq!(r.open_read(), Some(b), "B is still open");
    r.write_to(1, PasteTarget::Read(b), PastePacket::ReadOk)
        .unwrap();
    r.write_to(1, PasteTarget::Read(b), data(3)).unwrap();
    r.write_to(1, PasteTarget::Read(b), PastePacket::ReadDone)
        .unwrap();
    let mut expected = PastePacket::ReadOk.encode(Some("new")).unwrap();
    expected.extend(data(3).encode(Some("new")).unwrap());
    expected.extend(PastePacket::ReadDone.encode(Some("new")).unwrap());
    assert_eq!(r.sent_at_least(expected.len()), expected);
}

/// Turning the mode off, a disconnect or a reconnect closes the open read:
/// nothing of the old attempt is answered later.
#[test]
fn a_reset_closes_the_open_read() {
    let mut r = rig(false);
    r.output(1, b"\x1b[?5522h");
    r.request("a");
    let a = r.open_read().unwrap();
    r.write(1, PastePacket::ReadOk).unwrap();
    r.output(1, b"\x1b[?5522l\x1b[?5522h");
    assert_eq!(
        r.write_to(1, PasteTarget::Read(a), data(1)),
        Err(PasteRefusal::StaleRead)
    );
    r.request("b");
    let events = r.paste_events();
    assert!(
        matches!(events.last(), Some(HandleEvent::EnhancedPasteRequest { request, .. }) if request.id.as_deref() == Some("b")),
        "{events:?}"
    );
    r.core.dispatch(Msg::Disconnected {
        attempt: AttemptId(1),
    });
    assert!(r.core.paste.open.is_none());
    assert_eq!(r.sent(), b"");
}

fn mode(attempt: u64, enabled: bool) -> HandleEvent {
    HandleEvent::EnhancedPasteMode {
        attempt: AttemptId(attempt),
        enabled,
    }
}

/// The end of a stream turns paste events off under the attempt that had
/// them on, before the event that ends it, and the published state agrees.
#[test]
fn the_end_of_a_stream_turns_paste_off_first() {
    let ends: [(Msg, HandleEvent); 2] = [
        (
            Msg::Disconnected {
                attempt: AttemptId(1),
            },
            HandleEvent::Disconnected,
        ),
        (
            Msg::ChildExited {
                attempt: AttemptId(1),
                code: 0,
            },
            HandleEvent::Exited(0),
        ),
    ];
    for (end, ended) in ends {
        let mut r = rig(false);
        r.output(1, b"\x1b[?5522h");
        r.core.dispatch(end);
        let events: Vec<_> = r
            .events
            .try_iter()
            .filter(|event| {
                matches!(event, HandleEvent::EnhancedPasteMode { .. }) || *event == ended
            })
            .collect();
        assert_eq!(events, [mode(1, true), mode(1, false), ended]);
        assert!(!r.core.shared.state.lock().unwrap().modes.enhanced_paste);
    }

    // A reconnect ends the old stream before it tries the new one, so the
    // mode is off under the old attempt even when the new one fails.
    let mut r = rig(false);
    r.output(1, b"\x1b[?5522h");
    assert!(r.core.reconnect().is_err());
    assert_eq!(r.paste_events(), [mode(1, true), mode(1, false)]);
    let st = r.core.shared.state.lock().unwrap();
    assert_eq!((st.attempt, st.modes.enhanced_paste), (2, false));
}

/// A request cut off when its stream ended is never finished by the next
/// stream, even one whose peer skips the SCREEN.
#[test]
fn a_request_cut_off_by_a_disconnect_is_never_finished() {
    let mut r = rig(false);
    r.output(1, b"\x1b[?5522h");
    let request = format!("\x1b]5522;type=read:id=a;{PNG_AND_TEXT}\x1b\\");
    let (head, tail) = request.split_at(20);
    r.output(1, head.as_bytes());
    r.core.dispatch(Msg::Disconnected {
        attempt: AttemptId(1),
    });
    r.core.attempt = AttemptId(2);
    r.output(2, tail.as_bytes());
    assert_eq!(r.paste_events(), [mode(1, true), mode(1, false)]);
    assert_eq!(r.sent(), b"");
}

/// A rig whose one reconnect succeeds, and the packets its new stream
/// carries to the daemon.
fn reconnecting_rig() -> (Rig, Receiver<Packet>) {
    let mut r = rig(false);
    let (ours, daemon) = UnixStream::pair().expect("socket pair");
    let second = read_packets(daemon);
    let ours = Mutex::new(Some(ours));
    if let Backend::Attach { connector, .. } = &mut r.core.backend {
        *connector = Box::new(move || {
            ours.lock()
                .unwrap()
                .take()
                .ok_or_else(|| io::Error::other("one reconnect"))
        });
    }
    (r, second)
}

/// The types of the packets a stream has carried so far.
fn types(sent: &Receiver<Packet>) -> Vec<MessageType> {
    std::iter::from_fn(|| sent.recv_timeout(Duration::from_millis(100)).ok())
        .map(|packet| packet.type_)
        .collect()
}

fn frame(bytes: Vec<u8>) -> Packet {
    PacketReader::new().feed(&bytes).unwrap().remove(0)
}

fn listing() -> PastePacket {
    PastePacket::Listing {
        location: Location::Clipboard,
        password: None,
        mimes: vec!["image/png".into()],
    }
}

/// A write made while attempt 1 was current, then dispatched after a
/// reconnect whose SCREEN turned paste events on again, is refused there.
/// It reaches neither stream: the race a caller would have if it read the
/// attempt and then wrote.
#[test]
fn a_write_queued_before_a_reconnect_is_refused_after_it() {
    let (mut r, second) = reconnecting_rig();
    r.output(1, b"\x1b[?5522h");
    let (reply, answer) = mpsc::channel();
    let queued = Msg::EnhancedPaste {
        attempt: AttemptId(1),
        target: PasteTarget::Paste,
        packet: listing(),
        reply,
    };
    r.core.reconnect().expect("reconnect");
    r.core.dispatch(Msg::Frame {
        attempt: AttemptId(2),
        packet: frame(encode_screen(b"\x1b[?5522h")),
    });
    assert!(r.core.actor.modes().enhanced_paste);
    r.core.dispatch(queued);
    assert_eq!(answer.recv().unwrap(), Err(PasteRefusal::StaleAttempt));
    assert_eq!(types(&r.sent), [MessageType::Detach]);
    assert_eq!(types(&second), [MessageType::Attach]);
}

/// A request cut off by a reconnect is never finished by the new stream,
/// even when its peer sends output before its SCREEN.
#[test]
fn a_request_cut_off_by_a_reconnect_is_never_finished() {
    let (mut r, _second) = reconnecting_rig();
    r.output(1, b"\x1b[?5522h");
    let request = format!("\x1b]5522;type=read:id=a;{PNG_AND_TEXT}\x1b\\");
    let (head, tail) = request.split_at(20);
    r.output(1, head.as_bytes());
    r.core.reconnect().expect("reconnect");
    r.output(2, tail.as_bytes());
    assert_eq!(r.paste_events(), [mode(1, true), mode(1, false)]);
}

/// A frame from an attempt that is no longer current, SCREEN or DATA,
/// publishes nothing and changes no mode.
#[test]
fn a_stale_frame_publishes_no_paste_events() {
    let mut r = rig(false);
    r.output(1, b"\x1b[?5522h");
    let request = format!("\x1b]5522;type=read:id=a;{PNG_AND_TEXT}\x1b\\");
    for bytes in [
        encode_data(format!("\x1b[?5522l{request}").as_bytes()),
        encode_screen(request.as_bytes()),
    ] {
        r.core.dispatch(Msg::Frame {
            attempt: AttemptId(0),
            packet: frame(bytes),
        });
    }
    assert_eq!(r.paste_events(), [mode(1, true)]);
    assert!(r.core.actor.modes().enhanced_paste);
}

/// Output split anywhere, or fed one byte at a time, publishes what the
/// whole does.
#[test]
fn a_split_at_every_byte_through_the_handle_changes_nothing() {
    let stream = format!("\x1b[?5522h\x1b]5522;type=read:id=a;{PNG_AND_TEXT}\x1b\\text\x1b[?5522l")
        .into_bytes();
    let whole = {
        let mut r = rig(false);
        r.output(1, &stream);
        r.paste_events()
    };
    assert_eq!(whole.len(), 3, "{whole:?}");
    for split in 1..stream.len() {
        let mut r = rig(false);
        r.output(1, &stream[..split]);
        r.output(1, &stream[split..]);
        assert_eq!(r.paste_events(), whole, "split at {split}");
    }
    let mut r = rig(false);
    for byte in &stream {
        r.output(1, std::slice::from_ref(byte));
    }
    assert_eq!(r.paste_events(), whole, "one byte at a time");
}

/// After the child exits, a write is refused as disconnected and sends
/// nothing.
#[test]
fn a_write_after_the_child_exits_is_refused_and_sends_nothing() {
    let mut r = rig(false);
    r.output(1, b"\x1b[?5522h");
    r.request("a");
    let read = r.open_read().unwrap();
    r.core.dispatch(Msg::ChildExited {
        attempt: AttemptId(1),
        code: 0,
    });
    assert_eq!(
        r.write_to(1, PasteTarget::Read(read), PastePacket::ReadOk),
        Err(PasteRefusal::Disconnected)
    );
    assert_eq!(r.write(1, listing()), Err(PasteRefusal::Disconnected));
    assert_eq!(r.sent(), b"");
}
