//! `pty operation reserve|show|settle|retire`: the owner's side of a launch
//! operation outside the control channel. Each prints exactly one JSON
//! object on stdout.
//!
//! - `reserve --json` reads the envelope on stdin and creates the record
//!   exclusively, in state `reserved`. It creates no process.
//! - `show <operationId> --json` prints the record as stored.
//! - `settle <operationId> --envelope-sha256 H --json` observes only.
//! - `retire <operationId> --envelope-sha256 H --request-id S
//!   [--incarnation ID] --json` retires an operation whose writer is gone.
//!
//! Exit status: 0 when the verb printed its answer (a verdict, a record, a
//! reservation), 1 when it printed a refusal, 64 for a usage error.

use std::io::Read;

use serde_json::{Value, json};

use super::CliResult;
use pty_core::operation::envelope::{ENVELOPE_MAX_BYTES, parse_envelope};
use pty_core::operation::frames::is_request_id;
use pty_core::operation::record::CreateError;
use pty_core::operation::settle::{RequestKind, retire, settle};
use pty_core::operation::{OperationRecord, OperationStore, is_digest, operation_digest};

const USAGE: &str = "Usage: pty operation reserve --json < envelope
       pty operation show <operationId> --json
       pty operation settle <operationId> --envelope-sha256 <hex> --json
       pty operation retire <operationId> --envelope-sha256 <hex> --request-id <id> [--incarnation <pid:createdAt>] --json

Launch-operation records under <root>/operations. Each command prints one JSON object.

Examples:
  pty operation reserve --json < envelope.json
  pty operation settle op-1 --envelope-sha256 <hex> --json
";

fn usage(message: &str) -> CliResult {
    eprintln!("pty operation: {message}");
    eprint!("{USAGE}");
    Ok(64)
}

fn print(value: &Value) {
    println!("{value}");
}

pub fn run(args: &[String]) -> CliResult {
    let Some(verb) = args.first().map(String::as_str) else {
        return usage("expected reserve, show, settle or retire.");
    };
    if matches!(verb, "-h" | "--help")
        || (args.len() == 2 && matches!(args[1].as_str(), "-h" | "--help"))
    {
        print!("{USAGE}");
        return Ok(0);
    }
    let store = OperationStore::current();
    match verb {
        "reserve" => reserve(&store, &args[1..]),
        "show" => show(&store, &args[1..]),
        "settle" => settle_verb(&store, &args[1..]),
        "retire" => retire_verb(&store, &args[1..]),
        other => usage(&format!("unknown subcommand \"{other}\".")),
    }
}

/// `--flag value` pairs plus positionals; `--json` is required everywhere.
struct Parsed {
    positional: Vec<String>,
    envelope_sha256: Option<String>,
    request_id: Option<String>,
    incarnation: Option<String>,
    json: bool,
}

fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut parsed = Parsed {
        positional: Vec::new(),
        envelope_sha256: None,
        request_id: None,
        incarnation: None,
        json: false,
    };
    let mut i = 0;
    while i < args.len() {
        let slot = match args[i].as_str() {
            "--json" => {
                parsed.json = true;
                i += 1;
                continue;
            }
            "--envelope-sha256" => &mut parsed.envelope_sha256,
            "--request-id" => &mut parsed.request_id,
            "--incarnation" => &mut parsed.incarnation,
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}.")),
            value => {
                parsed.positional.push(value.to_string());
                i += 1;
                continue;
            }
        };
        let Some(value) = args.get(i + 1) else {
            return Err(format!("{} requires a value.", args[i]));
        };
        if slot.is_some() {
            return Err(format!("{} may only be given once.", args[i]));
        }
        *slot = Some(value.clone());
        i += 2;
    }
    if !parsed.json {
        return Err("--json is required.".into());
    }
    Ok(parsed)
}

fn root_json(store: &OperationStore) -> Value {
    store
        .root_identity()
        .map_or(Value::Null, |root| json!(root))
}

fn reserve(store: &OperationStore, args: &[String]) -> CliResult {
    let parsed = match parse(args) {
        Ok(parsed) => parsed,
        Err(message) => return usage(&message),
    };
    if !parsed.positional.is_empty() {
        return usage("reserve reads the envelope on stdin and takes no arguments.");
    }
    let refused = |code: &str, path: &str, reason: &str| {
        print(&json!({"verdict": "refused", "code": code, "path": path, "reason": reason}));
        Ok(1)
    };
    let mut input = Vec::new();
    if let Err(error) = std::io::stdin()
        .lock()
        .take((ENVELOPE_MAX_BYTES + 2) as u64)
        .read_to_end(&mut input)
    {
        return refused("envelope-invalid", "", &format!("stdin: {error}"));
    }
    let line = match input.iter().position(|b| *b == b'\n') {
        Some(end) if end + 1 == input.len() => &input[..end],
        Some(_) => return refused("envelope-invalid", "", "trailing-data"),
        None => &input[..],
    };
    let envelope = match parse_envelope(line) {
        Ok(envelope) => envelope,
        Err(error) => return refused("envelope-invalid", &error.path, &error.reason),
    };
    let digest = envelope.digest();
    let record = OperationRecord::reserved(&envelope);
    match store.create_exclusive(&digest, &record) {
        Ok(()) => {
            print(&json!({
                "verdict": "reserved",
                "operationId": envelope.operation_id,
                "envelopeSha256": envelope.sha256,
                "digest": digest,
                "root": root_json(store),
                "record": record,
            }));
            Ok(0)
        }
        Err(CreateError::Exists) => {
            print(&json!({
                "verdict": "refused",
                "code": "operation-exists",
                "operationId": envelope.operation_id,
                "digest": digest,
            }));
            Ok(1)
        }
        Err(CreateError::Io(error)) => refused("io", "", &error.to_string()),
    }
}

/// The one positional every other verb takes: the exact `operationId`.
fn operation_id(parsed: &Parsed) -> Result<String, String> {
    match parsed.positional.as_slice() {
        [id] if !id.is_empty() => Ok(id.clone()),
        _ => Err("expected exactly one <operationId>.".into()),
    }
}

fn show(store: &OperationStore, args: &[String]) -> CliResult {
    let parsed = match parse(args) {
        Ok(parsed) => parsed,
        Err(message) => return usage(&message),
    };
    let id = match operation_id(&parsed) {
        Ok(id) => id,
        Err(message) => return usage(&message),
    };
    let digest = operation_digest(&id);
    match store.read(&digest) {
        Ok(record) => {
            let found = record.is_some();
            print(&json!({
                "operationId": id,
                "digest": digest,
                "root": root_json(store),
                "record": record,
            }));
            Ok(if found { 0 } else { 1 })
        }
        Err(error) => {
            print(&json!({
                "operationId": id,
                "digest": digest,
                "root": root_json(store),
                "record": null,
                "error": error.to_string(),
            }));
            Ok(1)
        }
    }
}

fn envelope_sha(parsed: &Parsed) -> Result<String, String> {
    match &parsed.envelope_sha256 {
        Some(sha) if is_digest(sha) => Ok(sha.clone()),
        Some(_) => Err("--envelope-sha256 must be 64 lowercase hex characters.".into()),
        None => Err("--envelope-sha256 is required.".into()),
    }
}

fn settle_verb(store: &OperationStore, args: &[String]) -> CliResult {
    let parsed = match parse(args) {
        Ok(parsed) => parsed,
        Err(message) => return usage(&message),
    };
    if parsed.request_id.is_some() || parsed.incarnation.is_some() {
        return usage("settle takes no --request-id or --incarnation.");
    }
    let (id, sha) = match (operation_id(&parsed), envelope_sha(&parsed)) {
        (Ok(id), Ok(sha)) => (id, sha),
        (Err(message), _) | (_, Err(message)) => return usage(&message),
    };
    print(&settle(store, &id, &sha, (RequestKind::Settle, None)).json);
    Ok(0)
}

fn retire_verb(store: &OperationStore, args: &[String]) -> CliResult {
    let parsed = match parse(args) {
        Ok(parsed) => parsed,
        Err(message) => return usage(&message),
    };
    let (id, sha) = match (operation_id(&parsed), envelope_sha(&parsed)) {
        (Ok(id), Ok(sha)) => (id, sha),
        (Err(message), _) | (_, Err(message)) => return usage(&message),
    };
    let Some(request_id) = parsed.request_id.filter(|id| is_request_id(id)) else {
        return usage("--request-id is required: 1 to 256 bytes, no control characters.");
    };
    match retire(store, &id, &sha, &request_id, parsed.incarnation.as_deref()) {
        Ok(verdict) => {
            print(&verdict.json);
            Ok(0)
        }
        Err(refusal) => {
            print(&json!({
                "verdictVersion": 1,
                "verdict": "refused",
                "code": refusal.code,
                "detail": refusal.detail,
                "operationId": id,
                "envelopeSha256": sha,
                "request": {"kind": "retire", "requestId": request_id},
            }));
            Ok(1)
        }
    }
}
