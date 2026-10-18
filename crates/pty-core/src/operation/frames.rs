//! The control channel's frames: one JSON object per line on the owner's
//! `SOCK_STREAM` socket, at most 128 KiB including the newline.

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::envelope::{is_incarnation_id, parse_strict_json};
use super::record::OperationRecord;

/// The longest frame, newline included.
pub const FRAME_MAX_BYTES: usize = 128 * 1024;

/// Why an owner frame changed nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectCode {
    WrongOperation,
    WrongEnvelope,
    WrongIncarnation,
    UnknownCommand,
    AlreadyTransferred,
    /// A transfer the current state does not allow: the child is not held
    /// yet, or the operation is already decided or retiring.
    NotHeld,
    /// A cancel after the operation was already decided.
    AlreadyDecided,
    Malformed,
}

impl RejectCode {
    pub fn as_str(self) -> &'static str {
        match self {
            RejectCode::WrongOperation => "wrong-operation",
            RejectCode::WrongEnvelope => "wrong-envelope",
            RejectCode::WrongIncarnation => "wrong-incarnation",
            RejectCode::UnknownCommand => "unknown-command",
            RejectCode::AlreadyTransferred => "already-transferred",
            RejectCode::NotHeld => "not-held",
            RejectCode::AlreadyDecided => "already-decided",
            RejectCode::Malformed => "malformed",
        }
    }
}

/// A command the owner may send after the envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerCommand {
    Cancel {
        request_id: String,
    },
    Transfer {
        request_id: String,
        incarnation_id: String,
        generation: String,
    },
}

impl OwnerCommand {
    pub fn request_id(&self) -> &str {
        match self {
            OwnerCommand::Cancel { request_id } | OwnerCommand::Transfer { request_id, .. } => {
                request_id
            }
        }
    }
}

/// One parsed owner line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerFrame {
    Command(OwnerCommand),
    /// A well-formed frame that changes nothing; reading continues.
    Rejected {
        code: RejectCode,
        request_id: Option<String>,
    },
    /// Broken framing: the producer stops reading.
    Malformed,
}

/// Parse one owner line (without its newline) for the operation
/// `operation_id` whose envelope hashed to `envelope_sha256`.
pub fn parse_owner_frame(line: &[u8], operation_id: &str, envelope_sha256: &str) -> OwnerFrame {
    if line.len() + 1 > FRAME_MAX_BYTES {
        return OwnerFrame::Malformed;
    }
    let Ok(text) = std::str::from_utf8(line) else {
        return OwnerFrame::Malformed;
    };
    let Ok(Value::Object(object)) = parse_strict_json(text) else {
        return OwnerFrame::Malformed;
    };
    let request_id = object
        .get("requestId")
        .and_then(Value::as_str)
        .filter(|id| is_request_id(id))
        .map(str::to_string);
    let kind = match object.get("type").and_then(Value::as_str) {
        Some(kind) => kind,
        None => return OwnerFrame::Malformed,
    };
    if kind != "cancel" && kind != "transfer" {
        return OwnerFrame::Rejected {
            code: RejectCode::UnknownCommand,
            request_id,
        };
    }
    let allowed: &[&str] = if kind == "cancel" {
        &[
            "type",
            "version",
            "operationId",
            "envelopeSha256",
            "requestId",
        ]
    } else {
        &[
            "type",
            "version",
            "operationId",
            "envelopeSha256",
            "requestId",
            "incarnation",
        ]
    };
    if object.keys().any(|key| !allowed.contains(&key.as_str()))
        || allowed.iter().any(|key| !object.contains_key(*key))
        || object.get("version").and_then(Value::as_u64) != Some(1)
        || !object.get("version").is_some_and(Value::is_u64)
    {
        return OwnerFrame::Malformed;
    }
    let Some(request_id) = request_id else {
        return OwnerFrame::Malformed;
    };
    let (Some(op), Some(sha)) = (
        object.get("operationId").and_then(Value::as_str),
        object.get("envelopeSha256").and_then(Value::as_str),
    ) else {
        return OwnerFrame::Malformed;
    };
    if op != operation_id {
        return OwnerFrame::Rejected {
            code: RejectCode::WrongOperation,
            request_id: Some(request_id),
        };
    }
    if sha != envelope_sha256 {
        return OwnerFrame::Rejected {
            code: RejectCode::WrongEnvelope,
            request_id: Some(request_id),
        };
    }
    if kind == "cancel" {
        return OwnerFrame::Command(OwnerCommand::Cancel { request_id });
    }
    let Some(Value::Object(incarnation)) = object.get("incarnation") else {
        return OwnerFrame::Malformed;
    };
    if incarnation.len() != 2 {
        return OwnerFrame::Malformed;
    }
    let (Some(id), Some(generation)) = (
        incarnation.get("id").and_then(Value::as_str),
        incarnation.get("generation").and_then(Value::as_str),
    ) else {
        return OwnerFrame::Malformed;
    };
    if !is_incarnation_id(id) || generation.is_empty() || generation.len() > 256 {
        return OwnerFrame::Malformed;
    }
    OwnerFrame::Command(OwnerCommand::Transfer {
        request_id,
        incarnation_id: id.to_string(),
        generation: generation.to_string(),
    })
}

/// A `requestId`: an `S` (1 to 256 bytes, no control characters).
pub fn is_request_id(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 256
        && !text.chars().any(|c| (c as u32) < 0x20 || c == '\u{7f}')
}

/// A ticket as frames report it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TicketView {
    pub id: u64,
    pub kind: &'static str,
    pub state: &'static str,
}

/// The fields every producer frame carries, from the record as last
/// written. `record` is `None` only for a launcher refusal that came before
/// any record was read.
pub fn producer_frame(
    kind: &str,
    seq: u64,
    operation_id: &str,
    envelope_sha256: &str,
    envelope: &Value,
    record: Option<&OperationRecord>,
    tickets: &[TicketView],
    extra: Map<String, Value>,
) -> Vec<u8> {
    let mut frame = Map::new();
    frame.insert("type".into(), json!(kind));
    frame.insert("version".into(), json!(1));
    frame.insert("seq".into(), json!(seq));
    frame.insert("operationId".into(), json!(operation_id));
    frame.insert("envelopeSha256".into(), json!(envelope_sha256));
    frame.insert("envelope".into(), envelope.clone());
    frame.insert(
        "recordRevision".into(),
        record.map_or(Value::Null, |r| json!(r.revision)),
    );
    frame.insert(
        "state".into(),
        record.map_or(Value::Null, |r| json!(r.state)),
    );
    frame.insert(
        "phase".into(),
        record.map_or(Value::Null, |r| json!(r.phase)),
    );
    frame.insert(
        "participants".into(),
        record.map_or(Value::Null, |r| json!(r.participants)),
    );
    frame.insert(
        "incarnation".into(),
        record.map_or(Value::Null, |r| json!(r.incarnation)),
    );
    frame.insert("tickets".into(), json!(tickets));
    for (key, value) in extra {
        frame.insert(key, value);
    }
    let mut line = serde_json::to_vec(&Value::Object(frame)).expect("a JSON map serializes");
    line.push(b'\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    const OP: &str = "op";
    const SHA: &str = "abc";

    fn parse(text: &str) -> OwnerFrame {
        parse_owner_frame(text.as_bytes(), OP, SHA)
    }

    #[test]
    fn commands_parse_and_wrong_bindings_are_rejected() {
        assert_eq!(
            parse(
                r#"{"type":"cancel","version":1,"operationId":"op","envelopeSha256":"abc","requestId":"r1"}"#
            ),
            OwnerFrame::Command(OwnerCommand::Cancel {
                request_id: "r1".into()
            })
        );
        assert_eq!(
            parse(
                r#"{"type":"transfer","version":1,"operationId":"op","envelopeSha256":"abc","requestId":"r2","incarnation":{"id":"7:2026-10-13T17:27:07.118Z","generation":"g"}}"#
            ),
            OwnerFrame::Command(OwnerCommand::Transfer {
                request_id: "r2".into(),
                incarnation_id: "7:2026-10-13T17:27:07.118Z".into(),
                generation: "g".into(),
            })
        );
        assert_eq!(
            parse(
                r#"{"type":"cancel","version":1,"operationId":"other","envelopeSha256":"abc","requestId":"r1"}"#
            ),
            OwnerFrame::Rejected {
                code: RejectCode::WrongOperation,
                request_id: Some("r1".into())
            }
        );
        assert_eq!(
            parse(
                r#"{"type":"cancel","version":1,"operationId":"op","envelopeSha256":"zzz","requestId":"r1"}"#
            ),
            OwnerFrame::Rejected {
                code: RejectCode::WrongEnvelope,
                request_id: Some("r1".into())
            }
        );
        assert_eq!(
            parse(r#"{"type":"resume","requestId":"r9"}"#),
            OwnerFrame::Rejected {
                code: RejectCode::UnknownCommand,
                request_id: Some("r9".into())
            }
        );
    }

    #[test]
    fn broken_framing_is_malformed() {
        for text in [
            "",
            "not json",
            "[1]",
            r#"{"version":1}"#,
            r#"{"type":"cancel","version":1,"operationId":"op","envelopeSha256":"abc"}"#,
            r#"{"type":"cancel","version":2,"operationId":"op","envelopeSha256":"abc","requestId":"r"}"#,
            r#"{"type":"cancel","version":1,"operationId":"op","envelopeSha256":"abc","requestId":"r","x":1}"#,
            r#"{"type":"cancel","version":1,"operationId":"op","operationId":"op","envelopeSha256":"abc","requestId":"r"}"#,
            r#"{"type":"transfer","version":1,"operationId":"op","envelopeSha256":"abc","requestId":"r","incarnation":{"id":"nope","generation":"g"}}"#,
        ] {
            assert_eq!(parse(text), OwnerFrame::Malformed, "{text}");
        }
        assert_eq!(
            parse_owner_frame(&[0xff, b'{', b'}'], OP, SHA),
            OwnerFrame::Malformed
        );
        let oversize = format!(
            r#"{{"type":"cancel","version":1,"operationId":"op","envelopeSha256":"abc","requestId":"{}"}}"#,
            "r".repeat(FRAME_MAX_BYTES)
        );
        assert_eq!(parse(&oversize), OwnerFrame::Malformed);
    }

    #[test]
    fn a_producer_frame_is_one_line_with_the_common_fields() {
        let envelope = json!({"type":"envelope"});
        let line = producer_frame("refused", 3, OP, SHA, &envelope, None, &[], {
            let mut extra = Map::new();
            extra.insert("code".into(), json!("not-claimable"));
            extra
        });
        assert_eq!(line.last(), Some(&b'\n'));
        assert_eq!(line.iter().filter(|b| **b == b'\n').count(), 1);
        let value: Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(value["type"], "refused");
        assert_eq!(value["seq"], 3);
        assert_eq!(value["envelope"], envelope);
        assert_eq!(value["code"], "not-claimable");
        assert!(value["recordRevision"].is_null());
    }
}
