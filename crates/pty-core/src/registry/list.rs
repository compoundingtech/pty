//! Read-only observation of the registry: `list_sessions` and the liveness
//! rules behind it, reference resolution, and the process helpers the
//! lifecycle commands share. Nothing here creates, unlinks or repairs.
//!
//! node: src/sessions.ts:183-199, 801-817, 895-1013, 1345-1370, 2076-2175

use std::collections::{BTreeSet, HashMap};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::atomic::is_tmp_name;
use super::lock::parse_leading_int;
use super::metadata::{SessionMetadata, read_metadata_at};
use super::root::{metadata_path, pid_path, session_dir};
use crate::unix_connect::{self, Connect};

/// `running` / `exited` / `vanished`.
///
/// node: src/sessions.ts:183-199
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    /// The daemon process is alive (or its socket is reachable).
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
    /// The daemon pid when it is known to be alive, else `None`.
    pub pid: Option<i32>,
    pub status: SessionStatus,
    /// `None` when `<name>.json` is missing or unreadable (a socket-only
    /// entry).
    pub metadata: Option<SessionMetadata>,
}

impl SessionInfo {
    /// Is the daemon alive?
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

/// Default shared budget for probing sockets whose pid looks dead.
pub const DEFAULT_SOCKET_PROBE_BUDGET: Duration = Duration::from_millis(500);

/// Per-socket connect timeout inside the probe.
pub const SOCKET_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Knobs for [`list_sessions_with`].
#[derive(Debug, Clone)]
pub struct ListOptions {
    /// One deadline for every socket probe of this listing.
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
        match std::fs::read(path) {
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
        let result = serde_json::from_slice::<serde_json::Value>(&bytes).and_then(|value| {
            if !value.is_object() {
                return Err(<serde_json::Error as serde::de::Error>::custom(
                    "metadata must be a JSON object",
                ));
            }
            serde_json::from_value(value)
        });
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

/// Is a process with `pid` alive? `kill(pid, 0)` succeeding or failing with
/// `EPERM` (exists, not ours) both count as alive.
///
/// node: src/sessions.ts:2097-2114
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks for existence/permission.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
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

/// The token that proves an OS process identity: Linux `linux:<starttime>`
/// (`/proc/<pid>/stat` field 22), macOS `darwin:<ps -o lstart=>`.
///
/// node: src/recovery.ts:137-156
pub fn read_process_start_token(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    if cfg!(target_os = "linux") {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let tail = &stat[stat.rfind(')')? + 2..];
        let start_time = tail.split_whitespace().nth(19)?;
        return Some(format!("linux:{start_time}"));
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
        let out = std::process::Command::new("ps")
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

/// The pid to judge a session by: the sidecar pid file, else the record's
/// `daemonPid` only when `recovery.processStartToken` still matches the
/// live process.
///
/// node: src/sessions.ts:2086-2095
pub fn read_pid_with(name: &str, metadata: Option<&SessionMetadata>) -> Option<i32> {
    let root = session_dir();
    read_pid_with_in(&root, name, metadata)
}

fn read_pid_with_in(root: &Path, name: &str, metadata: Option<&SessionMetadata>) -> Option<i32> {
    if let Some(pid) = read_session_pid_at(&root.join(format!("{name}.pid"))) {
        return Some(pid);
    }
    let owned;
    let retained = match metadata {
        Some(m) => m,
        None => {
            owned = read_metadata_at(&root.join(format!("{name}.json")))?;
            &owned
        }
    };
    let daemon_pid = retained.daemon_pid?;
    let token = retained.process_start_token()?;
    (read_process_start_token(daemon_pid).as_deref() == Some(token)).then_some(daemon_pid)
}

/// Resolve a daemon pid only when lock-held metadata binds it to the exact
/// live OS process. Unlike [`read_pid_with`], this is suitable for destructive
/// signals: an unbound legacy pid sidecar is never authority.
pub fn read_signal_target_with(name: &str, metadata: Option<&SessionMetadata>) -> Option<i32> {
    read_signal_target_with_in(&session_dir(), name, metadata, read_process_start_token)
}

/// [`read_signal_target_with`] over an explicit registry root and live
/// start-token reader, so the binding rules are provable without `ps`.
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
    let daemon_pid = retained.daemon_pid?;
    if let Some(sidecar_pid) = read_session_pid_at(&root.join(format!("{name}.pid")))
        && sidecar_pid != daemon_pid
    {
        return None;
    }
    let token = retained.daemon_start_token()?;
    (live_start_token(daemon_pid).as_deref() == Some(token)).then_some(daemon_pid)
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
                let result = stream.take_error().and_then(|error| error.map_or(Ok(()), Err));
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

/// [`list_sessions`] with an explicit probe budget.
pub fn list_sessions_with(options: &ListOptions) -> Vec<SessionInfo> {
    let root = session_dir();
    list_sessions_in(&root, options)
}

/// One bounded, read-only observation of `root`, sorted by session name.
pub fn list_sessions_in(root: &Path, options: &ListOptions) -> Vec<SessionInfo> {
    scan_sessions(root, options, false).entries
}

/// Observe `root` without creating or repairing it, reporting every incomplete
/// observation alongside retained sessions. Definitively absent or refused
/// sockets retain an exited or vanished row; unknown probe failures and
/// timeouts make the inventory incomplete. Optional absent companions are
/// allowed; existing metadata and PID files are checked even when their session
/// is omitted. Strict PID validation requires a complete, positive decimal i32;
/// classification retains the legacy leading-integer parse.
pub fn list_sessions_strict_in(root: &Path, options: &ListOptions) -> Inventory {
    scan_sessions(root, options, true)
}

fn scan_sessions(root: &Path, options: &ListOptions, strict: bool) -> Inventory {
    let mut scanner = Scanner {
        strict,
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
    entries.sort();

    // Read each companion once; strict also validates pid-only records.
    let names: BTreeSet<&str> = entries
        .iter()
        .filter_map(|entry| {
            entry
                .strip_suffix(".sock")
                .or_else(|| entry.strip_suffix(".json"))
                .or_else(|| strict.then(|| entry.strip_suffix(".pid")).flatten())
        })
        .collect();
    let mut records = HashMap::new();
    for name in names {
        let metadata_name = format!("{name}.json");
        let pid_name = format!("{name}.pid");
        let metadata = scanner.metadata(
            &root.join(&metadata_name),
            entries.binary_search(&metadata_name).is_ok(),
        );
        let pid = scanner
            .pid(
                &root.join(&pid_name),
                entries.binary_search(&pid_name).is_ok(),
            )
            .or_else(|| {
                let metadata = metadata.as_ref()?;
                let pid = metadata.daemon_pid?;
                let token = metadata.process_start_token()?;
                (read_process_start_token(pid).as_deref() == Some(token)).then_some(pid)
            });
        records.insert(name, (metadata, pid));
    }

    let mut sessions: Vec<SessionInfo> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    struct Candidate {
        name: String,
        socket_path: PathBuf,
        pid: Option<i32>,
        pid_alive: bool,
        metadata: Option<SessionMetadata>,
    }
    let candidates: Vec<Candidate> = entries
        .iter()
        .filter_map(|e| e.strip_suffix(".sock"))
        .map(|name| {
            let (metadata, pid) = records.remove(name).expect("socket record was observed");
            Candidate {
                name: name.to_string(),
                socket_path: root.join(format!("{name}.sock")),
                pid,
                pid_alive: pid.is_some_and(pid_alive),
                metadata,
            }
        })
        .collect();

    let needs_probe: Vec<PathBuf> = candidates
        .iter()
        .filter(|c| !c.pid_alive)
        .map(|c| c.socket_path.clone())
        .collect();
    let reachability =
        probe_socket_outcomes(&needs_probe, options.socket_probe_budget, |result| result);
    for path in &needs_probe {
        match reachability.get(path) {
            None => scanner.error(
                path,
                InventoryErrorKind::ProbeTimeout,
                "socket probe deadline elapsed",
            ),
            Some(Err(error)) if !socket_definitively_gone(error) => {
                scanner.error(path, InventoryErrorKind::EntryUnreadable, error);
            }
            _ => {}
        }
    }

    for c in candidates {
        seen.insert(c.name.clone());
        let socket_reachable =
            c.pid_alive || reachability.get(&c.socket_path).is_some_and(Result::is_ok);
        let socket_gone = reachability
            .get(&c.socket_path)
            .is_some_and(|result| result.as_ref().is_err_and(|error| socket_definitively_gone(error)));
        if strict && !c.pid_alive && socket_gone {
            let status = if c.metadata.as_ref().is_some_and(|metadata| {
                metadata.has_exited() || metadata.exit_code.is_some()
            }) {
                SessionStatus::Exited
            } else {
                SessionStatus::Vanished
            };
            sessions.push(SessionInfo {
                name: c.name,
                socket_path: c.socket_path,
                pid: None,
                status,
                metadata: c.metadata,
            });
        } else if c.pid_alive || socket_reachable {
            let metadata = c.metadata;
            let status = if metadata.as_ref().is_some_and(SessionMetadata::has_exited) {
                SessionStatus::Exited
            } else {
                SessionStatus::Running
            };
            sessions.push(SessionInfo {
                name: c.name,
                socket_path: c.socket_path,
                pid: c.pid,
                status,
                metadata,
            });
        } else if c.pid.is_some() {
            if let Some(metadata) = c.metadata {
                let vanished = metadata.exited_at.is_none() && metadata.exit_code.is_none();
                sessions.push(SessionInfo {
                    name: c.name,
                    socket_path: c.socket_path,
                    pid: None,
                    status: if vanished {
                        SessionStatus::Vanished
                    } else {
                        SessionStatus::Exited
                    },
                    metadata: Some(metadata),
                });
            }
        } else {
            let metadata = c.metadata;
            let status = if metadata.as_ref().is_some_and(SessionMetadata::has_exited) {
                SessionStatus::Exited
            } else {
                SessionStatus::Running
            };
            sessions.push(SessionInfo {
                name: c.name,
                socket_path: c.socket_path,
                pid: c.pid,
                status,
                metadata,
            });
        }
    }

    for name in entries.iter().filter_map(|e| e.strip_suffix(".json")) {
        if seen.contains(name) {
            continue;
        }
        let (metadata, pid) = records.remove(name).expect("metadata record was observed");
        let Some(metadata) = metadata else {
            continue;
        };
        if let Some(pid) = pid
            && pid_alive(pid)
        {
            sessions.push(SessionInfo {
                name: name.to_string(),
                socket_path: root.join(format!("{name}.sock")),
                pid: Some(pid),
                status: if metadata.has_exited() {
                    SessionStatus::Exited
                } else {
                    SessionStatus::Running
                },
                metadata: Some(metadata),
            });
            continue;
        }
        let vanished = metadata.exited_at.is_none() && metadata.exit_code.is_none();
        sessions.push(SessionInfo {
            name: name.to_string(),
            socket_path: root.join(format!("{name}.sock")),
            pid: None,
            status: if vanished {
                SessionStatus::Vanished
            } else {
                SessionStatus::Exited
            },
            metadata: Some(metadata),
        });
    }

    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    Inventory {
        root: root.to_path_buf(),
        complete: scanner.errors.is_empty(),
        entries: sessions,
        errors: scanner.errors,
    }
}

fn socket_definitively_gone(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ENOENT) | Some(libc::ECONNREFUSED))
}

/// Look a session up by its immutable id only.
///
/// node: src/sessions.ts:1345-1347
pub fn get_session_by_name(name: &str) -> Option<SessionInfo> {
    list_sessions().into_iter().find(|s| s.name == name)
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
    let sessions = list_sessions();
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
