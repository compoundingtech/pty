//! Enhanced paste (kitty clipboard paste events, private mode 5522): the
//! mode, the program's read requests, DECRQM, resets and the replay.

use pty_terminal::enhanced_paste::{Location, MAX_METADATA};
use pty_terminal::{SerializeOpts, TerminalActor, TerminalEvent};

fn actor() -> TerminalActor {
    TerminalActor::new(24, 80, 100)
}

/// `image/png text/plain`, base64.
const PNG_AND_TEXT: &str = "aW1hZ2UvcG5nIHRleHQvcGxhaW4=";

fn read_request() -> Vec<u8> {
    format!("\x1b]5522;type=read:pw=dG9rZW4=:name=UGFzdGUgZXZlbnQ=;{PNG_AND_TEXT}\x1b\\")
        .into_bytes()
}

fn paste_events(a: &mut TerminalActor) -> Vec<TerminalEvent> {
    a.take_events()
        .into_iter()
        .filter(|event| {
            matches!(
                event,
                TerminalEvent::EnhancedPasteMode(_) | TerminalEvent::EnhancedPasteRead(_)
            )
        })
        .collect()
}

#[test]
fn the_mode_and_a_read_request_are_events_and_still_reach_clients() {
    let mut a = actor();
    let mut stream = b"before\x1b[?5522h".to_vec();
    stream.extend(read_request());
    stream.extend(b"\x1b[?5522h\x1b[?5522lafter");
    let broadcast = a.write(&stream);
    assert_eq!(
        broadcast, stream,
        "a direct-attached terminal answers it itself"
    );
    let events = paste_events(&mut a);
    assert_eq!(events.len(), 3, "{events:?}");
    assert_eq!(events[0], TerminalEvent::EnhancedPasteMode(true));
    let TerminalEvent::EnhancedPasteRead(request) = &events[1] else {
        panic!("{events:?}");
    };
    assert_eq!(request.mimes, ["image/png", "text/plain"]);
    assert_eq!(request.location, Location::Clipboard);
    assert_eq!(request.name.as_deref(), Some("Paste event"));
    // Setting it twice is one change.
    assert_eq!(events[2], TerminalEvent::EnhancedPasteMode(false));
    assert!(!a.modes().enhanced_paste);
}

/// Split anywhere, the stream yields the same events and the same bytes.
#[test]
fn a_split_at_every_byte_changes_nothing() {
    let mut stream = b"\x1b[?5522h".to_vec();
    stream.extend(read_request());
    stream.extend(b"tail");
    let whole = {
        let mut a = actor();
        a.write(&stream);
        paste_events(&mut a)
    };
    assert_eq!(whole.len(), 2);
    for split in 1..stream.len() {
        let mut a = actor();
        let mut broadcast = a.write(&stream[..split]);
        broadcast.extend(a.write(&stream[split..]));
        assert_eq!(broadcast, stream, "split at {split}");
        assert_eq!(paste_events(&mut a), whole, "split at {split}");
    }
}

#[test]
fn an_invalid_request_is_no_event_but_its_bytes_pass() {
    let mut a = actor();
    for invalid in [
        "\x1b]5522;type=read;aW1hZ2UvcG5n\n\x1b\\".to_string(),
        "\x1b]5522;type=read:status=OK\x1b\\".to_string(),
        format!("\x1b]5522;type=write;{PNG_AND_TEXT}\x1b\\"),
    ] {
        assert_eq!(a.write(invalid.as_bytes()), invalid.as_bytes());
    }
    assert_eq!(paste_events(&mut a), vec![]);
}

/// Other OSCs are given up on past 4 KiB; a read request may use the whole
/// metadata limit.
#[test]
fn a_request_past_the_osc_bound_is_still_typed() {
    let mut a = actor();
    let name = "x".repeat(6000);
    let request = format!(
        "\x1b]5522;type=read:name={};{PNG_AND_TEXT}\x1b\\",
        b64(name.as_bytes())
    );
    assert!(request.len() > pty_terminal::strip::MAX_OSC + 4);
    let mut broadcast = Vec::new();
    for chunk in request.as_bytes().chunks(1000) {
        broadcast.extend(a.write(chunk));
    }
    assert_eq!(broadcast, request.as_bytes());
    let events = paste_events(&mut a);
    assert!(
        matches!(&events[..], [TerminalEvent::EnhancedPasteRead(r)] if r.name.as_deref() == Some(name.as_str())),
        "{events:?}"
    );
}

/// Standard base64, for building requests.
fn b64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let word = chunk.iter().fold(0u32, |w, &b| w << 8 | b as u32) << (8 * (3 - chunk.len()));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(word >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Over the metadata limit a request is refused: no event, no partial
/// sequence held, the bytes pass through, and the next request is seen.
#[test]
fn a_request_over_the_metadata_limit_is_refused_whole() {
    let mut a = actor();
    let head = b"\x1b]5522;type=read;";
    let mut long = head.to_vec();
    long.resize(2 + MAX_METADATA + 1, b'A');
    long.extend(b"\x1b\\");
    let mut broadcast = Vec::new();
    for chunk in long.chunks(4096) {
        broadcast.extend(a.write(chunk));
    }
    assert_eq!(broadcast, long);
    assert_eq!(paste_events(&mut a), vec![]);

    // At the limit it is held whole and parsed (and rejected only because
    // this payload is not a MIME list).
    let mut exact = head.to_vec();
    exact.resize(2 + MAX_METADATA, b'A');
    exact.extend(b"\x1b\\");
    assert_eq!(a.write(&exact), exact);

    assert_eq!(a.write(&read_request()), read_request());
    assert_eq!(paste_events(&mut a).len(), 1);
}

/// DECRQM is answered here (libghostty does not know the mode), and as
/// supported only while a client can answer paste events.
#[test]
fn decrqm_reports_the_mode_only_while_a_client_can_answer_it() {
    let mut a = actor();
    let query = b"\x1b[?5522$p";
    assert_eq!(
        a.write(query),
        b"",
        "a query is answered here, not broadcast"
    );
    assert_eq!(a.take_pty_replies(), b"\x1b[?5522;0$y");
    a.set_enhanced_paste_available(true);
    a.write(query);
    assert_eq!(a.take_pty_replies(), b"\x1b[?5522;2$y");
    a.write(b"\x1b[?5522h");
    a.write(query);
    assert_eq!(a.take_pty_replies(), b"\x1b[?5522;1$y");
}

#[test]
fn every_reset_turns_the_mode_off_as_an_event() {
    let mut a = actor();
    a.write(b"\x1b[?5522h");
    a.write(b"\x1bc");
    assert_eq!(
        paste_events(&mut a),
        [
            TerminalEvent::EnhancedPasteMode(true),
            TerminalEvent::EnhancedPasteMode(false)
        ]
    );

    a.write(b"\x1b[?5522h");
    a.reset();
    assert!(!a.modes().enhanced_paste);
    assert_eq!(
        paste_events(&mut a),
        [
            TerminalEvent::EnhancedPasteMode(true),
            TerminalEvent::EnhancedPasteMode(false)
        ]
    );

    a.write(b"\x1b[?5522h");
    let output = a.reset_input_modes();
    assert!(
        output.windows(8).any(|bytes| bytes == b"\x1b[?5522l"),
        "the client's terminal is told too: {output:?}"
    );
    assert!(!a.modes().enhanced_paste);
    assert_eq!(
        paste_events(&mut a),
        [
            TerminalEvent::EnhancedPasteMode(true),
            TerminalEvent::EnhancedPasteMode(false)
        ]
    );
}

/// A client attaching after the child set the mode learns it from the
/// replay; a peek never carries it; and a request the child made earlier is
/// history, never part of the replay.
#[test]
fn the_attach_replay_carries_the_mode_and_no_request() {
    let mut a = actor();
    a.write(b"\x1b[?5522h");
    a.write(&read_request());
    a.write(b"screen text");
    let replay = a.serialize(SerializeOpts::ATTACH);
    assert!(replay.contains("\x1b[?5522h"), "{replay:?}");
    assert!(!replay.contains("5522;"), "{replay:?}");
    for opts in [SerializeOpts::PEEK, SerializeOpts::PEEK_FULL] {
        assert!(!a.serialize(opts).contains("5522"), "{opts:?}");
    }

    let mut client = actor();
    client.write(replay.as_bytes());
    assert!(client.modes().enhanced_paste);
    assert_eq!(
        paste_events(&mut client),
        [TerminalEvent::EnhancedPasteMode(true)]
    );

    a.write(b"\x1b[?5522l");
    assert!(
        !a.serialize(SerializeOpts::ATTACH).contains("5522"),
        "off is the default and needs no bytes"
    );
}
