//! Read-only observation of the registry: `list_sessions` and the liveness
//! rules behind it, reference resolution, and the process helpers the
//! lifecycle commands share. Nothing here creates, unlinks or repairs.
//!
//! node: src/sessions.ts:183-199, 801-817, 895-1013, 1345-1370, 2076-2175

use std::collections::{BTreeSet, HashMap};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::atomic::is_tmp_name;
use super::lock::parse_leading_int;
use super::metadata::{SessionMetadata, read_metadata_at};
use super::root::{metadata_path, pid_path, session_dir, socket_path_in};
use crate::unix_connect::{self, Connect};

/// `running` / `exited` / `vanished`.
///
/// node: src/sessions.ts:183-199
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    /// A matching daemon, or an unproven owner conservatively retained.
    Running,
    /// The daemon wrote an exit record before shutting down.
    Exited,
    /// The daemon is gone and no exit record was written.
    Vanished,
}

impl SessionStatus {
    /// The wire/CLI string.
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionStatus::Running => "running",
            SessionStatus::Exited => "exited",
            SessionStatus::Vanished => "vanished",
        }
    }

    /// `exited` or `vanished`: there is a record but no live daemon.
    pub fn is_gone(&self) -> bool {
        !matches!(self, SessionStatus::Running)
    }
}

impl std::fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One session as `pty list` sees it.
///
/// node: src/sessions.ts:183-199
#[derive(Debug, Clone, PartialEq)]
pub struct SessionInfo {
    /// The stable id.
    pub name: String,
    pub socket_path: PathBuf,
    /// The named daemon pid, retained on Unknown; never signal authority.
    pub pid: Option<i32>,
    pub status: SessionStatus,
    /// Ownership evidence, independent of the conservative display status.
    pub identity: DaemonIdentityCheck,
    /// `None` when `<name>.json` is missing or unreadable (a socket-only
    /// entry).
    /// Ordinary listings omit `last_lines`, `extra_env`, and `session_env`;
    /// [`list_sessions_full`] and reference lookups retain them for restart.
    pub metadata: Option<SessionMetadata>,
}

impl SessionInfo {
    /// Is the session conservatively listed as running? Not signal authority.
    pub fn is_running(&self) -> bool {
        self.status == SessionStatus::Running
    }

    /// `exited` or `vanished`.
    pub fn is_gone(&self) -> bool {
        self.status.is_gone()
    }

    /// The display name, when the record has one.
    pub fn display_name(&self) -> Option<&str> {
        self.metadata.as_ref()?.display_name.as_deref()
    }
}

/// Default shared budget for strict unknown-identity transport diagnostics.
pub const DEFAULT_SOCKET_PROBE_BUDGET: Duration = Duration::from_millis(500);

/// Per-socket connect timeout inside the probe.
pub const SOCKET_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Knobs for [`list_sessions_with`].
#[derive(Debug, Clone)]
pub struct ListOptions {
    /// Shared deadline for strict unknown-identity socket diagnostics only.
    /// Probes never establish liveness or override a proven mismatch/absence.
    pub socket_probe_budget: Duration,
}

impl Default for ListOptions {
    fn default() -> Self {
        ListOptions {
            socket_probe_budget: DEFAULT_SOCKET_PROBE_BUDGET,
        }
    }
}

/// A read-only inventory, including evidence that the observation is incomplete.
#[derive(Debug, Clone, PartialEq)]
pub struct Inventory {
    pub root: PathBuf,
    pub complete: bool,
    pub entries: Vec<SessionInfo>,
    pub errors: Vec<InventoryError>,
}

/// Why a strict inventory could not completely observe the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InventoryErrorKind {
    RootMissing,
    RootUnreadable,
    EntryUnreadable,
    MetadataUnreadable,
    MetadataMalformed,
    PidUnreadable,
    PidMalformed,
    ProbeTimeout,
    IdentityUnknown,
}

/// An observation failure at an exact registry path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InventoryError {
    #[serde(serialize_with = "serialize_inventory_path")]
    pub path: PathBuf,
    pub kind: InventoryErrorKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

fn serialize_inventory_path<S: serde::Serializer>(
    path: &Path,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&path.to_string_lossy())
}

struct Scanner {
    strict: bool,
    full_metadata: bool,
    errors: Vec<InventoryError>,
}

impl Scanner {
    fn error(&mut self, path: &Path, kind: InventoryErrorKind, detail: impl ToString) {
        if self.strict {
            self.errors.push(InventoryError {
                path: path.to_path_buf(),
                kind,
                detail: Some(detail.to_string()),
            });
        }
    }

    fn read_optional(
        &mut self,
        path: &Path,
        kind: InventoryErrorKind,
        observed: bool,
    ) -> Option<Vec<u8>> {
        let read = if self.strict {
            use std::io::Read;
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path)
                .and_then(|mut file| {
                    // File::metadata uses fstat on the opened descriptor, not
                    // a path lookup that could race a sidecar replacement.
                    if !file.metadata()?.is_file() {
                        return Err(std::io::Error::other(
                            "registry companion is not a regular file",
                        ));
                    }
                    let mut bytes = Vec::new();
                    file.read_to_end(&mut bytes)?;
                    Ok(bytes)
                })
        } else {
            std::fs::read(path)
        };
        match read {
            Ok(bytes) => Some(bytes),
            Err(error) => {
                // A dangling symlink is an existing but unreadable sidecar.
                if self.strict
                    && (observed
                        || error.kind() != std::io::ErrorKind::NotFound
                        || std::fs::symlink_metadata(path).is_ok())
                {
                    self.error(path, kind, error);
                }
                None
            }
        }
    }

    fn metadata(&mut self, path: &Path, observed: bool) -> Option<SessionMetadata> {
        let bytes = self.read_optional(path, InventoryErrorKind::MetadataUnreadable, observed)?;
        let result = super::list_metadata::parse_with_payloads(&bytes, self.full_metadata);
        match result {
            Ok(metadata) => Some(metadata),
            Err(error) => {
                self.error(path, InventoryErrorKind::MetadataMalformed, error);
                None
            }
        }
    }

    fn pid(&mut self, path: &Path, observed: bool) -> Option<i32> {
        let bytes = self.read_optional(path, InventoryErrorKind::PidUnreadable, observed)?;
        let content = match std::str::from_utf8(&bytes) {
            Ok(content) => content.trim(),
            Err(error) => {
                self.error(path, InventoryErrorKind::PidMalformed, error);
                return None;
            }
        };
        if self.strict
            && (content.is_empty()
                || !content.bytes().all(|byte| byte.is_ascii_digit())
                || !content.parse::<i32>().is_ok_and(|pid| pid > 0))
        {
            self.error(
                path,
                InventoryErrorKind::PidMalformed,
                "expected a positive decimal PID",
            );
        }
        // Listing remains byte-compatible with Node's tolerant parseInt.
        parse_leading_int(content)
    }
}

/// Read a complete, regular-file identity observation without blocking on
/// FIFOs or treating malformed/unreadable sidecars as absent.
pub(super) fn read_identity_record(
    root: &Path,
    name: &str,
) -> Option<(SessionMetadata, Option<i32>)> {
    let mut scanner = Scanner {
        strict: true,
        full_metadata: true,
        errors: Vec::new(),
    };
    let metadata = scanner.metadata(&root.join(format!("{name}.json")), false)?;
    let sidecar = scanner.pid(&root.join(format!("{name}.pid")), false);
    scanner.errors.is_empty().then_some((metadata, sidecar))
}

/// Is a process with `pid` alive? `kill(pid, 0)` succeeding or failing with
/// `EPERM` (exists, not ours) both count as alive.
///
/// node: src/sessions.ts:2097-2114
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // Signal 0 only checks for existence/permission.
    match crate::kill::process(pid, 0) {
        Ok(()) => true,
        Err(error) => error.raw_os_error() == Some(libc::EPERM),
    }
}

/// The sidecar `<name>.pid`, parsed like `parseInt(content.trim(), 10)`.
///
/// node: src/sessions.ts:2076-2084
pub fn read_session_pid(name: &str) -> Option<i32> {
    read_session_pid_at(&pid_path(name))
}

fn read_session_pid_at(path: &Path) -> Option<i32> {
    let content = std::fs::read_to_string(path).ok()?;
    parse_leading_int(content.trim())
}
/// The result of proving a registry record against the live daemon identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DaemonIdentityCheck {
    Match,
    Mismatch,
    Gone,
    Unknown,
}

impl DaemonIdentityCheck {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Mismatch => "mismatch",
            Self::Gone => "gone",
            Self::Unknown => "unknown",
        }
    }
}

static IDENTITY_MATCH: AtomicU64 = AtomicU64::new(0);
static IDENTITY_MISMATCH: AtomicU64 = AtomicU64::new(0);
static IDENTITY_GONE: AtomicU64 = AtomicU64::new(0);
static IDENTITY_UNKNOWN: AtomicU64 = AtomicU64::new(0);

/// Process-local, low-cardinality liveness observations; no per-scan logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonIdentityCounters {
    pub matched: u64,
    pub mismatched: u64,
    pub gone: u64,
    pub unknown: u64,
}

pub fn record_daemon_identity_check(result: DaemonIdentityCheck) {
    let counter = match result {
        DaemonIdentityCheck::Match => &IDENTITY_MATCH,
        DaemonIdentityCheck::Mismatch => &IDENTITY_MISMATCH,
        DaemonIdentityCheck::Gone => &IDENTITY_GONE,
        DaemonIdentityCheck::Unknown => &IDENTITY_UNKNOWN,
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

pub fn daemon_identity_counters() -> DaemonIdentityCounters {
    DaemonIdentityCounters {
        matched: IDENTITY_MATCH.load(Ordering::Relaxed),
        mismatched: IDENTITY_MISMATCH.load(Ordering::Relaxed),
        gone: IDENTITY_GONE.load(Ordering::Relaxed),
        unknown: IDENTITY_UNKNOWN.load(Ordering::Relaxed),
    }
}

/// Kernel boot identity; unavailable or unreadable evidence stays unknown.
pub fn read_boot_id() -> Option<String> {
    current_boot_id()
}

pub(super) fn current_boot_id() -> Option<String> {
    crate::boot::read_boot_identity()
}

fn linux_thread_group(pid: i32) -> std::io::Result<Option<i32>> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    Ok(status
        .lines()
        .find_map(|line| line.strip_prefix("Tgid:")?.trim().parse().ok()))
}

// Caller has already proved Tgid == pid. Keep the common identity path to one
// status read and one stat read; standalone token readers prove the leader too.
fn read_linux_leader_start_token(pid: i32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = stat.get(stat.rfind(')')? + 2..)?;
    let start_time = tail.split_whitespace().nth(19)?;
    Some(format!("linux:{start_time}"))
}

/// Prove a positive process-leader PID, start token, and boot identity.
/// Missing evidence is never liveness or authority to replace a generation.
pub fn check_daemon_identity(pid: i32, metadata: &SessionMetadata) -> DaemonIdentityCheck {
    if pid <= 0 {
        record_daemon_identity_check(DaemonIdentityCheck::Gone);
        return DaemonIdentityCheck::Gone;
    }
    #[cfg(target_os = "linux")]
    let result = match linux_thread_group(pid) {
        Ok(Some(tgid)) if tgid != pid => DaemonIdentityCheck::Mismatch,
        Ok(Some(_)) => check_daemon_identity_with(pid, metadata, read_linux_leader_start_token),
        Err(error) if process_status_is_gone(&error) => DaemonIdentityCheck::Gone,
        _ => DaemonIdentityCheck::Unknown,
    };
    #[cfg(not(target_os = "linux"))]
    let result = match crate::proctable::process(pid) {
        crate::proctable::Answer::NotPresent => DaemonIdentityCheck::Gone,
        crate::proctable::Answer::Unknown(_) => DaemonIdentityCheck::Unknown,
        crate::proctable::Answer::Known(_) => {
            check_daemon_identity_with(pid, metadata, read_process_start_token)
        }
    };
    record_daemon_identity_check(result);
    result
}

/// Identity evidence for explicit stop, signal and remove controls only.
/// `LegacyVerified` is not full daemon identity proof: it must never authorize
/// inventory liveness, generation replacement, launch or retirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlIdentityCheck {
    Match,
    LegacyVerified,
    Mismatch,
    Gone,
    Unknown,
}

/// Keep modern records on the full identity check. Only absent boot evidence
/// permits the pre-boot-ID control policy, never present but invalid evidence.
pub fn check_control_identity(pid: i32, metadata: &SessionMetadata) -> ControlIdentityCheck {
    if metadata.daemon_boot_id.is_some() {
        return match check_daemon_identity(pid, metadata) {
            DaemonIdentityCheck::Match => ControlIdentityCheck::Match,
            DaemonIdentityCheck::Mismatch => ControlIdentityCheck::Mismatch,
            DaemonIdentityCheck::Gone => ControlIdentityCheck::Gone,
            DaemonIdentityCheck::Unknown => ControlIdentityCheck::Unknown,
        };
    }
    if pid <= 0 {
        return ControlIdentityCheck::Gone;
    }
    if metadata.daemon_pid.is_some_and(|recorded| recorded != pid) {
        return ControlIdentityCheck::Mismatch;
    }
    #[cfg(target_os = "linux")]
    match linux_thread_group(pid) {
        Ok(Some(tgid)) if tgid != pid => ControlIdentityCheck::Mismatch,
        Ok(Some(_)) => {
            check_legacy_control_identity_with(pid, metadata, read_linux_leader_start_token)
        }
        Err(error) if process_status_is_gone(&error) => ControlIdentityCheck::Gone,
        _ => ControlIdentityCheck::Unknown,
    }
    #[cfg(not(target_os = "linux"))]
    match crate::proctable::process(pid) {
        crate::proctable::Answer::NotPresent => ControlIdentityCheck::Gone,
        crate::proctable::Answer::Unknown(_) => ControlIdentityCheck::Unknown,
        crate::proctable::Answer::Known(_) => {
            check_legacy_control_identity_with(pid, metadata, read_process_start_token)
        }
    }
}

fn check_legacy_control_identity_with(
    pid: i32,
    metadata: &SessionMetadata,
    live_start_token: impl FnOnce(i32) -> Option<String>,
) -> ControlIdentityCheck {
    match live_start_token(pid) {
        Some(live) => {
            // Do not filter empty recorded tokens: a present token must match.
            if metadata.daemon_start_token().is_some_and(|expected| expected != live) {
                ControlIdentityCheck::Mismatch
            } else {
                ControlIdentityCheck::LegacyVerified
            }
        }
        None => {
            #[cfg(target_os = "linux")]
            match linux_thread_group(pid) {
                Ok(Some(tgid)) if tgid != pid => return ControlIdentityCheck::Mismatch,
                Err(error) if process_status_is_gone(&error) => return ControlIdentityCheck::Gone,
                _ => {}
            }
            #[cfg(not(target_os = "linux"))]
            if !pid_alive(pid) {
                return ControlIdentityCheck::Gone;
            }
            ControlIdentityCheck::Unknown
        }
    }
}

/// A conflicting sidecar may be an unpublished replacement. No single PID
/// from that observation can authorize liveness, replacement or retirement.
pub fn check_record_identity(
    sidecar: Option<i32>,
    metadata: Option<&SessionMetadata>,
) -> DaemonIdentityCheck {
    let recorded = metadata.and_then(|record| record.daemon_pid);
    if matches!((sidecar, recorded), (Some(a), Some(b)) if a != b) {
        record_daemon_identity_check(DaemonIdentityCheck::Unknown);
        return DaemonIdentityCheck::Unknown;
    }
    let Some(pid) = sidecar.or(recorded) else {
        record_daemon_identity_check(DaemonIdentityCheck::Unknown);
        return DaemonIdentityCheck::Unknown;
    };
    match metadata {
        Some(metadata) => check_daemon_identity(pid, metadata),
        None => check_daemon_identity(pid, &SessionMetadata::default()),
    }
}

/// Check an observed record with its companion paths. Legacy metadata with
/// no named PID is Gone only when both PID and socket companions are absent.
/// A malformed, unreadable or non-regular companion never proves absence.
pub fn check_record_identity_in(
    root: &Path,
    name: &str,
    sidecar: Option<i32>,
    metadata: Option<&SessionMetadata>,
) -> DaemonIdentityCheck {
    if sidecar.is_none() && metadata.is_some_and(|record| record.daemon_pid.is_none()) {
        let absent = |path: &Path| {
            std::fs::symlink_metadata(path)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        };
        if absent(&root.join(format!("{name}.pid"))) && absent(&socket_path_in(root, name)) {
            record_daemon_identity_check(DaemonIdentityCheck::Gone);
            return DaemonIdentityCheck::Gone;
        }
    }
    check_record_identity(sidecar, metadata)
}

fn process_status_is_gone(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

fn check_daemon_identity_with(
    pid: i32,
    metadata: &SessionMetadata,
    live_start_token: impl Fn(i32) -> Option<String>,
) -> DaemonIdentityCheck {
    check_daemon_identity_evidence(pid, metadata, live_start_token, current_boot_id)
}

fn check_daemon_identity_evidence(
    pid: i32,
    metadata: &SessionMetadata,
    live_start_token: impl Fn(i32) -> Option<String>,
    live_boot_id: impl FnOnce() -> Option<String>,
) -> DaemonIdentityCheck {
    use DaemonIdentityCheck::{Gone, Match, Mismatch, Unknown};
    if pid <= 0 {
        return Gone;
    }
    if metadata.daemon_pid.is_some_and(|recorded| recorded != pid) {
        return Mismatch;
    }
    let Some(expected) = metadata
        .daemon_start_token()
        .filter(|token| !token.is_empty())
    else {
        #[cfg(not(target_os = "linux"))]
        if !pid_alive(pid) {
            return Gone;
        }
        return Unknown;
    };
    match live_start_token(pid) {
        Some(live) if live != expected => Mismatch,
        Some(_) => {
            // A different readable token disproves ownership on any boot.
            // Only a token match needs complete, stable boot evidence.
            let Some(recorded) = metadata.daemon_boot_id.as_deref().filter(|id| !id.is_empty())
            else {
                return Unknown;
            };
            // Legacy macOS boottime changes with wall-clock steps. It cannot
            // prove either equality or mismatch, including after a UUID upgrade.
            if recorded.starts_with("darwin:{") {
                return Unknown;
            }
            match live_boot_id() {
                Some(live) if live.starts_with("darwin:{") => Unknown,
                Some(live) if live == recorded => Match,
                Some(_) => Mismatch,
                None => Unknown,
            }
        }
        None => {
            #[cfg(target_os = "linux")]
            match linux_thread_group(pid) {
                Ok(Some(tgid)) if tgid != pid => return Mismatch,
                Err(error) if process_status_is_gone(&error) => return Gone,
                _ => {}
            }
            #[cfg(not(target_os = "linux"))]
            if !pid_alive(pid) {
                return Gone;
            }
            Unknown
        }
    }
}

/// The token that proves an OS process identity: Linux `linux:<starttime>`
/// (`/proc/<pid>/stat` field 22), macOS `darwin:<ps -o lstart=>`.
///
/// node: src/recovery.ts:137-156
pub fn read_process_start_token(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    if cfg!(target_os = "linux") {
        if linux_thread_group(pid).ok().flatten()? != pid {
            return None;
        }
        return read_linux_leader_start_token(pid);
    }
    if cfg!(target_os = "macos") {
        // **THE LAST `ps` IN THIS CODEBASE, AND IT STAYS.**
        //
        // Everything else moved to `proctable`, which reads `/proc` on Linux
        // and `proc_pidinfo` on macOS and spawns nothing. This one cannot,
        // because the text it produces is written into session metadata as
        // `recovery.processStartToken` and the Node tool reads it back from
        // the same registry. `ps -o lstart=` output is therefore a contract
        // between two programs, not an implementation detail, and libproc's
        // microsecond start time is a different string for the same process.
        //
        // It is safe where it is: one call per session lookup, never inside a
        // poll loop, and a failure here already means "cannot confirm" rather
        // than "gone". `proctable::LiveIdentity` exists so the cheap identity
        // used by the teardown can never be confused with this one.
        let out = std::process::Command::new("/bin/ps")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        let started = String::from_utf8_lossy(&out.stdout).trim().to_string();
        return (!started.is_empty()).then(|| format!("darwin:{started}"));
    }
    None
}

/// Resolve a daemon only when metadata binds its sidecar (or daemonPid) to
/// the matching live process identity. A raw PID sidecar is never authority.
pub fn read_pid_with(name: &str, metadata: Option<&SessionMetadata>) -> Option<i32> {
    let root = session_dir();
    read_pid_with_in(&root, name, metadata)
}

fn read_pid_with_in(root: &Path, name: &str, metadata: Option<&SessionMetadata>) -> Option<i32> {
    let sidecar = read_session_pid_at(&root.join(format!("{name}.pid")));
    let owned;
    let retained = match metadata {
        Some(m) => m,
        None => {
            owned = read_metadata_at(&root.join(format!("{name}.json")))?;
            &owned
        }
    };
    let pid = sidecar.or(retained.daemon_pid)?;
    (check_record_identity(sidecar, Some(retained)) == DaemonIdentityCheck::Match).then_some(pid)
}

/// Resolve an explicit control target, accepting either full identity proof or
/// the absence-only legacy policy. Never use this for launch or retirement.
/// A conflicting, malformed or unreadable PID companion cannot grant authority.
pub fn read_control_target_with(name: &str, metadata: Option<&SessionMetadata>) -> Option<i32> {
    read_control_target_with_in(&session_dir(), name, metadata)
}

fn read_control_target_with_in(
    root: &Path,
    name: &str,
    metadata: Option<&SessionMetadata>,
) -> Option<i32> {
    let mut scanner = Scanner {
        strict: true,
        full_metadata: true,
        errors: Vec::new(),
    };
    let owned;
    let retained = match metadata {
        Some(metadata) => metadata,
        None => {
            owned = scanner.metadata(&root.join(format!("{name}.json")), false)?;
            &owned
        }
    };
    let sidecar = scanner.pid(&root.join(format!("{name}.pid")), false);
    if !scanner.errors.is_empty()
        || matches!((sidecar, retained.daemon_pid), (Some(a), Some(b)) if a != b)
    {
        return None;
    }
    let pid = sidecar.or(retained.daemon_pid)?;
    matches!(
        check_control_identity(pid, retained),
        ControlIdentityCheck::Match | ControlIdentityCheck::LegacyVerified
    )
    .then_some(pid)
}

/// Resolve the same identity-bound daemon used by inventory readers.
/// Call while holding the lifecycle locks and revalidate before signalling.
pub fn read_signal_target_with(name: &str, metadata: Option<&SessionMetadata>) -> Option<i32> {
    read_pid_with(name, metadata)
}

/// [`read_signal_target_with`] over an explicit registry root and live
/// start-token reader, so the binding rules are provable without `ps`.
#[cfg(test)]
fn read_signal_target_with_in(
    root: &Path,
    name: &str,
    metadata: Option<&SessionMetadata>,
    live_start_token: impl Fn(i32) -> Option<String>,
) -> Option<i32> {
    let owned;
    let retained = match metadata {
        Some(metadata) => metadata,
        None => {
            owned = read_metadata_at(&root.join(format!("{name}.json")))?;
            &owned
        }
    };
    let pid = read_session_pid_at(&root.join(format!("{name}.pid"))).or(retained.daemon_pid)?;
    (check_daemon_identity_with(pid, retained, live_start_token) == DaemonIdentityCheck::Match)
        .then_some(pid)
}

/// [`read_pid_with`] reading the metadata itself when the sidecar is absent.
pub fn read_pid(name: &str) -> Option<i32> {
    read_pid_with(name, None)
}

/// Is `pid` gone for reaping purposes? A zombie counts as exited.
///
/// node: src/sessions.ts:801-817
pub fn has_process_exited_for_reap(pid: i32) -> bool {
    if !pid_alive(pid) {
        return true;
    }
    // One `/proc` read on Linux, one `proc_pidinfo` call on macOS. Neither
    // spawns anything, which is the whole point: this is called from poll
    // loops, and it used to be a `ps` subprocess per iteration.
    match crate::proctable::has_exited(pid) {
        crate::proctable::Answer::Known(exited) => exited,
        crate::proctable::Answer::NotPresent => true,
        // We did not find out. Ask the kernel once more rather than reading
        // our own silence as a death.
        crate::proctable::Answer::Unknown(_) => !pid_alive(pid),
    }
}

/// Read a `ps -o stat=` field. `still_alive` is asked only when the field is
/// empty, and it is a FRESH answer rather than the one taken before `ps` ran.
///
/// **An empty field is two answers and only one of them means the process is
/// gone.** `ps` prints nothing for a pid that has left, and some builds of
/// `ps` print nothing for a state they do not report. The caller has already
/// established that the pid was alive, so silence here is not evidence of
/// death; asking the kernel again is.
///
/// The Node tool reads an empty field as gone (`src/sessions.ts`,
/// `hasProcessExitedForReap`). That is the same code shape and it is wrong in
/// the same way. It went unnoticed because a Mac's own `ps` answers properly;
/// the one in a nix build sandbox prints a blank state for a live process,
/// and this branch then called it reapable. Measured 2026-09-02.
///
/// **The asymmetry decides it.** Saying "not yet" about a process that has
/// gone costs one more poll. Saying "gone" about a process that is running
/// reaps a live session.
///
/// Split out from its caller so the decision can be tested on any platform.
/// The caller only reaches it off Linux, which has `/proc`.
pub(crate) fn reaped_from_ps_state(state: &str, still_alive: impl FnOnce() -> bool) -> bool {
    let state = state.trim();
    if state.starts_with('Z') {
        return true;
    }
    state.is_empty() && !still_alive()
}

#[cfg(test)]
mod ps_state_tests {
    use super::reaped_from_ps_state;

    #[test]
    fn a_zombie_is_reapable() {
        assert!(reaped_from_ps_state("Z", || panic!("must not ask")));
        assert!(reaped_from_ps_state("Z+  ", || panic!("must not ask")));
    }

    #[test]
    fn a_running_state_is_not_reapable() {
        for state in ["S", "S+", "Ss", "R", "I", "U", "T"] {
            assert!(
                !reaped_from_ps_state(state, || panic!("must not ask")),
                "{state} was called reapable"
            );
        }
    }

    /// The one this function exists for: a `ps` that says nothing about a
    /// process that is still there.
    #[test]
    fn a_blank_state_asks_the_kernel_rather_than_assuming_death() {
        assert!(
            !reaped_from_ps_state("", || true),
            "a blank state from a live process was called reapable"
        );
        assert!(
            !reaped_from_ps_state("   \n", || true),
            "a whitespace state from a live process was called reapable"
        );
        assert!(
            reaped_from_ps_state("", || false),
            "a blank state from a process that has really gone must reap"
        );
    }
}

/// Poll every 50 ms until `pid` is gone or `timeout` elapses.
///
/// node: src/sessions.ts:2120-2127
pub fn wait_for_process_exit(pid: i32, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if has_process_exited_for_reap(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    has_process_exited_for_reap(pid)
}

/// Probe every socket concurrently under one shared deadline. Sockets that
/// have not answered by the deadline are absent from the result (read as
/// unreachable).
///
/// A connect that succeeds is `true`; one that fails (ENOENT, ECONNREFUSED,
/// anything else) is `false`. A listener whose accept queue is full answers
/// neither: Linux reports EAGAIN, which is retried until the deadline, so a
/// wedged daemon ends up absent, as a blocking connect left pending would.
/// All probes run on the calling thread as non-blocking connects in one
/// poll(2) set; a probe never outlives this call.
///
/// node: src/sessions.ts:2129-2175
pub fn probe_sockets_within_budget(paths: &[PathBuf], budget: Duration) -> HashMap<PathBuf, bool> {
    probe_socket_outcomes(paths, budget, |result| result.is_ok())
}

fn probe_socket_outcomes<T>(
    paths: &[PathBuf],
    budget: Duration,
    answer: impl Fn(std::io::Result<()>) -> T,
) -> HashMap<PathBuf, T> {
    enum Probe {
        /// Connect due at the instant held: the first attempt, or
        /// `RETRY_TICK` after a full accept queue.
        Retry(Instant),
        InProgress(UnixStream),
        Answered,
    }
    let mut results = HashMap::new();
    if paths.is_empty() {
        return results;
    }
    let start = Instant::now();
    let deadline = start + budget;
    let mut probes: Vec<Probe> = paths.iter().map(|_| Probe::Retry(start)).collect();
    let mut polled: Vec<usize> = Vec::with_capacity(paths.len());
    let mut fds: Vec<libc::pollfd> = Vec::with_capacity(paths.len());
    // Every path gets its first connect even under a zero budget; a busy
    // retry runs only while the deadline is still ahead, so a retry that falls
    // due at or after it leaves the path absent.
    let mut first_round = true;
    loop {
        polled.clear();
        fds.clear();
        let now = Instant::now();
        let mut next_retry: Option<Instant> = None;
        for (i, (probe, path)) in probes.iter_mut().zip(paths).enumerate() {
            if let Probe::Retry(due) = *probe
                && due <= now
                && (first_round || now < deadline)
            {
                *probe = match unix_connect::connect(path) {
                    Connect::Connected(_) => {
                        results.insert(path.clone(), answer(Ok(())));
                        Probe::Answered
                    }
                    Connect::Failed(error) => {
                        results.insert(path.clone(), answer(Err(error)));
                        Probe::Answered
                    }
                    Connect::InProgress(stream) => Probe::InProgress(stream),
                    Connect::Busy => Probe::Retry(now + unix_connect::RETRY_TICK),
                };
            }
            if let Probe::Retry(due) = *probe {
                next_retry = Some(next_retry.map_or(due, |at| at.min(due)));
            }
            if let Probe::InProgress(stream) = probe {
                polled.push(i);
                fds.push(libc::pollfd {
                    fd: stream.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                });
            }
        }
        first_round = false;
        if fds.is_empty() && next_retry.is_none() {
            break;
        }
        if !unix_connect::poll_until(&mut fds, deadline, next_retry) {
            break;
        }
        for (pfd, &i) in fds.iter().zip(&polled) {
            if pfd.revents == 0 {
                continue;
            }
            if let Probe::InProgress(stream) = &probes[i] {
                let result = stream
                    .take_error()
                    .and_then(|error| error.map_or(Ok(()), Err));
                results.insert(paths[i].clone(), answer(result));
                probes[i] = Probe::Answered;
            }
        }
    }
    results
}

/// Is a live listener behind `socket_path`? One probe with the default
/// per-socket timeout.
///
/// node: src/sessions.ts:2157-2175
pub fn socket_reachable(socket_path: &Path) -> bool {
    probe_sockets_within_budget(
        std::slice::from_ref(&socket_path.to_path_buf()),
        SOCKET_PROBE_TIMEOUT,
    )
    .get(socket_path)
    .copied()
    .unwrap_or(false)
}

/// One bounded, read-only observation of the registry, sorted by name.
///
/// node: src/sessions.ts:895-1013
pub fn list_sessions() -> Vec<SessionInfo> {
    list_sessions_with(&ListOptions::default())
}

/// A full observation for consumers that restart sessions or compare legacy
/// generation-less records as whole metadata. Ordinary observers should use
/// [`list_sessions`] to avoid reading retained output and environment payloads.
pub fn list_sessions_full() -> Vec<SessionInfo> {
    scan_sessions(&session_dir(), &ListOptions::default(), false, None, true).entries
}

/// [`list_sessions`] with an explicit probe budget.
pub fn list_sessions_with(options: &ListOptions) -> Vec<SessionInfo> {
    let root = session_dir();
    list_sessions_in(&root, options)
}

/// One bounded, read-only observation of `root`, sorted by session name.
pub fn list_sessions_in(root: &Path, options: &ListOptions) -> Vec<SessionInfo> {
    scan_sessions(root, options, false, None, false).entries
}

/// Observe `root` without creating or repairing it, reporting every incomplete
/// observation alongside retained sessions. Only an identity match is live;
/// unknown identity makes inventory incomplete even if its socket answers.
/// Unknown sockets receive bounded transport diagnostics, never liveness
/// authority. Existing metadata and PID files are checked even for omitted
/// sessions. Strict PID validation requires a complete, positive decimal i32;
/// tolerant classification retains the legacy leading-integer parse.
pub fn list_sessions_strict_in(root: &Path, options: &ListOptions) -> Inventory {
    scan_sessions(root, options, true, None, false)
}

/// [`list_sessions_strict_in`] for one session id: that id's companions are
/// read and classified exactly as a whole-root strict scan would classify
/// them, and no other entry is touched.
pub fn observe_session_strict_in(root: &Path, name: &str, options: &ListOptions) -> Inventory {
    scan_sessions(root, options, true, Some(name), false)
}

fn scan_sessions(
    root: &Path,
    options: &ListOptions,
    strict: bool,
    only: Option<&str>,
    full_metadata: bool,
) -> Inventory {
    let mut scanner = Scanner {
        strict,
        full_metadata,
        errors: Vec::new(),
    };
    let dir = match std::fs::read_dir(root) {
        Ok(dir) => dir,
        Err(error) => {
            let kind = if error.kind() == std::io::ErrorKind::NotFound {
                InventoryErrorKind::RootMissing
            } else {
                InventoryErrorKind::RootUnreadable
            };
            scanner.error(root, kind, error);
            return Inventory {
                root: root.to_path_buf(),
                complete: scanner.errors.is_empty(),
                entries: Vec::new(),
                errors: scanner.errors,
            };
        }
    };
    let mut entries = Vec::new();
    for entry in dir {
        match entry {
            Ok(entry) => match entry.file_name().into_string() {
                Ok(name) if !is_tmp_name(&name) => entries.push(name),
                Ok(_) => {}
                Err(_) => scanner.error(
                    &entry.path(),
                    InventoryErrorKind::EntryUnreadable,
                    "entry name is not UTF-8",
                ),
            },
            Err(error) => scanner.error(root, InventoryErrorKind::EntryUnreadable, error),
        }
    }
    if let Some(only) = only {
        let companions = [
            socket_path_in(root, only)
                .file_name()
                .expect("session socket has a filename")
                .to_string_lossy()
                .into_owned(),
            format!("{only}.json"),
            format!("{only}.pid"),
        ];
        entries.retain(|entry| companions.contains(entry));
    }
    entries.retain(|entry| {
        if entry.starts_with('@') && (entry.ends_with(".json") || entry.ends_with(".pid")) {
            scanner.error(
                &root.join(entry),
                InventoryErrorKind::EntryUnreadable,
                "registry companion uses the reserved socket digest namespace",
            );
            false
        } else {
            true
        }
    });
    entries.sort();

    // A bounded transport filename carries no logical id. Resolve it from
    // the registry's id-named companions, never invent an id from the digest.
    let socket_names: HashMap<String, &str> = entries
        .iter()
        .filter_map(|entry| {
            entry.strip_suffix(".json").or_else(|| entry.strip_suffix(".pid"))
        })
        .map(|name| {
            let path = socket_path_in(root, name);
            let filename = path.file_name().expect("session socket has a filename");
            (filename.to_string_lossy().into_owned(), name)
        })
        .collect();
    let sockets: Vec<(&str, &str)> = entries
        .iter()
        .filter_map(|entry| {
            let stem = entry.strip_suffix(".sock")?;
            let name = if let Some(name) = socket_names.get(entry) {
                *name
            } else if stem.starts_with('@') {
                scanner.error(
                    &root.join(entry),
                    InventoryErrorKind::EntryUnreadable,
                    "bounded socket has no id-named registry companion",
                );
                return None;
            } else {
                stem
            };
            if socket_path_in(root, name).file_name() != Some(std::ffi::OsStr::new(entry)) {
                scanner.error(
                    &root.join(entry),
                    InventoryErrorKind::EntryUnreadable,
                    "socket filename is not the canonical address for its logical id",
                );
                return None;
            }
            Some((name, entry.as_str()))
        })
        .collect();
    let socket_ids: BTreeSet<&str> = sockets.iter().map(|(name, _)| *name).collect();

    // Read each companion once; strict also validates pid-only records.
    let names: BTreeSet<&str> = entries
        .iter()
        .filter_map(|entry| {
            entry
                .strip_suffix(".json")
                .or_else(|| strict.then(|| entry.strip_suffix(".pid")).flatten())
        })
        .chain(socket_ids.iter().copied())
        .collect();
    let mut sessions = Vec::new();
    let mut uncertain_sockets = Vec::new();
    for name in names {
        let metadata_name = format!("{name}.json");
        let pid_name = format!("{name}.pid");
        let metadata = scanner.metadata(
            &root.join(&metadata_name),
            entries.binary_search(&metadata_name).is_ok(),
        );
        let has_socket = socket_ids.contains(name);
        // Avoid opening a possibly blocking PID special file during a tolerant
        // listing when there is no usable record or socket.
        if !strict && metadata.is_none() && !has_socket {
            continue;
        }
        let pid_errors_before = scanner.errors.len();
        let sidecar = scanner.pid(
            &root.join(&pid_name),
            entries.binary_search(&pid_name).is_ok(),
        );
        let pid = sidecar.or_else(|| metadata.as_ref().and_then(|record| record.daemon_pid));
        if metadata.is_none() && !has_socket {
            // A valid orphan PID is not replacement authority. Preserve the
            // existing companion-error convention when the PID read itself
            // already failed; otherwise explicitly surface unknown identity.
            if strict
                && sidecar.is_some()
                && scanner.errors.len() == pid_errors_before
                && check_record_identity(sidecar, None) == DaemonIdentityCheck::Unknown
            {
                scanner.error(
                    &root.join(&pid_name),
                    InventoryErrorKind::IdentityUnknown,
                    "orphan PID has no daemon identity record",
                );
            }
            continue;
        }
        let identity = check_record_identity_in(root, name, sidecar, metadata.as_ref());
        if identity == DaemonIdentityCheck::Unknown {
            scanner.error(
                &root.join(&metadata_name),
                InventoryErrorKind::IdentityUnknown,
                "cannot prove daemon leader, start token and boot identity",
            );
            if strict && has_socket {
                uncertain_sockets.push(socket_path_in(root, name));
            }
        }
        // Unknown is not a loss observation. Retain the named PID without
        // turning old pid/socket evidence into signal or cleanup authority.
        let retained = matches!(identity, DaemonIdentityCheck::Match | DaemonIdentityCheck::Unknown);
        let status = if metadata
            .as_ref()
            .is_some_and(|record| record.has_exited() || record.exit_code.is_some())
        {
            SessionStatus::Exited
        } else if retained {
            SessionStatus::Running
        } else {
            SessionStatus::Vanished
        };
        // Socket reachability is transport evidence, never process identity.
        // In particular, do not connect to rescue a known mismatched PID.
        sessions.push(SessionInfo {
            name: name.to_string(),
            socket_path: socket_path_in(root, name),
            pid: if retained { pid } else { None },
            status,
            identity,
            metadata,
        });
    }
    let diagnostics =
        probe_socket_outcomes(&uncertain_sockets, options.socket_probe_budget, |result| {
            result
        });
    for path in uncertain_sockets {
        match diagnostics.get(&path) {
            None => scanner.error(
                &path,
                InventoryErrorKind::ProbeTimeout,
                "socket diagnostic deadline elapsed",
            ),
            Some(Err(error))
                if !matches!(
                    error.raw_os_error(),
                    Some(libc::ENOENT) | Some(libc::ECONNREFUSED)
                ) =>
            {
                scanner.error(&path, InventoryErrorKind::EntryUnreadable, error);
            }
            _ => {}
        }
    }

    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    Inventory {
        root: root.to_path_buf(),
        complete: scanner.errors.is_empty(),
        entries: sessions,
        errors: scanner.errors,
    }
}

/// Look a session up by its immutable id only.
///
/// node: src/sessions.ts:1345-1347
pub fn get_session_by_name(name: &str) -> Option<SessionInfo> {
    scan_sessions(&session_dir(), &ListOptions::default(), false, Some(name), true)
        .entries.into_iter().next()
}

/// The text `get_session` fails with when a display name matches several
/// sessions.
pub fn ambiguous_reference_message(reference: &str, ids: &[String]) -> String {
    let mut ids = ids.to_vec();
    ids.sort();
    format!(
        "Session reference \"{reference}\" is ambiguous. Matching stable session IDs:\n{}\nUse a stable session ID instead.",
        ids.iter()
            .map(|id| format!("  {id}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// Resolve `reference` by exact id first, then by a unique display name.
/// Several display-name matches fail closed with the id list.
///
/// node: src/sessions.ts:1351-1363
pub fn get_session(reference: &str) -> Result<Option<SessionInfo>, String> {
    let sessions = list_sessions_full();
    if let Some(s) = sessions.iter().find(|s| s.name == reference) {
        return Ok(Some(s.clone()));
    }
    let mut by_display: Vec<SessionInfo> = sessions
        .into_iter()
        .filter(|s| s.display_name() == Some(reference))
        .collect();
    if by_display.len() <= 1 {
        return Ok(by_display.pop());
    }
    let ids: Vec<String> = by_display.into_iter().map(|s| s.name).collect();
    Err(ambiguous_reference_message(reference, &ids))
}

/// Every immutable id currently claimed by a live or retained session.
///
/// node: src/sessions.ts:1366-1368
pub fn all_session_names() -> BTreeSet<String> {
    list_sessions().into_iter().map(|s| s.name).collect()
}

/// Resolve a reference (id or unique display name) to a stable id; `None`
/// when nothing matches or the display name is ambiguous. Prefer
/// [`get_session`] where the ambiguity text matters.
pub fn resolve_ref(reference: &str) -> Option<String> {
    get_session(reference).ok().flatten().map(|s| s.name)
}

/// Does `<name>.json` exist?
pub fn session_exists(name: &str) -> bool {
    metadata_path(name).exists()
}

#[cfg(test)]
mod signal_target_tests {
    use super::*;

    const PID: i32 = 4242;
    const TOKEN: &str = "test:live-generation";

    /// The daemon at [`PID`] is live with start token [`TOKEN`]; every other
    /// pid has no readable identity. Keeps the binding rules independent of
    /// whether the host (or build sandbox) can run `ps`.
    fn live(pid: i32) -> Option<String> {
        (pid == PID).then(|| TOKEN.to_string())
    }

    fn bound(token: Option<&str>) -> SessionMetadata {
        SessionMetadata {
            daemon_pid: Some(PID),
            daemon_start_token: token.map(str::to_string),
            daemon_boot_id: read_boot_id(),
            ..Default::default()
        }
    }

    #[test]
    fn destructive_target_requires_matching_live_start_identity() {
        let root = std::env::temp_dir().join(format!("pty-signal-target-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let target = |metadata: &SessionMetadata| {
            read_signal_target_with_in(&root, "session", Some(metadata), live)
        };

        // Without a sidecar, only the lock-held start token can bind the pid.
        assert_eq!(target(&bound(None)), None, "unbound pid is never authority");
        assert_eq!(target(&bound(Some(TOKEN))), Some(PID));
        assert_eq!(
            target(&bound(Some("test:previous-generation"))),
            None,
            "a reused pid with another start token is not the daemon"
        );
        let unreadable = SessionMetadata {
            daemon_pid: Some(PID + 1),
            daemon_start_token: Some(TOKEN.to_string()),
            daemon_boot_id: read_boot_id(),
            ..Default::default()
        };
        assert_eq!(
            target(&unreadable),
            None,
            "unconfirmable identity fails closed"
        );

        // An agreeing sidecar does not replace the token check.
        std::fs::write(root.join("session.pid"), PID.to_string()).unwrap();
        assert_eq!(target(&bound(None)), None);
        assert_eq!(target(&bound(Some(TOKEN))), Some(PID));

        // A disagreeing sidecar vetoes even an exact token match.
        std::fs::write(root.join("session.pid"), (PID + 1).to_string()).unwrap();
        assert_eq!(target(&bound(Some(TOKEN))), None);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod identity_observation_tests {
    use super::*;

    #[test]
    fn unreadable_start_token_is_unknown_not_absent_or_live() {
        let pid = std::process::id() as i32;
        let metadata = SessionMetadata {
            daemon_pid: Some(pid),
            daemon_start_token: Some("recorded:token".into()),
            daemon_boot_id: read_boot_id(),
            ..Default::default()
        };
        assert_eq!(
            check_daemon_identity_with(pid, &metadata, |_| None),
            DaemonIdentityCheck::Unknown,
        );
    }

    #[test]
    fn b3_stale_token_disproves_owner_before_missing_or_unreadable_boot() {
        let pid = std::process::id() as i32;
        for recorded_boot in [None, Some("linux:recorded"), Some("darwin:{ sec = 1, usec = 0 }")] {
            let metadata = SessionMetadata {
                daemon_pid: Some(pid),
                daemon_start_token: Some("old-token".into()),
                daemon_boot_id: recorded_boot.map(str::to_string),
                ..Default::default()
            };
            assert_eq!(
                check_daemon_identity_evidence(pid, &metadata, |_| Some("new-token".into()), || {
                    panic!("token mismatch must not require readable boot evidence")
                }),
                DaemonIdentityCheck::Mismatch,
            );
        }
    }

    #[test]
    fn b3_matching_token_requires_complete_stable_boot_evidence() {
        let pid = std::process::id() as i32;
        for (recorded_boot, current_boot, expected) in [
            (None, Some("linux:current"), DaemonIdentityCheck::Unknown),
            (Some("linux:current"), None, DaemonIdentityCheck::Unknown),
            (Some("darwin:{ sec = 1, usec = 0 }"), Some("darwin:uuid"), DaemonIdentityCheck::Unknown),
            (Some("darwin:uuid"), Some("darwin:{ sec = 1, usec = 0 }"), DaemonIdentityCheck::Unknown),
            (Some("darwin:uuid"), Some("darwin:uuid"), DaemonIdentityCheck::Match),
        ] {
            let metadata = SessionMetadata {
                daemon_pid: Some(pid),
                daemon_start_token: Some("same-token".into()),
                daemon_boot_id: recorded_boot.map(str::to_string),
                ..Default::default()
            };
            assert_eq!(
                check_daemon_identity_evidence(
                    pid, &metadata, |_| Some("same-token".into()),
                    || current_boot.map(str::to_string),
                ),
                expected,
            );
        }
    }

    #[test]
    fn linux_status_esrch_is_exit_evidence_not_unknown() {
        assert!(process_status_is_gone(&std::io::Error::from_raw_os_error(libc::ESRCH)));
        assert!(process_status_is_gone(&std::io::Error::from_raw_os_error(libc::ENOENT)));
        assert!(!process_status_is_gone(&std::io::Error::from_raw_os_error(libc::EACCES)));
    }
}

#[cfg(test)]
mod control_identity_tests {
    use super::*;

    fn legacy_record() -> SessionMetadata {
        let pid = std::process::id() as i32;
        // The old producer published daemonPid and optionally a recovery token,
        // but no daemonBootId. No timestamp or migration marker is necessary.
        serde_json::from_value(serde_json::json!({
            "name": "agent/example-legacy-control",
            "daemonPid": pid
        }))
        .unwrap()
    }

    #[test]
    fn legacy_matching_token_is_control_only_not_inventory_match() {
        let pid = std::process::id() as i32;
        let mut metadata = legacy_record();
        let token = read_process_start_token(pid).expect("test process has a readable start token");
        metadata.recovery = Some(serde_json::json!({ "processStartToken": token }));
        assert_eq!(
            check_control_identity(pid, &metadata),
            ControlIdentityCheck::LegacyVerified,
        );
        assert_eq!(check_daemon_identity(pid, &metadata), DaemonIdentityCheck::Unknown);
    }

    #[test]
    fn legacy_mismatched_or_empty_recorded_token_is_not_authority() {
        let pid = std::process::id() as i32;
        for token in ["test:another-process", ""] {
            let mut metadata = legacy_record();
            metadata.daemon_start_token = Some(token.into());
            assert_eq!(check_control_identity(pid, &metadata), ControlIdentityCheck::Mismatch);
        }
    }

    #[test]
    fn legacy_no_token_fixture_preserves_pre_boot_control_policy() {
        let pid = std::process::id() as i32;
        let metadata = legacy_record();
        assert_eq!(metadata.daemon_boot_id, None);
        assert_eq!(metadata.daemon_start_token(), None);
        assert_eq!(
            check_control_identity(pid, &metadata),
            ControlIdentityCheck::LegacyVerified,
        );
        assert_eq!(check_daemon_identity(pid, &metadata), DaemonIdentityCheck::Unknown);
        assert_eq!(
            check_legacy_control_identity_with(pid, &metadata, |_| None),
            ControlIdentityCheck::Unknown,
            "an unreadable live process is not legacy verification",
        );
    }

    #[test]
    fn legacy_gone_and_conflicting_recorded_pids_are_refused() {
        let mut metadata = legacy_record();
        for pid in [0, -1, i32::MAX] {
            metadata.daemon_pid = Some(pid);
            assert_eq!(check_control_identity(pid, &metadata), ControlIdentityCheck::Gone);
        }
        let pid = std::process::id() as i32;
        metadata.daemon_pid = Some(pid + 1);
        assert_eq!(check_control_identity(pid, &metadata), ControlIdentityCheck::Mismatch);
    }

    #[test]
    fn present_boot_evidence_never_falls_back_to_legacy() {
        let pid = std::process::id() as i32;
        let mut metadata = legacy_record();
        metadata.daemon_start_token =
            Some(read_process_start_token(pid).expect("test process has a readable start token"));
        for boot in ["", "test:wrong-boot", "darwin:{ sec = 1, usec = 0 }"] {
            metadata.daemon_boot_id = Some(boot.into());
            let strict = check_daemon_identity(pid, &metadata);
            let expected = match strict {
                DaemonIdentityCheck::Mismatch => ControlIdentityCheck::Mismatch,
                DaemonIdentityCheck::Unknown => ControlIdentityCheck::Unknown,
                other => panic!("invalid boot evidence unexpectedly proved identity: {other:?}"),
            };
            assert_eq!(check_control_identity(pid, &metadata), expected);
        }
        if let Some(boot) = read_boot_id() {
            metadata.daemon_boot_id = Some(boot);
            assert_eq!(check_control_identity(pid, &metadata), ControlIdentityCheck::Match);
        }
    }

    #[test]
    fn control_target_requires_consistent_readable_pid_companions() {
        let root = std::env::temp_dir().join(format!(
            "pty-control-target-{}-{}",
            std::process::id(),
            super::super::atomic::random_hex16(),
        ));
        std::fs::create_dir_all(root.join("agent")).unwrap();
        let name = "agent/example-legacy-control";
        let pid = std::process::id() as i32;
        let metadata = legacy_record();
        let target = || read_control_target_with_in(&root, name, Some(&metadata));
        assert_eq!(target(), Some(pid), "recorded PID can select the legacy leader");
        let path = root.join(format!("{name}.pid"));
        std::fs::write(&path, pid.to_string()).unwrap();
        assert_eq!(target(), Some(pid));
        std::fs::write(&path, (pid + 1).to_string()).unwrap();
        assert_eq!(target(), None, "conflicting sidecar vetoes a live recorded PID");
        std::fs::write(&path, "not-a-pid").unwrap();
        assert_eq!(target(), None, "malformed is not absent");
        std::fs::write(&path, pid.to_string()).unwrap();
        let mut sidecar_only = metadata.clone();
        sidecar_only.daemon_pid = None;
        assert_eq!(read_control_target_with_in(&root, name, Some(&sidecar_only)), Some(pid));
        assert_eq!(read_control_target_with_in(&root, name, None), None, "metadata is required");
        std::fs::write(
            root.join(format!("{name}.json")),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
        assert_eq!(read_control_target_with_in(&root, name, None), Some(pid));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(read_control_target_with_in(&root, name, Some(&sidecar_only)), None);
        std::fs::remove_dir_all(root).unwrap();
    }
}
