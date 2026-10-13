//! The envelope: the owner's exact declaration of one launch operation.
//!
//! It is validated byte for byte, and `envelopeSha256` is the hash of the
//! exact line, so the reservation, the launcher's first frame and every
//! later command can be bound to the same declaration without any party
//! re-serializing it.

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};

/// The longest envelope line, without its newline.
pub const ENVELOPE_MAX_BYTES: usize = 32 * 1024;
/// The longest identifying string (`operationId`, `owner`, …).
pub const STRING_MAX_BYTES: usize = 256;
/// The longest string binding value.
pub const BINDING_STRING_MAX_BYTES: usize = 1024;
/// The most bindings one envelope carries.
pub const BINDINGS_MAX: usize = 64;

/// The fields of a version 1 envelope, in the order the contract lists them.
const FIELDS: [&str; 9] = [
    "type",
    "version",
    "operationId",
    "owner",
    "origin",
    "subject",
    "runtimeId",
    "expectedIncarnation",
    "bindings",
];

/// A validated envelope.
#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    /// The exact line, without its newline.
    pub line: Vec<u8>,
    /// Lowercase hex SHA-256 of [`Envelope::line`].
    pub sha256: String,
    /// The parsed object, keys in their original order.
    pub value: Value,
    pub operation_id: String,
    pub owner: String,
    pub origin: String,
    pub subject: String,
    pub runtime_id: String,
    /// The PRIOR target only: `<decimal pid>:<createdAt>`.
    pub expected_incarnation: Option<String>,
}

impl Envelope {
    /// `<h>`: the digest that names this operation's record and birth marker.
    pub fn digest(&self) -> String {
        operation_digest(&self.operation_id)
    }
}

/// Why an envelope was refused: the field path (empty for the line itself)
/// and a short reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvelopeError {
    pub path: String,
    pub reason: String,
}

impl EnvelopeError {
    fn new(path: impl Into<String>, reason: impl Into<String>) -> Self {
        EnvelopeError {
            path: path.into(),
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            write!(f, "{}", self.reason)
        } else {
            write!(f, "{}: {}", self.path, self.reason)
        }
    }
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// `<h>`: lowercase hex SHA-256 of the exact `operationId` bytes.
pub fn operation_digest(operation_id: &str) -> String {
    sha256_hex(operation_id.as_bytes())
}

/// Is `value` a digest as [`operation_digest`] writes one?
pub fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Validate one envelope line (without its newline).
pub fn parse_envelope(line: &[u8]) -> Result<Envelope, EnvelopeError> {
    if line.len() > ENVELOPE_MAX_BYTES {
        return Err(EnvelopeError::new("", "too-large"));
    }
    let text = std::str::from_utf8(line).map_err(|_| EnvelopeError::new("", "not-utf8"))?;
    let value = parse_strict_json(text)?;
    let Value::Object(object) = &value else {
        return Err(EnvelopeError::new("", "not-object"));
    };
    for key in object.keys() {
        if !FIELDS.contains(&key.as_str()) {
            return Err(EnvelopeError::new(key.clone(), "unknown-field"));
        }
    }
    for field in FIELDS {
        if !object.contains_key(field) {
            return Err(EnvelopeError::new(field, "missing"));
        }
    }
    if object.get("type") != Some(&Value::String("envelope".into())) {
        return Err(EnvelopeError::new("type", "must-be-envelope"));
    }
    if object.get("version").and_then(Value::as_u64) != Some(1)
        || !object.get("version").is_some_and(Value::is_u64)
    {
        return Err(EnvelopeError::new("version", "unsupported"));
    }
    let string = |field: &str| -> Result<String, EnvelopeError> {
        let value = object.get(field).expect("presence checked above");
        identifying_string(value).map_err(|reason| EnvelopeError::new(field, reason))
    };
    let operation_id = string("operationId")?;
    let owner = string("owner")?;
    let origin = string("origin")?;
    let subject = string("subject")?;
    let runtime_id = string("runtimeId")?;
    let expected_incarnation = match object.get("expectedIncarnation") {
        Some(Value::Null) => None,
        Some(value) => {
            let text = identifying_string(value)
                .map_err(|reason| EnvelopeError::new("expectedIncarnation", reason))?;
            if !is_incarnation_id(&text) {
                return Err(EnvelopeError::new(
                    "expectedIncarnation",
                    "not-pid-colon-created-at",
                ));
            }
            Some(text)
        }
        None => unreachable!("presence checked above"),
    };
    validate_bindings(object.get("bindings").expect("presence checked above"))?;
    Ok(Envelope {
        line: line.to_vec(),
        sha256: sha256_hex(line),
        value: value.clone(),
        operation_id,
        owner,
        origin,
        subject,
        runtime_id,
        expected_incarnation,
    })
}

/// An `S`: 1 to 256 bytes of UTF-8 with no C0 control and no DEL.
fn identifying_string(value: &Value) -> Result<String, &'static str> {
    let Value::String(text) = value else {
        return Err("not-string");
    };
    if text.is_empty() {
        return Err("empty");
    }
    if text.len() > STRING_MAX_BYTES {
        return Err("too-long");
    }
    if text.chars().any(|c| (c as u32) < 0x20 || c == '\u{7f}') {
        return Err("control-character");
    }
    Ok(text.clone())
}

/// `<decimal pid>:<createdAt>`, the incarnation text the registry gives a
/// daemon generation: a positive pid that fits an `i32`, then the ISO-8601
/// millisecond UTC time the daemon publishes as `createdAt`.
pub fn is_incarnation_id(text: &str) -> bool {
    let Some((pid, created_at)) = text.split_once(':') else {
        return false;
    };
    let pid_ok = !pid.is_empty()
        && !pid.starts_with('0')
        && pid.bytes().all(|b| b.is_ascii_digit())
        && pid.parse::<i32>().is_ok_and(|pid| pid > 0);
    pid_ok && is_created_at(created_at)
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ`.
fn is_created_at(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 24 {
        return false;
    }
    bytes.iter().enumerate().all(|(i, b)| match i {
        4 | 7 => *b == b'-',
        10 => *b == b'T',
        13 | 16 => *b == b':',
        19 => *b == b'.',
        23 => *b == b'Z',
        _ => b.is_ascii_digit(),
    })
}

fn validate_bindings(value: &Value) -> Result<(), EnvelopeError> {
    let Value::Object(bindings) = value else {
        return Err(EnvelopeError::new("bindings", "not-object"));
    };
    if bindings.len() > BINDINGS_MAX {
        return Err(EnvelopeError::new("bindings", "too-many"));
    }
    for (key, value) in bindings {
        let path = format!("bindings.{key}");
        if !is_binding_key(key) {
            return Err(EnvelopeError::new(path, "invalid-key"));
        }
        match value {
            Value::Null => {}
            Value::String(text) if text.len() <= BINDING_STRING_MAX_BYTES => {}
            Value::String(_) => return Err(EnvelopeError::new(path, "too-long")),
            Value::Number(number) if number.is_u64() => {}
            _ => return Err(EnvelopeError::new(path, "unsupported-value")),
        }
    }
    Ok(())
}

/// `[A-Za-z][A-Za-z0-9_.-]{0,63}`.
fn is_binding_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_alphabetic()
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// Parse JSON, refusing a key that appears twice in one object. `serde_json`
/// alone keeps the last, which would let two parties read one line
/// differently.
pub fn parse_strict_json(text: &str) -> Result<Value, EnvelopeError> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let value = StrictSeed {
        path: String::new(),
    }
    .deserialize(&mut deserializer)
    .map_err(strict_error)?;
    deserializer.end().map_err(strict_error)?;
    Ok(value)
}

const DUPLICATE_MARKER: &str = "duplicate-key@";

fn strict_error(error: serde_json::Error) -> EnvelopeError {
    let message = error.to_string();
    if let Some(at) = message.find(DUPLICATE_MARKER) {
        let rest = &message[at + DUPLICATE_MARKER.len()..];
        let path = rest.split('\u{0}').next().unwrap_or("").to_string();
        return EnvelopeError::new(path, "duplicate-key");
    }
    EnvelopeError::new("", "malformed-json")
}

struct StrictSeed {
    path: String,
}

impl<'de> DeserializeSeed<'de> for StrictSeed {
    type Value = Value;

    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(StrictVisitor { path: self.path })
    }
}

struct StrictVisitor {
    path: String,
}

impl StrictVisitor {
    fn child(&self, key: &str) -> String {
        if self.path.is_empty() {
            key.to_string()
        } else {
            format!("{}.{key}", self.path)
        }
    }
}

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Value, E> {
        Ok(Number::from_f64(v).map_or(Value::Null, Value::Number))
    }

    fn visit_str<E>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_string()))
    }

    fn visit_string<E>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        let mut index = 0usize;
        while let Some(item) = seq.next_element_seed(StrictSeed {
            path: self.child(&index.to_string()),
        })? {
            items.push(item);
            index += 1;
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut object = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let path = self.child(&key);
            if object.contains_key(&key) {
                return Err(de::Error::custom(format!("{DUPLICATE_MARKER}{path}\u{0}")));
            }
            let value = map.next_value_seed(StrictSeed { path })?;
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(extra: &str) -> String {
        format!(
            r#"{{"type":"envelope","version":1,"operationId":"op-1","owner":"agent/a","origin":"o","subject":"s","runtimeId":"rt","expectedIncarnation":null,"bindings":{{{extra}}}}}"#
        )
    }

    #[test]
    fn a_valid_envelope_parses_and_hashes_its_exact_bytes() {
        let text = line(r#""k":"v","n":7,"z":null"#);
        let envelope = parse_envelope(text.as_bytes()).unwrap();
        assert_eq!(envelope.operation_id, "op-1");
        assert_eq!(envelope.runtime_id, "rt");
        assert_eq!(envelope.sha256, sha256_hex(text.as_bytes()));
        assert_eq!(envelope.digest(), sha256_hex(b"op-1"));
        assert!(is_digest(&envelope.digest()));
        // The same declaration with different spacing is a different line.
        let spaced = text.replacen(r#""version":1"#, r#""version": 1"#, 1);
        assert_ne!(
            parse_envelope(spaced.as_bytes()).unwrap().sha256,
            envelope.sha256
        );
    }

    #[test]
    fn each_refusal_names_its_field() {
        let cases: Vec<(String, &str, &str)> = vec![
            (
                line("").replace(r#""op-1""#, r#""""#),
                "operationId",
                "empty",
            ),
            (
                line("").replace(r#""op-1""#, &format!("\"{}\"", "x".repeat(257))),
                "operationId",
                "too-long",
            ),
            (
                line("").replace(r#""op-1""#, r#""a\u0001b""#),
                "operationId",
                "control-character",
            ),
            (
                line("").replace(r#""op-1""#, r#""a\u007fb""#),
                "operationId",
                "control-character",
            ),
            (line("").replace(r#""rt""#, "7"), "runtimeId", "not-string"),
            (
                line("").replace(r#""version":1"#, r#""version":2"#),
                "version",
                "unsupported",
            ),
            (
                line("").replace(r#""version":1"#, r#""version":1.0"#),
                "version",
                "unsupported",
            ),
            (
                line("").replace(r#""type":"envelope""#, r#""type":"cancel""#),
                "type",
                "must-be-envelope",
            ),
            (
                line("").replace(r#","subject":"s""#, ""),
                "subject",
                "missing",
            ),
            (
                line("").replace(r#""subject":"s""#, r#""subject":"s","extra":1"#),
                "extra",
                "unknown-field",
            ),
            (
                line("").replace("null", r#""12:yesterday""#),
                "expectedIncarnation",
                "not-pid-colon-created-at",
            ),
            (line(r#""1k":"v""#), "bindings.1k", "invalid-key"),
            (line(r#""k":-1"#), "bindings.k", "unsupported-value"),
            (line(r#""k":1e3"#), "bindings.k", "unsupported-value"),
            (line(r#""k":1.5"#), "bindings.k", "unsupported-value"),
            (
                line(r#""k":18446744073709551616"#),
                "bindings.k",
                "unsupported-value",
            ),
            (line(r#""k":true"#), "bindings.k", "unsupported-value"),
            (line(r#""k":["a"]"#), "bindings.k", "unsupported-value"),
            (
                line(&format!(r#""k":"{}""#, "v".repeat(1025))),
                "bindings.k",
                "too-long",
            ),
            (line(r#""k":"a","k":"b""#), "bindings.k", "duplicate-key"),
            (
                line("").replacen(
                    r#""owner":"agent/a""#,
                    r#""owner":"agent/a","owner":"agent/b""#,
                    1,
                ),
                "owner",
                "duplicate-key",
            ),
            ("[]".to_string(), "", "not-object"),
            ("{".to_string(), "", "malformed-json"),
            (format!("{} x", line("")), "", "malformed-json"),
        ];
        for (text, path, reason) in cases {
            let error = parse_envelope(text.as_bytes()).unwrap_err();
            assert_eq!(
                (error.path.as_str(), error.reason.as_str()),
                (path, reason),
                "{text}"
            );
        }
    }

    #[test]
    fn binding_limits_hold_at_their_edges() {
        let max = (0..64)
            .map(|i| format!(r#""k{i}":{i}"#))
            .collect::<Vec<_>>()
            .join(",");
        assert!(parse_envelope(line(&max).as_bytes()).is_ok());
        let over = (0..65)
            .map(|i| format!(r#""k{i}":{i}"#))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            parse_envelope(line(&over).as_bytes()).unwrap_err().reason,
            "too-many"
        );
        let edge = format!(r#""k":"{}","big":18446744073709551615"#, "v".repeat(1024));
        assert!(parse_envelope(line(&edge).as_bytes()).is_ok());
        let long_key = format!(r#""{}":1"#, "k".repeat(65));
        assert_eq!(
            parse_envelope(line(&long_key).as_bytes())
                .unwrap_err()
                .reason,
            "invalid-key"
        );
    }

    #[test]
    fn the_line_size_limit_is_exact() {
        let mut bindings: Vec<String> = Vec::new();
        let (text, pad) = loop {
            let mut all = bindings.clone();
            all.push(r#""pad":"""#.to_string());
            let text = line(&all.join(","));
            let remaining = ENVELOPE_MAX_BYTES - text.len();
            if remaining <= BINDING_STRING_MAX_BYTES {
                break (text, remaining);
            }
            bindings.push(format!(r#""p{}":"{}""#, bindings.len(), "x".repeat(1000)));
        };
        let exact = text.replacen(r#""pad":"""#, &format!(r#""pad":"{}""#, "y".repeat(pad)), 1);
        assert_eq!(exact.len(), ENVELOPE_MAX_BYTES);
        assert!(parse_envelope(exact.as_bytes()).is_ok());
        let over = exact.replacen(r#""pad":""#, r#""pad":"y"#, 1);
        assert_eq!(over.len(), ENVELOPE_MAX_BYTES + 1);
        assert_eq!(
            parse_envelope(over.as_bytes()).unwrap_err().reason,
            "too-large"
        );
    }

    #[test]
    fn incarnation_ids_follow_the_registry_shape() {
        assert!(is_incarnation_id("4242:2026-10-13T17:27:07.118Z"));
        assert!(!is_incarnation_id("0:2026-10-13T17:27:07.118Z"));
        assert!(!is_incarnation_id("042:2026-10-13T17:27:07.118Z"));
        assert!(!is_incarnation_id("4242:2026-10-13T17:27:07Z"));
        assert!(!is_incarnation_id("99999999999:2026-10-13T17:27:07.118Z"));
        assert!(!is_incarnation_id("4242"));
    }
}
