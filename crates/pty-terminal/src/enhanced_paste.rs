//! Enhanced paste: the kitty clipboard protocol's paste events.
//!
//! A program that sets private mode 5522 asks its terminal to announce every
//! paste as an OSC 5522 listing of the clipboard's MIME types instead of
//! bracketed paste text. The program then reads the types it wants with an
//! OSC 5522 read request, and the terminal answers with the data. See
//! <https://sw.kovidgoyal.net/kitty/clipboard/> and
//! <https://rockorager.dev/misc/bracketed-paste-mime/>.
//!
//! This module types both sides: the read request the scanner finds in the
//! child's output, and the [`PastePacket`]s a client answers with. No `Debug`
//! output carries clipboard bytes or a password.

use std::fmt;

/// The longest OSC 5522 sequence the scanner holds back, payload included:
/// the limit on request and listing metadata. Longer ones pass through as
/// bytes and are never typed.
///
/// While a sequence is held, so is the output behind it: an unterminated
/// `ESC ] 5522 ;` delays up to this many bytes of the child's output from
/// reaching clients, until its terminator, a cancel (CAN, SUB or a new ESC)
/// or this limit. Other OSCs hold at most [`crate::strip::MAX_OSC`].
pub const MAX_METADATA: usize = 64 * 1024;

/// The most data one [`PastePacket::ReadData`] may carry.
pub const MAX_PACKET: usize = 1024 * 1024;

/// The most data one read's answer may carry, over all its packets.
pub const MAX_TRANSACTION: usize = 32 * 1024 * 1024;

/// The protocol's chunk: data is sent at most this many bytes, before
/// encoding, per `DATA` sequence.
const DATA_CHUNK: usize = 4096;

/// Which clipboard a request reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    /// The system clipboard (no `loc` key).
    Clipboard,
    /// The primary selection (`loc=primary`).
    Primary,
}

/// A program's OSC 5522 read request, validated: `type=read`, a strict
/// base64 payload holding one or more MIME types, and well-formed optional
/// keys. A request that fails any check is not one, and the terminal must
/// ignore it (the protocol has no error status for reads).
#[derive(Clone, PartialEq, Eq)]
pub struct ReadRequest {
    /// The requested MIME types, in order. `.` alone asks for the listing.
    pub mimes: Vec<String>,
    pub location: Location,
    /// The multiplexer id, echoed in every response.
    pub id: Option<String>,
    /// The password, base64 as sent (the paste event's one-time token).
    pub password: Option<String>,
    /// The human-friendly name, decoded.
    pub name: Option<String>,
}

impl fmt::Debug for ReadRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadRequest")
            .field("mimes", &self.mimes)
            .field("location", &self.location)
            .field("id", &self.id)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("name", &self.name)
            .finish()
    }
}

impl ReadRequest {
    /// Parse an OSC payload (everything between `ESC ]` and the terminator).
    /// `None` unless it is a valid read request.
    pub fn parse(osc_payload: &[u8]) -> Option<ReadRequest> {
        if osc_payload.len() > MAX_METADATA {
            return None;
        }
        let rest = osc_payload.strip_prefix(b"5522;")?;
        let (metadata, payload) = match rest.iter().position(|&b| b == b';') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, &rest[rest.len()..]),
        };
        let metadata = std::str::from_utf8(metadata).ok()?;
        let mut kind = None;
        let mut location = Location::Clipboard;
        let mut id = None;
        let mut password = None;
        let mut name = None;
        // Only the keys the protocol defines may not repeat. Tracking every
        // key would cost quadratic time over 64 KiB of distinct junk keys.
        let mut seen = [false; KEYS.len()];
        for pair in metadata.split(':') {
            let (key, value) = pair.split_once('=')?;
            if let Some(i) = KEYS.iter().position(|known| *known == key) {
                if seen[i] {
                    return None;
                }
                seen[i] = true;
            }
            match key {
                "type" => kind = Some(value),
                "loc" if value == "primary" => location = Location::Primary,
                "loc" => return None,
                "id" => {
                    if !valid_id(value) {
                        return None;
                    }
                    id = Some(value.to_string());
                }
                "pw" => {
                    decode(value.as_bytes())?;
                    password = Some(value.to_string());
                }
                "name" => name = Some(String::from_utf8(decode(value.as_bytes())?).ok()?),
                // A response, or a key that only belongs to writes.
                "status" | "mime" => return None,
                _ => {}
            }
        }
        if kind != Some("read") {
            return None;
        }
        let list = String::from_utf8(decode(payload)?).ok()?;
        let mimes: Vec<String> = list.split_ascii_whitespace().map(str::to_string).collect();
        let valid = !mimes.is_empty() && mimes.iter().all(|mime| valid_mime(mime));
        valid.then_some(ReadRequest {
            mimes,
            location,
            id,
            password,
            name,
        })
    }
}

/// The metadata keys this side of the protocol knows. Any other key is
/// ignored.
const KEYS: [&str; 7] = ["type", "loc", "id", "pw", "name", "status", "mime"];

/// Why a read was refused, in place of its `status=OK`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    /// `ENOSYS`: the requested clipboard is not available.
    NotAvailable,
    /// `EPERM`: the user or the system denied the read.
    Denied,
    /// `EBUSY`: another read is in flight, or the answer could not be sent.
    Busy,
}

impl ReadError {
    fn status(self) -> &'static str {
        match self {
            ReadError::NotAvailable => "ENOSYS",
            ReadError::Denied => "EPERM",
            ReadError::Busy => "EBUSY",
        }
    }
}

/// What a client sends the program, typed. A read's answer is `ReadOk`,
/// any number of `ReadData`, then `ReadDone`, or a single `ReadError`; the
/// request's `id` is echoed by whoever sends them.
#[derive(Clone, PartialEq, Eq)]
pub enum PastePacket {
    /// The paste event: the clipboard's MIME types, unsolicited, with the
    /// one-time password (base64, as it will be sent) the program reads
    /// them with.
    Listing {
        location: Location,
        password: Option<String>,
        mimes: Vec<String>,
    },
    /// Opens a read's answer: `status=OK`.
    ReadOk,
    /// Data for one MIME type, at most [`MAX_PACKET`] bytes. It is sent in
    /// the protocol's 4096-byte chunks.
    ReadData { mime: String, data: Vec<u8> },
    /// Ends a read's answer: `status=DONE`.
    ReadDone,
    /// Refuses a read, in place of `ReadOk`.
    ReadError(ReadError),
}

impl fmt::Debug for PastePacket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PastePacket::Listing {
                location, mimes, ..
            } => f
                .debug_struct("Listing")
                .field("location", location)
                .field("mimes", mimes)
                .finish_non_exhaustive(),
            PastePacket::ReadOk => f.write_str("ReadOk"),
            PastePacket::ReadData { mime, data } => f
                .debug_struct("ReadData")
                .field("mime", mime)
                .field("bytes", &data.len())
                .finish(),
            PastePacket::ReadDone => f.write_str("ReadDone"),
            PastePacket::ReadError(error) => f.debug_tuple("ReadError").field(error).finish(),
        }
    }
}

/// Why a packet cannot be sent as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketError {
    /// An empty listing, or a MIME type, id or password the protocol would
    /// not carry.
    Invalid,
    /// Over [`MAX_METADATA`] of listing, or [`MAX_PACKET`] of data.
    TooLarge,
}

impl PastePacket {
    /// The packet as OSC 5522 sequences, echoing `id` (a read request's).
    /// Validated and within limits, or an error and no bytes.
    pub fn encode(&self, id: Option<&str>) -> Result<Vec<u8>, PacketError> {
        if id.is_some_and(|id| !valid_id(id)) {
            return Err(PacketError::Invalid);
        }
        let mut out = Vec::new();
        match self {
            PastePacket::Listing {
                location,
                password,
                mimes,
            } => {
                if mimes.is_empty() || !mimes.iter().all(|mime| valid_mime(mime)) {
                    return Err(PacketError::Invalid);
                }
                if password
                    .as_deref()
                    .is_some_and(|pw| decode(pw.as_bytes()).is_none())
                {
                    return Err(PacketError::Invalid);
                }
                let mut keys = String::new();
                if *location == Location::Primary {
                    keys.push_str(":loc=primary");
                }
                if let Some(pw) = password {
                    keys.push_str(":pw=");
                    keys.push_str(pw);
                }
                let list = mimes.join(" ");
                if list.len().div_ceil(3) * 4 + keys.len() > MAX_METADATA {
                    return Err(PacketError::TooLarge);
                }
                let pw_only = password
                    .as_ref()
                    .map(|pw| format!(":pw={pw}"))
                    .unwrap_or_default();
                osc(&mut out, &format!("type=read:status=OK{keys}"), "");
                // The listing is data for MIME type `.`, chunked like any.
                let metadata = format!("type=read:status=DATA:mime=Lg=={pw_only}");
                for chunk in list.as_bytes().chunks(DATA_CHUNK) {
                    osc(&mut out, &metadata, &crate::graphics::base64(chunk));
                }
                osc(&mut out, &format!("type=read:status=DONE{pw_only}"), "");
            }
            PastePacket::ReadOk => osc(&mut out, &read_status("OK", id), ""),
            PastePacket::ReadData { mime, data } => {
                if !valid_mime(mime) {
                    return Err(PacketError::Invalid);
                }
                if data.len() > MAX_PACKET {
                    return Err(PacketError::TooLarge);
                }
                let metadata = format!(
                    "{}:mime={}",
                    read_status("DATA", id),
                    crate::graphics::base64(mime.as_bytes())
                );
                for chunk in data.chunks(DATA_CHUNK) {
                    osc(&mut out, &metadata, &crate::graphics::base64(chunk));
                }
            }
            PastePacket::ReadDone => osc(&mut out, &read_status("DONE", id), ""),
            PastePacket::ReadError(error) => osc(&mut out, &read_status(error.status(), id), ""),
        }
        Ok(out)
    }
}

fn read_status(status: &str, id: Option<&str>) -> String {
    match id {
        Some(id) => format!("type=read:status={status}:id={id}"),
        None => format!("type=read:status={status}"),
    }
}

/// `ESC ] 5522 ; metadata [; payload] ESC \`.
fn osc(out: &mut Vec<u8>, metadata: &str, payload: &str) {
    out.extend_from_slice(b"\x1b]5522;");
    out.extend_from_slice(metadata.as_bytes());
    if !payload.is_empty() {
        out.push(b';');
        out.extend_from_slice(payload.as_bytes());
    }
    out.extend_from_slice(b"\x1b\\");
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_+.".contains(&b))
}

fn valid_mime(mime: &str) -> bool {
    !mime.is_empty() && mime.bytes().all(|b| b.is_ascii_graphic())
}

/// Strict RFC 4648 base64 with the standard alphabet: padding required, no
/// whitespace, no other byte. The protocol tells terminals to reject rather
/// than repair.
pub(crate) fn decode(text: &[u8]) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(4) {
        return None;
    }
    let value = |b: u8| -> Option<u32> {
        Some(match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let chunks = text.len() / 4;
    for (index, chunk) in text.chunks(4).enumerate() {
        let last = index + 1 == chunks;
        let pad = chunk.iter().rev().take_while(|&&b| b == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut word = 0u32;
        for &b in &chunk[..4 - pad] {
            word = word << 6 | value(b)?;
        }
        word <<= 6 * pad as u32;
        let bytes = word.to_be_bytes();
        out.extend_from_slice(&bytes[1..4 - pad]);
        // Canonical padding: the bits a pad drops must be zero.
        if pad > 0 && bytes[4 - pad..4].iter().any(|&b| b != 0) {
            return None;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(text: &str) -> String {
        crate::graphics::base64(text.as_bytes())
    }

    fn request(metadata: &str, payload: &str) -> Vec<u8> {
        format!("5522;{metadata};{payload}").into_bytes()
    }

    #[test]
    fn a_read_request_is_typed() {
        let parsed = ReadRequest::parse(&request(
            &format!("type=read:pw={}:name={}", b64("token"), b64("Paste event")),
            &b64("image/png text/plain\n"),
        ))
        .expect("valid");
        assert_eq!(parsed.mimes, ["image/png", "text/plain"]);
        assert_eq!(parsed.location, Location::Clipboard);
        assert_eq!(parsed.password.as_deref(), Some(b64("token").as_str()));
        assert_eq!(parsed.name.as_deref(), Some("Paste event"));

        let listing = ReadRequest::parse(&request("type=read:loc=primary:id=a.b-c", &b64(".")))
            .expect("a listing request");
        assert_eq!(listing.mimes, ["."]);
        assert_eq!(listing.location, Location::Primary);
        assert_eq!(listing.id.as_deref(), Some("a.b-c"));
    }

    #[test]
    fn anything_else_is_not_a_read_request() {
        let png = b64("image/png");
        let spaces = b64("   ");
        let control = b64("image/png\u{1}");
        for (metadata, payload) in [
            ("type=write", png.as_str()),
            ("type=read:status=OK", png.as_str()),
            ("type=read:status=DATA:mime=Lg==", png.as_str()),
            ("type=read:type=read", png.as_str()),
            ("type=read:loc=secondary", png.as_str()),
            ("type=read:id=bad id", png.as_str()),
            ("type=read:pw=not base64", png.as_str()),
            ("type=read", "aW1hZ2UvcG5n\n"),
            ("type=read", "aW1hZ2UvcG5n="),
            ("type=read", ""),
            ("type=read", spaces.as_str()),
            ("type=read", control.as_str()),
            ("read", png.as_str()),
        ] {
            assert_eq!(
                ReadRequest::parse(&request(metadata, payload)),
                None,
                "{metadata};{payload}"
            );
        }
        assert_eq!(ReadRequest::parse(b"52;c;?"), None);
        let mut long = request("type=read", &png);
        long.resize(MAX_METADATA + 1, b'A');
        assert_eq!(ReadRequest::parse(&long), None);
    }

    /// Duplicate checks cover only known keys, so distinct unknown keys
    /// cost linear time and are ignored.
    #[test]
    fn unknown_keys_are_ignored() {
        let junk: Vec<String> = (0..8000).map(|i| format!("k{i}=v")).collect();
        let metadata = format!("type=read:{}", junk.join(":"));
        let parsed = ReadRequest::parse(&request(&metadata, &b64("image/png")));
        assert_eq!(parsed.map(|r| r.mimes), Some(vec!["image/png".to_string()]));
        assert_eq!(
            ReadRequest::parse(&request("type=read:k=1:k=2", &b64("image/png"))).map(|r| r.mimes),
            Some(vec!["image/png".to_string()]),
            "an unknown key may repeat"
        );
    }

    #[test]
    fn debug_output_redacts_the_password() {
        let parsed = ReadRequest::parse(&request(
            &format!("type=read:pw={}", b64("secret-token")),
            &b64("image/png"),
        ))
        .unwrap();
        let debug = format!("{parsed:?}");
        assert!(!debug.contains(&b64("secret-token")), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
    }

    #[test]
    fn a_listing_is_ok_data_done() {
        let listing = PastePacket::Listing {
            location: Location::Primary,
            password: Some(b64("token")),
            mimes: vec!["image/png".into(), "text/plain".into()],
        };
        let bytes = String::from_utf8(listing.encode(None).unwrap()).unwrap();
        let pw = b64("token");
        assert_eq!(
            bytes,
            format!(
                "\x1b]5522;type=read:status=OK:loc=primary:pw={pw}\x1b\\\
                 \x1b]5522;type=read:status=DATA:mime=Lg==:pw={pw};{}\x1b\\\
                 \x1b]5522;type=read:status=DONE:pw={pw}\x1b\\",
                b64("image/png text/plain")
            )
        );
    }

    /// A listing is data for MIME type `.`: over 4096 bytes it is chunked
    /// like any other, and the chunks rejoin to the whole list.
    #[test]
    fn a_long_listing_is_chunked() {
        let mimes: Vec<String> = (0..500).map(|_| "text/plain".to_string()).collect();
        let listing = PastePacket::Listing {
            location: Location::Clipboard,
            password: Some(b64("token")),
            mimes: mimes.clone(),
        };
        let bytes = String::from_utf8(listing.encode(None).unwrap()).unwrap();
        let pw = b64("token");
        let data_head = format!("\x1b]5522;type=read:status=DATA:mime=Lg==:pw={pw};");
        let mut joined = Vec::new();
        let mut chunks = 0;
        for sequence in bytes.split_terminator("\x1b\\") {
            if let Some(payload) = sequence.strip_prefix(&data_head) {
                let chunk = decode(payload.as_bytes()).unwrap();
                assert!(chunk.len() <= DATA_CHUNK, "{}", chunk.len());
                joined.extend(chunk);
                chunks += 1;
            }
        }
        assert_eq!(chunks, 2, "5499 bytes: 4096 + 1403");
        assert_eq!(String::from_utf8(joined).unwrap(), mimes.join(" "));
        assert!(bytes.starts_with(&format!("\x1b]5522;type=read:status=OK:pw={pw}\x1b\\")));
        assert!(bytes.ends_with(&format!("\x1b]5522;type=read:status=DONE:pw={pw}\x1b\\")));
    }

    #[test]
    fn data_is_chunked_and_echoes_the_id() {
        let data: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
        let packet = PastePacket::ReadData {
            mime: "image/png".into(),
            data: data.clone(),
        };
        let bytes = String::from_utf8(packet.encode(Some("w1")).unwrap()).unwrap();
        let sequences: Vec<&str> = bytes.split_terminator("\x1b\\").collect();
        assert_eq!(sequences.len(), 3, "4096 + 4096 + 1808");
        let mut joined = Vec::new();
        for sequence in sequences {
            let rest = sequence
                .strip_prefix(&format!(
                    "\x1b]5522;type=read:status=DATA:id=w1:mime={};",
                    b64("image/png")
                ))
                .unwrap();
            let chunk = decode(rest.as_bytes()).unwrap();
            assert!(chunk.len() <= DATA_CHUNK);
            joined.extend(chunk);
        }
        assert_eq!(joined, data);
        assert_eq!(
            PastePacket::ReadOk.encode(None).unwrap(),
            b"\x1b]5522;type=read:status=OK\x1b\\"
        );
        assert_eq!(
            PastePacket::ReadError(ReadError::Busy)
                .encode(Some("w1"))
                .unwrap(),
            b"\x1b]5522;type=read:status=EBUSY:id=w1\x1b\\"
        );
    }

    #[test]
    fn a_packet_over_a_limit_or_malformed_sends_nothing() {
        let big = PastePacket::ReadData {
            mime: "image/png".into(),
            data: vec![0; MAX_PACKET + 1],
        };
        assert_eq!(big.encode(None), Err(PacketError::TooLarge));
        let exact = PastePacket::ReadData {
            mime: "image/png".into(),
            data: vec![0; MAX_PACKET],
        };
        assert!(exact.encode(None).is_ok());
        let many = PastePacket::Listing {
            location: Location::Clipboard,
            password: None,
            mimes: vec!["x".repeat(1000); 60],
        };
        assert_eq!(many.encode(None), Err(PacketError::TooLarge));
        for invalid in [
            PastePacket::Listing {
                location: Location::Clipboard,
                password: None,
                mimes: vec![],
            },
            PastePacket::Listing {
                location: Location::Clipboard,
                password: Some("not base64".into()),
                mimes: vec!["image/png".into()],
            },
            PastePacket::ReadData {
                mime: "image png".into(),
                data: vec![1],
            },
        ] {
            assert_eq!(
                invalid.encode(None),
                Err(PacketError::Invalid),
                "{invalid:?}"
            );
        }
        assert_eq!(
            PastePacket::ReadOk.encode(Some("bad id")),
            Err(PacketError::Invalid)
        );
    }

    #[test]
    fn debug_output_never_shows_data_or_a_password() {
        let data = PastePacket::ReadData {
            mime: "image/png".into(),
            data: b"SECRET-PIXELS".to_vec(),
        };
        let listing = PastePacket::Listing {
            location: Location::Clipboard,
            password: Some(b64("secret-token")),
            mimes: vec!["image/png".into()],
        };
        for debug in [format!("{data:?}"), format!("{listing:?}")] {
            assert!(!debug.contains("SECRET"), "{debug}");
            assert!(!debug.contains(&b64("secret-token")), "{debug}");
        }
    }

    #[test]
    fn base64_is_strict() {
        assert_eq!(decode(b"").as_deref(), Some(&b""[..]));
        assert_eq!(decode(b"Lg==").as_deref(), Some(&b"."[..]));
        assert_eq!(decode(b"YWI=").as_deref(), Some(&b"ab"[..]));
        assert_eq!(decode(b"YWJj").as_deref(), Some(&b"abc"[..]));
        for bad in [
            &b"Lg"[..],
            b"Lg=",
            b"L===",
            b"Lh==",
            b"Lg==Lg==",
            b"Y WI=",
            b"YW-=",
        ] {
            assert_eq!(decode(bad), None, "{}", String::from_utf8_lossy(bad));
        }
        for text in ["", "a", "ab", "abc", "image/png text/plain", "\u{1F600}"] {
            assert_eq!(
                decode(b64(text).as_bytes()).as_deref(),
                Some(text.as_bytes())
            );
        }
    }
}
