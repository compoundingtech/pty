//! Recovery and closure: the occupancy observation the launch decision makes,
//! the birth-marker match, `settle` (observe only) and `retire` (an effect,
//! bound to its request).
//!
//! Every verdict is one self-contained evidence snapshot: the record, the
//! process table with argv, and the registry are each read once, while the
//! operation lock is held shared, so nothing can move the record during the
//! snapshot. A verdict never treats silence as absence: an unreadable table,
//! argv or row is `unknown`, with the component and the detail named.

use std::path::Path;
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::envelope::operation_digest;
use super::record::{
    AFTER_EXIT_CAP, Cause, Decision, Liveness, OperationLock, OperationRecord, OperationStore,
    ProcessId, Retirement, RetirementPhase, State, Writer, WriterRole,
};
use crate::proctable::ProcTable;
use crate::proctable::argv::{ArgvRow, ArgvScan, scan_argv};
use crate::registry::{self, ListOptions, SessionStatus};

/// The metadata field a launch-operation daemon adds to its session record.
pub const MARKER_FIELD: &str = "launchOperation";

/// The command-line option that is every participant's birth marker.
pub const MARKER_OPTION: &str = "--launch-operation";

/// Does `argv` carry the birth marker for `digest`? Only the designated
/// option counts: a `pty run` argv with `--launch-operation <h>` before its
/// `--`, or a `pty __daemon` argv with that value. The digest appearing in a
/// user's arguments after `--` does not.
pub fn argv_carries_marker(argv: &[String], digest: &str) -> bool {
    let options = match argv.iter().skip(1).position(|arg| arg == "--") {
        Some(end) => &argv[1..end + 1],
        None => argv.get(1..).unwrap_or(&[]),
    };
    let Some(verb) = options
        .iter()
        .position(|arg| arg == "run" || arg == "__daemon")
    else {
        return false;
    };
    options[verb + 1..]
        .windows(2)
        .any(|pair| pair[0] == MARKER_OPTION && pair[1] == digest)
}

/// What the registry holds for one session id, as a complete observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Occupancy {
    /// No socket, metadata or pid file.
    Absent,
    /// A daemon is serving it.
    Live { incarnation: Option<String> },
    /// A row with no live daemon. `incarnation` is `<daemonPid>:<createdAt>`
    /// when its metadata names both.
    Ended { incarnation: Option<String> },
    /// A companion that exists could not be read.
    Unreadable { path: String },
}

/// `<daemonPid>:<createdAt>` for a session record.
pub fn incarnation_of(metadata: &registry::SessionMetadata) -> Option<String> {
    let pid = metadata.daemon_pid.filter(|pid| *pid > 0)?;
    (!metadata.created_at.is_empty()).then(|| format!("{pid}:{}", metadata.created_at))
}

/// Observe `name` under `root`: each companion read or `ENOENT`.
pub fn observe_occupancy(root: &Path, name: &str) -> Occupancy {
    let inventory = registry::observe_session_strict_in(root, name, &ListOptions::default());
    if let Some(error) = inventory.errors.first() {
        if error.kind == registry::InventoryErrorKind::RootMissing {
            return Occupancy::Absent;
        }
        return Occupancy::Unreadable {
            path: error.path.to_string_lossy().into_owned(),
        };
    }
    if let Some(entry) = inventory.entries.iter().find(|entry| entry.name == name) {
        let incarnation = entry.metadata.as_ref().and_then(incarnation_of);
        return match entry.status {
            SessionStatus::Running => Occupancy::Live { incarnation },
            _ => Occupancy::Ended { incarnation },
        };
    }
    // A pid file alone is still a row that is not completely absent.
    for path in [
        registry::socket_path_in(root, name),
        root.join(format!("{name}.json")),
        root.join(format!("{name}.pid")),
    ] {
        match std::fs::symlink_metadata(&path) {
            Ok(_) => return Occupancy::Ended { incarnation: None },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Occupancy::Unreadable {
                    path: path.to_string_lossy().into_owned(),
                };
            }
        }
    }
    Occupancy::Absent
}

/// The launch decision's occupancy rule (contract §7.3, rule 2). `Ok` names
/// the ended predecessor that publication may clear, if any.
pub fn occupancy_rule(
    occupancy: &Occupancy,
    expected_incarnation: Option<&str>,
) -> Result<Option<String>, (Cause, String)> {
    match occupancy {
        Occupancy::Absent => Ok(None),
        Occupancy::Unreadable { path } => Err((Cause::Occupied, format!("unreadable:{path}"))),
        Occupancy::Live { incarnation } => Err((
            Cause::Occupied,
            format!("live:{}", incarnation.as_deref().unwrap_or("unknown")),
        )),
        Occupancy::Ended { incarnation } => match (expected_incarnation, incarnation) {
            (Some(expected), Some(found)) if expected == found => Ok(Some(found.clone())),
            (_, found) => Err((
                Cause::PredecessorMismatch,
                format!("ended:{}", found.as_deref().unwrap_or("unknown")),
            )),
        },
    }
}

/// Which command a snapshot answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Settle,
    Retire,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Verified,
    Pending,
    Unknown,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Verified => "verified",
            Status::Pending => "pending",
            Status::Unknown => "unknown",
        }
    }
}

struct Component {
    name: &'static str,
    status: Status,
    detail: Value,
}

impl Component {
    fn json(&self) -> Value {
        json!({"name": self.name, "status": self.status.as_str(), "detail": self.detail})
    }
}

/// One verdict.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub verdict: &'static str,
    pub kind: Option<String>,
    pub no_further_effects: bool,
    /// The whole snapshot as it is printed.
    pub json: Value,
}

struct Snapshot<'a> {
    store: &'a OperationStore,
    operation_id: &'a str,
    envelope_sha256: &'a str,
    request: (RequestKind, Option<&'a str>),
}

impl Snapshot<'_> {
    fn finish(
        &self,
        verdict: &'static str,
        kind: Option<&str>,
        no_further_effects: bool,
        record: Option<&OperationRecord>,
        components: &[Component],
    ) -> Verdict {
        let root = self
            .store
            .root_identity()
            .map_or(Value::Null, |root| json!(root));
        let record_json = record.map_or(Value::Null, |r| {
            json!({
                "schema": r.schema,
                "revision": r.revision,
                "state": r.state,
                "phase": r.phase,
                "decision": r.decision,
                "writer": r.writer,
                "retirement": r.retirement,
                "transfer": r.transfer,
            })
        });
        let json = json!({
            "verdictVersion": 1,
            "verdict": verdict,
            "kind": kind,
            "noFurtherEffects": no_further_effects,
            "root": root,
            "record": record_json,
            "operationId": self.operation_id,
            "envelopeSha256": self.envelope_sha256,
            "envelope": record.map_or(Value::Null, |r| r.envelope.clone()),
            "incarnation": record.and_then(|r| r.incarnation.as_ref()).map_or(Value::Null, |i| json!(i)),
            "request": {
                "kind": match self.request.0 { RequestKind::Settle => "settle", RequestKind::Retire => "retire" },
                "requestId": self.request.1,
            },
            "components": components.iter().map(Component::json).collect::<Vec<_>>(),
            "observedAt": registry::now_iso8601(),
        });
        Verdict {
            verdict,
            kind: kind.map(str::to_string),
            no_further_effects,
            json,
        }
    }
}

/// Inputs a snapshot reads, overridable by debug-build tests so the
/// unreadable and incomplete paths can be driven end to end.
fn snapshot_process_table() -> ProcTable {
    #[cfg(debug_assertions)]
    match std::env::var("PTY_TEST_PROCESS_TABLE").as_deref() {
        Ok("unreadable") => return ProcTable::unreadable(),
        Ok("incomplete") => return ProcTable::read().marked_incomplete(),
        _ => {}
    }
    ProcTable::read()
}

fn snapshot_argv_scan(uid: u32) -> ArgvScan {
    let scan = scan_argv(uid);
    #[cfg(debug_assertions)]
    if let Ok(pids) = std::env::var("PTY_TEST_ARGV_UNREADABLE") {
        let mut scan = scan;
        for pid in pids
            .split(',')
            .filter_map(|pid| pid.trim().parse::<i32>().ok())
        {
            if let Some(at) = scan.rows.iter().position(|row| row.pid == pid) {
                scan.rows.remove(at);
                scan.unreadable.push(pid);
            }
        }
        return scan;
    }
    scan
}

/// How often settle tries the lock again when its holder is already gone.
const WRITER_RELEASE_RETRIES: usize = 8;

/// `pty operation settle`: observe only, never an effect.
pub fn settle(
    store: &OperationStore,
    operation_id: &str,
    envelope_sha256: &str,
    request: (RequestKind, Option<&str>),
) -> Verdict {
    let snap = Snapshot {
        store,
        operation_id,
        envelope_sha256,
        request,
    };
    let digest = operation_digest(operation_id);
    // A writer that lets go between a refused lock and the holder lookup
    // leaves nobody to name, and "live" would describe a moment already
    // over: try the lock again. Only a holder still there is reported.
    let mut holder = None;
    let mut lock = None;
    for _ in 0..WRITER_RELEASE_RETRIES {
        lock = match OperationLock::shared_now(store, &digest) {
            Ok(lock) => lock,
            // No lock: no reservation was ever made here (reserve creates it
            // before the record). Nothing is created by looking.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let component = Component {
                    name: "record",
                    status: Status::Unknown,
                    detail: json!({"reason": "record-missing", "path": store.record_path(&digest)}),
                };
                return snap.finish("unknown", Some("record-missing"), false, None, &[component]);
            }
            Err(error) => {
                let component = Component {
                    name: "writer",
                    status: Status::Unknown,
                    detail: json!({"lock": "unavailable", "error": error.to_string()}),
                };
                return snap.finish("unknown", Some("writer"), false, None, &[component]);
            }
        };
        if lock.is_some() {
            break;
        }
        crate::operation::barrier::barrier("settle-writer-lock-refused");
        holder = OperationLock::holder(store, &digest);
        if holder.is_some() {
            break;
        }
    }
    let read = store.read(&digest);
    let record = match &read {
        Ok(Some(record)) => Some(record),
        _ => None,
    };
    let record_component = match &read {
        Ok(Some(record))
            if record.operation_id == operation_id && record.envelope_sha256 == envelope_sha256 =>
        {
            Component {
                name: "record",
                status: Status::Verified,
                detail: json!({"path": store.record_path(&digest), "revision": record.revision}),
            }
        }
        Ok(Some(record)) => Component {
            name: "record",
            status: Status::Unknown,
            detail: json!({
                "reason": "record-mismatch",
                "operationId": record.operation_id,
                "envelopeSha256": record.envelope_sha256,
            }),
        },
        Ok(None) => Component {
            name: "record",
            status: Status::Unknown,
            detail: json!({"reason": "record-missing", "path": store.record_path(&digest)}),
        },
        Err(error) => Component {
            name: "record",
            status: Status::Unknown,
            detail: json!({"reason": "record-unreadable", "error": error.to_string()}),
        },
    };
    let Some(lock) = lock else {
        // A writer holds the lock for its whole tenure, and the kernel drops
        // it when the writer dies: an unavailable lock is a live writer.
        let writer = Component {
            name: "writer",
            status: Status::Pending,
            detail: json!({
                "holdsLock": true,
                "role": record.map(|r| r.writer.role),
                "holderPid": holder,
            }),
        };
        return snap.finish(
            "live",
            Some("writer"),
            false,
            record,
            &[record_component, writer],
        );
    };
    let Some(record) = record.filter(|_| record_component.status == Status::Verified) else {
        let kind = record_component.detail["reason"]
            .as_str()
            .unwrap_or("record")
            .to_string();
        drop(lock);
        return snap.finish("unknown", Some(&kind), false, record, &[record_component]);
    };

    // One read of each source, all under the shared lock.
    let table = snapshot_process_table();
    let boot = crate::boot::read_boot_identity();
    let uid = scan_uid(store, &digest, record);
    let scan = snapshot_argv_scan(uid);
    let inventory = registry::list_sessions_strict_in(store.root(), &ListOptions::default());
    drop(lock);

    let writer = writer_component(record, &table, boot.as_deref());
    let participants = participants_component(record, &table, boot.as_deref());
    let marker = marker_component(&scan, &digest);
    let registry_component = registry_component(&inventory, &digest, record);
    let components = [
        record_component,
        writer,
        participants,
        marker,
        registry_component,
    ];

    let durable_end = match record.state {
        State::Cancelled => record
            .decision
            .as_ref()
            .is_some_and(|decision| decision.cause != Cause::Launch),
        State::Launched => record
            .retirement
            .as_ref()
            .is_some_and(|retirement| retirement.phase == RetirementPhase::Verified),
        _ => false,
    };
    // Every source of evidence must agree, the registry included: a row that
    // is unreadable, or live and carrying this digest, can still have effects.
    let no_further_effects = durable_end
        && components[1..]
            .iter()
            .all(|component| component.status == Status::Verified);

    if record.state == State::Transferred {
        return snap.finish("transferred", None, false, Some(record), &components);
    }
    if let Some(pending) = components.iter().find(|c| c.status == Status::Pending) {
        return snap.finish(
            "live",
            Some(pending.name),
            no_further_effects,
            Some(record),
            &components,
        );
    }
    if let Some(unknown) = components.iter().find(|c| c.status == Status::Unknown) {
        return snap.finish(
            "unknown",
            Some(unknown.name),
            no_further_effects,
            Some(record),
            &components,
        );
    }
    match record.state {
        State::Reserved | State::Pending => snap.finish(
            "retire-required",
            Some(record.state.as_str()),
            false,
            Some(record),
            &components,
        ),
        State::Cancelled if durable_end => snap.finish(
            "closed",
            Some("never-launched"),
            no_further_effects,
            Some(record),
            &components,
        ),
        State::Launched if durable_end => snap.finish(
            "closed",
            Some("retired"),
            no_further_effects,
            Some(record),
            &components,
        ),
        State::Launched => snap.finish(
            "retire-required",
            Some("launched"),
            false,
            Some(record),
            &components,
        ),
        // A cancelled record whose decision is `launch` cannot be written by
        // this version; it is not something to call closed.
        _ => snap.finish("unknown", Some("record"), false, Some(record), &components),
    }
}

/// The uid whose processes the marker scan reads: the launcher's, or for a
/// reservation the owner of the record file (the uid that ran `reserve`).
fn scan_uid(store: &OperationStore, digest: &str, record: &OperationRecord) -> u32 {
    if let Some(launcher) = &record.participants.launcher {
        return launcher.uid;
    }
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(store.record_path(digest))
        .map(|meta| meta.uid())
        // SAFETY: getuid(2) has no preconditions.
        .unwrap_or_else(|_| unsafe { libc::getuid() })
}

fn liveness_json(liveness: Liveness) -> &'static str {
    match liveness {
        Liveness::Live => "live",
        Liveness::Gone => "gone",
        Liveness::Unknown(reason) => reason,
    }
}

fn writer_component(record: &OperationRecord, table: &ProcTable, boot: Option<&str>) -> Component {
    let Some(id) = record
        .writer
        .id
        .as_ref()
        .filter(|_| record.writer.role != WriterRole::None)
    else {
        return Component {
            name: "writer",
            status: Status::Verified,
            detail: json!({"role": "none"}),
        };
    };
    let liveness = id.liveness(table, boot);
    Component {
        name: "writer",
        status: match liveness {
            Liveness::Gone => Status::Verified,
            Liveness::Live => Status::Pending,
            Liveness::Unknown(_) => Status::Unknown,
        },
        detail: json!({"role": record.writer.role, "pid": id.pid, "startToken": id.start_token, "status": liveness_json(liveness)}),
    }
}

fn participants_component(
    record: &OperationRecord,
    table: &ProcTable,
    boot: Option<&str>,
) -> Component {
    let mut entries = Vec::new();
    let mut status = Status::Verified;
    let mut seen: Vec<&ProcessId> = Vec::new();
    let mut ids: Vec<(&str, &ProcessId)> = Vec::new();
    for (role, id) in [
        ("launcher", record.participants.launcher.as_ref()),
        ("producer", record.participants.producer.as_ref()),
        ("child", record.participants.child.as_ref()),
        ("daemon", record.incarnation.as_ref().map(|inc| &inc.daemon)),
        (
            "child",
            record
                .incarnation
                .as_ref()
                .and_then(|inc| inc.child.as_ref()),
        ),
    ] {
        if let Some(id) = id
            && !seen.contains(&id)
        {
            seen.push(id);
            ids.push((role, id));
        }
    }
    for (role, id) in ids {
        let liveness = id.liveness(table, boot);
        match liveness {
            Liveness::Live => status = Status::Pending,
            Liveness::Unknown(_) if status == Status::Verified => status = Status::Unknown,
            _ => {}
        }
        entries.push(json!({"role": role, "pid": id.pid, "startToken": id.start_token, "status": liveness_json(liveness)}));
    }
    Component {
        name: "participants",
        status,
        detail: json!({"tableComplete": table.is_complete(), "participants": entries}),
    }
}

fn marker_component(scan: &ArgvScan, digest: &str) -> Component {
    let matches: Vec<&ArgvRow> = scan
        .rows
        .iter()
        .filter(|row| argv_carries_marker(&row.argv, digest))
        .collect();
    let status = if !matches.is_empty() {
        Status::Pending
    } else if !scan.is_whole() {
        Status::Unknown
    } else {
        Status::Verified
    };
    Component {
        name: "marker-scan",
        status,
        detail: json!({
            "uid": scan.uid,
            "source": scan.source,
            "readable": scan.readable,
            "complete": scan.complete,
            "rows": scan.rows.len(),
            "zombies": scan.zombies,
            "kernelThreads": scan.kernel_threads,
            "unreadable": scan.unreadable,
            "matches": matches.iter().map(|row| json!({
                "pid": row.pid,
                "startToken": row.identity.as_ref().map(|identity| identity.as_str()),
            })).collect::<Vec<_>>(),
        }),
    }
}

fn registry_component(
    inventory: &registry::Inventory,
    digest: &str,
    record: &OperationRecord,
) -> Component {
    if !inventory.complete {
        return Component {
            name: "registry",
            status: Status::Unknown,
            detail: json!({"errors": inventory.errors}),
        };
    }
    let mut status = Status::Verified;
    let mut rows = Vec::new();
    for entry in &inventory.entries {
        let Some(metadata) = &entry.metadata else {
            continue;
        };
        let carries = metadata
            .extra
            .get(MARKER_FIELD)
            .and_then(|marker| marker.get("digest"))
            .and_then(Value::as_str)
            == Some(digest);
        if !carries {
            continue;
        }
        let incarnation = incarnation_of(metadata);
        let recorded = record.incarnation.as_ref().is_some_and(|inc| {
            incarnation.as_deref() == Some(inc.id.as_str())
                && metadata.generation.as_deref() == Some(inc.generation.as_str())
        });
        let running = entry.status == SessionStatus::Running;
        if !recorded {
            status = Status::Unknown;
        } else if running && record.state != State::Transferred && status == Status::Verified {
            status = Status::Pending;
        }
        rows.push(json!({
            "name": entry.name,
            "incarnation": incarnation,
            "generation": metadata.generation,
            "status": entry.status.as_str(),
            "recorded": recorded,
        }));
    }
    Component {
        name: "registry",
        status,
        detail: json!({"root": inventory.root, "rows": rows}),
    }
}

/// Why `retire` made no change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetireRefusal {
    pub code: &'static str,
    pub detail: String,
}

/// End a recorded held child, and only it.
///
/// The process is pinned first (a Linux `pidfd`, a macOS audit token) and
/// its start identity checked after the pin, so `SIGKILL` can only reach
/// the recorded process. Where it cannot be pinned safely nothing is sent,
/// and the child is reported unknown. A held child never forks, so there is
/// no group to signal.
///
/// Its exit must be observed on the pin, and then the zombie seen: the table
/// is read again for at most [`AFTER_EXIT_CAP`] until it shows one, since on
/// macOS it says the process is still exiting for a moment after the event.
/// A process still exiting after that is unknown, never gone.
fn kill_recorded(id: &ProcessId, boot: Option<&str>) -> Liveness {
    if id.liveness_now(boot) != Liveness::Live {
        return id.liveness_now(boot);
    }
    let pinned = match super::exact::Exact::pin(id, boot) {
        Ok(Some(pinned)) => pinned,
        Ok(None) => return id.liveness_now(boot),
        Err(reason) => return Liveness::Unknown(reason),
    };
    if let Err(reason) = pinned.signal(libc::SIGKILL) {
        return Liveness::Unknown(reason);
    }
    if !pinned.wait_exit(Duration::from_secs(5)) {
        return Liveness::Live;
    }
    id.liveness_after_exit(boot, AFTER_EXIT_CAP)
}

/// `pty operation retire`: retire an operation whose writer and every
/// recorded participant but the held child are gone. Returns the
/// post-effect snapshot (taken after the terminal write and after the
/// exclusive lock is released), or why nothing was done.
pub fn retire(
    store: &OperationStore,
    operation_id: &str,
    envelope_sha256: &str,
    request_id: &str,
    incarnation: Option<&str>,
) -> Result<Verdict, RetireRefusal> {
    let refuse = |code: &'static str, detail: String| Err(RetireRefusal { code, detail });
    let digest = operation_digest(operation_id);
    let lock = match OperationLock::exclusive(store, &digest, Duration::from_millis(250)) {
        Ok(Some(lock)) => lock,
        Ok(None) => return refuse("live-writer", "a writer holds the operation lock".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return refuse(
                "record-missing",
                store.record_path(&digest).display().to_string(),
            );
        }
        Err(error) => return refuse("lock-unavailable", error.to_string()),
    };
    let record = match store.read(&digest) {
        Ok(Some(record)) => record,
        Ok(None) => {
            return refuse(
                "record-missing",
                store.record_path(&digest).display().to_string(),
            );
        }
        Err(error) => return refuse("record-unreadable", error.to_string()),
    };
    // The exact tuple first, before any replay can answer.
    if record.operation_id != operation_id {
        return refuse("record-mismatch", "operationId".into());
    }
    if record.envelope_sha256 != envelope_sha256 {
        return refuse("wrong-envelope", "envelopeSha256".into());
    }
    if record.state == State::Transferred {
        return refuse(
            "transferred",
            "a transferred operation is steady state".into(),
        );
    }
    if record.state == State::Launched {
        let recorded = record.incarnation_id().unwrap_or_default().to_string();
        match incarnation {
            None => return refuse("incarnation-required", recorded),
            Some(given) if given != recorded => return refuse("wrong-incarnation", recorded),
            Some(_) => {}
        }
    } else if let Some(given) = incarnation {
        return refuse(
            "wrong-incarnation",
            format!("{given}: nothing was launched"),
        );
    }
    let snapshot_after = |lock: OperationLock| {
        drop(lock);
        Ok(settle(
            store,
            operation_id,
            envelope_sha256,
            (RequestKind::Retire, Some(request_id)),
        ))
    };
    let phase = record
        .retirement
        .as_ref()
        .map(|retirement| retirement.phase);
    let same_request = record
        .retirement
        .as_ref()
        .is_some_and(|retirement| retirement.request_id == request_id);
    // A finished retirement, or a decision already made, is the answer.
    if phase.is_some_and(|phase| phase != RetirementPhase::Requested) && same_request {
        return snapshot_after(lock);
    }
    if record.state == State::Cancelled || phase == Some(RetirementPhase::Verified) {
        return snapshot_after(lock);
    }
    // Every recorded participant that could still act must be gone: the
    // writer, the launcher, the producer and the daemon. Only the held child
    // is ours to end.
    let boot = crate::boot::read_boot_identity();
    let table = ProcTable::read();
    let mut actors: Vec<(&str, &ProcessId)> = Vec::new();
    if let Some(writer) = &record.writer.id {
        actors.push(("writer", writer));
    }
    for (role, id) in [
        ("launcher", record.participants.launcher.as_ref()),
        ("producer", record.participants.producer.as_ref()),
        ("daemon", record.incarnation.as_ref().map(|inc| &inc.daemon)),
    ] {
        if let Some(id) = id {
            actors.push((role, id));
        }
    }
    for (role, id) in actors {
        match id.liveness(&table, boot.as_deref()) {
            Liveness::Gone => {}
            Liveness::Live => {
                return refuse(
                    if role == "writer" {
                        "live-writer"
                    } else {
                        "live-participant"
                    },
                    format!("{role} pid {}", id.pid),
                );
            }
            Liveness::Unknown(reason) => {
                return refuse("participant-unknown", format!("{role}: {reason}"));
            }
        }
    }
    let me = match ProcessId::current() {
        Ok(me) => me,
        Err(error) => return refuse("identity-unavailable", error),
    };

    // Take authority and record the request before any effect. An
    // interrupted request with this same id is resumed, not repeated.
    let mut requested = record.clone();
    requested.writer = Writer::of(WriterRole::Recovery, me);
    requested.retirement = Some(Retirement {
        request_id: request_id.to_string(),
        phase: RetirementPhase::Requested,
        evidence: json!({}),
    });
    let requested =
        match store.write_cas_as(&lock, &digest, &record, requested, "recovery-requested") {
            Ok(written) => written,
            Err(error) => return refuse("record-write", error.to_string()),
        };

    let mut terminal = requested.clone();
    let mut evidence = Map::new();
    let mut verified = true;
    match requested.state {
        State::Reserved => {
            terminal.state = State::Cancelled;
            terminal.decision = Some(Decision::now(
                Cause::Withdrawn,
                Some(request_id.to_string()),
                None,
            ));
        }
        State::Pending => {
            terminal.state = State::Cancelled;
            terminal.decision = Some(Decision::now(
                Cause::ProducerLost,
                Some(request_id.to_string()),
                None,
            ));
        }
        State::Launched => {
            let child = requested
                .incarnation
                .as_ref()
                .and_then(|inc| inc.child.clone())
                .or_else(|| requested.participants.child.clone());
            if let Some(child) = child {
                let after = kill_recorded(&child, boot.as_deref());
                evidence.insert(
                    "child".into(),
                    json!({"pid": child.pid, "status": liveness_json(after)}),
                );
                verified &= after == Liveness::Gone;
            } else {
                evidence.insert("child".into(), json!(null));
            }
            let row = unwind_row(store.root(), &requested);
            verified &= row == "removed" || row == "absent" || row == "other-generation";
            evidence.insert("row".into(), json!(row));
        }
        State::Cancelled | State::Transferred => unreachable!("handled above"),
    }
    terminal.writer = Writer::none();
    terminal.retirement = Some(Retirement {
        request_id: request_id.to_string(),
        phase: if verified {
            RetirementPhase::Verified
        } else {
            RetirementPhase::Incomplete
        },
        evidence: Value::Object(evidence),
    });
    if let Err(error) =
        store.write_cas_as(&lock, &digest, &requested, terminal, "recovery-terminal")
    {
        return refuse("record-write", error.to_string());
    }
    snapshot_after(lock)
}

/// Remove only the recorded generation's row, fenced by generation and
/// daemon pid under both session locks.
fn unwind_row(root: &Path, record: &OperationRecord) -> &'static str {
    let Some(inc) = &record.incarnation else {
        return "absent";
    };
    let Some(pid) = inc
        .id
        .split_once(':')
        .and_then(|(pid, _)| pid.parse::<i32>().ok())
    else {
        return "unknown-incarnation";
    };
    let name = inc.runtime_id.clone();
    let owner = registry::SessionGenerationOwner {
        generation: inc.generation.clone(),
        pid,
    };
    registry::with_root(root, || {
        match registry::with_both_locks(&name, || {
            let metadata = registry::read_metadata(&name);
            let socket = registry::socket_path(&name).exists();
            let pidfile = registry::pid_path(&name).exists();
            if metadata.is_none() && !socket && !pidfile {
                return "absent";
            }
            if !registry::is_current_generation_owner(&name, &owner)
                || metadata
                    .as_ref()
                    .is_some_and(|m| m.generation.as_deref() != Some(inc.generation.as_str()))
            {
                return "other-generation";
            }
            registry::cleanup_all_while_locked(&name);
            "removed"
        }) {
            Ok(outcome) => outcome,
            Err(_) => "locks-busy",
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A recorded child killed through its pin is gone once its exit is
    /// observed**, whatever the process table says a moment later. On macOS
    /// the table said running for a while after the exit, and recovery used
    /// to read it again and record the child live.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_recorded_child_killed_through_its_pin_is_gone() {
        let boot = crate::boot::read_boot_identity();
        for round in 0..200 {
            let mut child = std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap();
            let id = ProcessId::of(child.id()).unwrap();
            assert_eq!(
                kill_recorded(&id, boot.as_deref()),
                Liveness::Gone,
                "round {round}"
            );
            assert!(!child.wait().unwrap().success(), "round {round}: killed");
        }
    }

    fn argv(text: &str) -> Vec<String> {
        text.split(' ').map(str::to_string).collect()
    }

    #[test]
    fn only_the_designated_option_is_a_birth_marker() {
        let h = "a".repeat(64);
        let run = format!(
            "/usr/bin/pty run -d --operation-fd 9 --launch-operation {h} --id rt -- sleep 1"
        );
        assert!(argv_carries_marker(&argv(&run), &h));
        let daemon = format!("pty __daemon --launch-operation {h}");
        assert!(argv_carries_marker(&argv(&daemon), &h));
        let rooted = format!("pty --root /r run --launch-operation {h} -- x");
        assert!(argv_carries_marker(&argv(&rooted), &h));
        // A decoy with another digest.
        let other = format!("pty __daemon --launch-operation {}", "b".repeat(64));
        assert!(!argv_carries_marker(&argv(&other), &h));
        // The digest in the user's own arguments after `--`.
        let user = format!("pty run -d --id rt -- echo --launch-operation {h}");
        assert!(!argv_carries_marker(&argv(&user), &h));
        // A program that is not a pty participant naming the digest.
        let grep = format!("grep --launch-operation {h}");
        assert!(!argv_carries_marker(&argv(&grep), &h));
        assert!(!argv_carries_marker(&[], &h));
    }

    #[test]
    fn the_occupancy_rule_clears_only_the_exact_predecessor() {
        let inc = "7:2026-10-13T17:27:07.118Z".to_string();
        assert_eq!(occupancy_rule(&Occupancy::Absent, None), Ok(None));
        assert_eq!(occupancy_rule(&Occupancy::Absent, Some(&inc)), Ok(None));
        assert_eq!(
            occupancy_rule(
                &Occupancy::Ended {
                    incarnation: Some(inc.clone())
                },
                Some(&inc)
            ),
            Ok(Some(inc.clone()))
        );
        assert_eq!(
            occupancy_rule(
                &Occupancy::Ended {
                    incarnation: Some(inc.clone())
                },
                None
            )
            .unwrap_err()
            .0,
            Cause::PredecessorMismatch
        );
        assert_eq!(
            occupancy_rule(
                &Occupancy::Ended {
                    incarnation: Some("8:x".into())
                },
                Some(&inc)
            )
            .unwrap_err()
            .0,
            Cause::PredecessorMismatch
        );
        assert_eq!(
            occupancy_rule(&Occupancy::Ended { incarnation: None }, Some(&inc))
                .unwrap_err()
                .0,
            Cause::PredecessorMismatch
        );
        assert_eq!(
            occupancy_rule(
                &Occupancy::Live {
                    incarnation: Some(inc.clone())
                },
                Some(&inc)
            )
            .unwrap_err()
            .0,
            Cause::Occupied
        );
        assert_eq!(
            occupancy_rule(
                &Occupancy::Unreadable {
                    path: "/r/x.json".into()
                },
                None
            ),
            Err((Cause::Occupied, "unreadable:/r/x.json".to_string()))
        );
    }
}
