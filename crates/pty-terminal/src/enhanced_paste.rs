//! Enhanced paste: the kitty clipboard protocol's paste events.
//!
//! A program that sets private mode 5522 asks its terminal to announce every
//! paste as an OSC 5522 listing of the clipboard's MIME types instead of
//! bracketed paste text. The program then reads the types it wants with an
//! OSC 5522 read request, and the terminal answers with the data. See
//! <https://sw.kovidgoyal.net/kitty/clipboard/> and
//! <https://rockorager.dev/misc/bracketed-paste-mime/>.
//!
//! This module types the program's side: the read request the scanner finds
//! in the child's output. Nothing here holds clipboard bytes, and no `Debug`
//! output carries a password.

use std::fmt;

/// The longest OSC 5522 sequence the scanner holds back, payload included:
/// the limit on request and listing metadata. Longer ones pass through as
/// bytes and are never typed.
pub const MAX_METADATA: usize = 64 * 1024;

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
        let mut seen = Vec::new();
        for pair in metadata.split(':') {
            let (key, value) = pair.split_once('=')?;
            if seen.contains(&key) {
                return None;
            }
            seen.push(key);
            match key {
                "type" => kind = Some(value),
                "loc" if value == "primary" => location = Location::Primary,
                "loc" => return None,
                "id" => {
                    let valid = !value.is_empty()
                        && value
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"-_+.".contains(&b));
                    if !valid {
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
        let valid = !mimes.is_empty()
            && mimes
                .iter()
                .all(|mime| mime.bytes().all(|b| b.is_ascii_graphic()));
        valid.then_some(ReadRequest {
            mimes,
            location,
            id,
            password,
            name,
        })
    }
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
