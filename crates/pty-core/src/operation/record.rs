//! The operation record, `<root>/operations/<h>.json`, and its lock,
//! `<root>/operations/<h>.lock`.
//!
//! The record is created once, exclusively, and then only ever moved forward
//! by compare-and-swap: every write names the revision it read and the writer
//! it is, under `flock(LOCK_EX)` on the lock file. Holding the lock never
//! grants writing by itself; the record's `writer` does. Nothing under `<h>`
//! is ever deleted, so an `operationId` can never be launched twice.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::proctable::{Answer, ProcTable};
use crate::registry;

/// The only schema version 1 writes and reads.
pub const RECORD_SCHEMA: &str = "pty.launch-operation.v1";

/// One process, exactly: a pid is never enough on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessId {
    pub pid: u32,
    /// The process table's start identity (`linux:<starttime>` or
    /// `darwin:<sec>.<usec>`).
    pub start_token: String,
    pub boot_id: String,
    pub uid: u32,
}

impl ProcessId {
    /// This process.
    pub fn current() -> Result<ProcessId, String> {
        Self::of(std::process::id())
    }

    /// A live process this one can see, read once from the process table.
    pub fn of(pid: u32) -> Result<ProcessId, String> {
        let boot_id = crate::boot::read_boot_identity()
            .ok_or_else(|| "the host boot identity is unavailable".to_string())?;
        let start_token = match crate::proctable::process(pid as i32) {
            Answer::Known(row) => row
                .identity
                .map(|identity| identity.as_str().to_string())
                .ok_or_else(|| format!("process {pid} has no readable start identity"))?,
            Answer::NotPresent => return Err(format!("process {pid} is not running")),
            Answer::Unknown(_) => return Err(format!("process {pid} could not be read")),
        };
        Ok(ProcessId {
            pid,
            start_token,
            boot_id,
            // SAFETY: getuid(2) has no preconditions.
            uid: unsafe { libc::getuid() },
        })
    }

    /// Is this exact process gone, by `table`? A pid alone never answers:
    /// another start token or another boot is another process, a zombie has
    /// exited, and only a complete table can say a pid is absent. A process
    /// still exiting is unknown: it runs no user code, but a system call
    /// already under way in it may still finish, so it is not yet gone.
    pub fn liveness(&self, table: &ProcTable, boot_id: Option<&str>) -> Liveness {
        // A pid and a start token mean something only within one boot. With
        // no current boot identity nothing here can be told apart.
        let Some(boot_id) = boot_id else {
            return Liveness::Unknown("boot-identity-unavailable");
        };
        if boot_id != self.boot_id {
            return Liveness::Gone;
        }
        match table.row(self.pid as i32) {
            Answer::Known(row) => match &row.identity {
                Some(identity) if identity.as_str() != self.start_token => Liveness::Gone,
                Some(_) if row.is_zombie() => Liveness::Gone,
                Some(_) if row.is_exiting() => Liveness::Unknown("exiting"),
                Some(_) => Liveness::Live,
                None => Liveness::Unknown("start-identity-unreadable"),
            },
            Answer::NotPresent if table.is_complete() => Liveness::Gone,
            Answer::NotPresent => Liveness::Unknown("process-table-incomplete"),
            Answer::Unknown(_) => Liveness::Unknown("process-table-unreadable"),
        }
    }
}

impl ProcessId {
    /// [`ProcessId::liveness`] for this one pid, read now.
    pub fn liveness_now(&self, boot_id: Option<&str>) -> Liveness {
        self.liveness(&Self::table_of(self.pid), boot_id)
    }

    /// After this process's exit was observed (see
    /// [`super::exact::ExitWatch`]): its liveness once the table shows the
    /// zombie or no process, read again for at most `cap` until it does.
    ///
    /// **The observed exit is the fence and the zombie is the proof.** On
    /// macOS the table says the process is still exiting for a moment after
    /// the kernel's exit event; a row still exiting when `cap` runs out
    /// answers `Unknown("exiting")`, never gone.
    pub fn liveness_after_exit(&self, boot_id: Option<&str>, cap: Duration) -> Liveness {
        self.liveness_after_exit_with(boot_id, cap, || Self::table_of(self.pid))
    }

    fn liveness_after_exit_with(
        &self,
        boot_id: Option<&str>,
        cap: Duration,
        mut read: impl FnMut() -> ProcTable,
    ) -> Liveness {
        let deadline = Instant::now() + cap;
        loop {
            let liveness = self.liveness(&read(), boot_id);
            if liveness == Liveness::Gone || Instant::now() >= deadline {
                return liveness;
            }
            std::thread::sleep(AFTER_EXIT_POLL);
        }
    }

    fn table_of(pid: u32) -> ProcTable {
        match crate::proctable::process(pid as i32) {
            Answer::Known(row) => ProcTable::from_rows(vec![row]),
            Answer::NotPresent => ProcTable::from_rows(Vec::new()),
            Answer::Unknown(_) => ProcTable::unreadable(),
        }
    }
}

/// How long [`ProcessId::liveness_after_exit`] waits, at most, for the table
/// to show an observed exit. Measured on macOS 27, the table had caught up
/// with every exit event within a millisecond; this is a hard cap, and the
/// wait ends as soon as the table shows the zombie.
pub const AFTER_EXIT_CAP: Duration = Duration::from_millis(10);

/// The pause between two of those reads.
const AFTER_EXIT_POLL: Duration = Duration::from_micros(200);

/// What a recorded identity's process is doing now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Live,
    Gone,
    Unknown(&'static str),
}

/// Who may write the record next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WriterRole {
    None,
    Launcher,
    Producer,
    Recovery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Writer {
    pub role: WriterRole,
    pub id: Option<ProcessId>,
}

impl Writer {
    pub fn none() -> Writer {
        Writer {
            role: WriterRole::None,
            id: None,
        }
    }

    pub fn of(role: WriterRole, id: ProcessId) -> Writer {
        Writer { role, id: Some(id) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Reserved,
    Pending,
    Cancelled,
    Launched,
    Transferred,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Reserved => "reserved",
            State::Pending => "pending",
            State::Cancelled => "cancelled",
            State::Launched => "launched",
            State::Transferred => "transferred",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Reserved,
    Claimed,
    DaemonForking,
    DaemonGated,
    ChildForking,
    ChildPreparing,
    Held,
}

/// Why the operation was decided the way it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cause {
    Cancel,
    OwnerEof,
    Malformed,
    Occupied,
    PredecessorMismatch,
    Withdrawn,
    ProducerLost,
    Launch,
    Refused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    pub cause: Cause,
    pub at: String,
    pub request_id: Option<String>,
    pub observed: Option<String>,
}

impl Decision {
    pub fn now(cause: Cause, request_id: Option<String>, observed: Option<String>) -> Decision {
        Decision {
            cause,
            at: registry::now_iso8601(),
            request_id,
            observed,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Participants {
    pub launcher: Option<ProcessId>,
    pub producer: Option<ProcessId>,
    pub child: Option<ProcessId>,
}

/// The incarnation a launch created. `id` is today's incarnation text,
/// `<daemon-pid>:<createdAt>`, kept exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Incarnation {
    pub id: String,
    pub runtime_id: String,
    pub generation: String,
    pub daemon: ProcessId,
    pub child: Option<ProcessId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RetirementPhase {
    Requested,
    Verified,
    Incomplete,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Retirement {
    pub request_id: String,
    pub phase: RetirementPhase,
    pub evidence: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transfer {
    pub request_id: String,
    pub committed_at: String,
    /// `true` only means the release byte was written.
    pub released: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationRecord {
    pub schema: String,
    pub revision: u64,
    pub updated_at: String,
    pub operation_id: String,
    pub envelope: Value,
    pub envelope_sha256: String,
    pub writer: Writer,
    pub state: State,
    pub phase: Phase,
    pub decision: Option<Decision>,
    pub participants: Participants,
    pub incarnation: Option<Incarnation>,
    pub retirement: Option<Retirement>,
    pub transfer: Option<Transfer>,
}

impl OperationRecord {
    /// A fresh reservation for `envelope`.
    pub fn reserved(envelope: &super::Envelope) -> OperationRecord {
        OperationRecord {
            schema: RECORD_SCHEMA.to_string(),
            revision: 1,
            updated_at: registry::now_iso8601(),
            operation_id: envelope.operation_id.clone(),
            envelope: envelope.value.clone(),
            envelope_sha256: envelope.sha256.clone(),
            writer: Writer::none(),
            state: State::Reserved,
            phase: Phase::Reserved,
            decision: None,
            participants: Participants::default(),
            incarnation: None,
            retirement: None,
            transfer: None,
        }
    }

    /// The invariants every write of this protocol keeps. A record that
    /// breaks one was not written by it, and nothing may act on it.
    pub fn check(&self) -> Result<(), String> {
        let fail = |what: &str| Err(format!("inconsistent record: {what}"));
        if (self.writer.role == WriterRole::None) != self.writer.id.is_none() {
            return fail("writer role and id disagree");
        }
        let cause = self.decision.as_ref().map(|decision| decision.cause);
        let retirement = self.retirement.as_ref().map(|retirement| retirement.phase);
        let requested = retirement == Some(RetirementPhase::Requested);
        let terminal = matches!(
            retirement,
            Some(RetirementPhase::Verified | RetirementPhase::Incomplete)
        );
        match self.writer.role {
            WriterRole::Recovery if !requested => {
                return fail("a recovery writer without a requested retirement");
            }
            WriterRole::None if requested => {
                return fail("a requested retirement without a writer");
            }
            _ => {}
        }
        if terminal && self.writer.role != WriterRole::None {
            return fail("a finished retirement still has a writer");
        }
        let ok = match self.state {
            State::Reserved => {
                self.phase == Phase::Reserved
                    && cause.is_none()
                    && self.incarnation.is_none()
                    && self.transfer.is_none()
                    && self.participants == Participants::default()
                    && matches!(self.writer.role, WriterRole::None | WriterRole::Recovery)
                    && !terminal
            }
            State::Pending => {
                matches!(
                    self.phase,
                    Phase::Claimed | Phase::DaemonForking | Phase::DaemonGated
                ) && cause.is_none()
                    && self.incarnation.is_none()
                    && self.transfer.is_none()
                    && self.participants.launcher.is_some()
                    && self.writer.role != WriterRole::None
                    && !terminal
            }
            State::Cancelled => {
                cause.is_some_and(|cause| cause != Cause::Launch)
                    && self.incarnation.is_none()
                    && self.transfer.is_none()
                    && self.writer.role == WriterRole::None
                    && !requested
            }
            State::Launched => {
                cause == Some(Cause::Launch)
                    && self.incarnation.is_some()
                    && self.transfer.is_none()
                    && matches!(
                        self.phase,
                        Phase::DaemonGated
                            | Phase::ChildForking
                            | Phase::ChildPreparing
                            | Phase::Held
                    )
                    && (self.writer.role != WriterRole::None || terminal)
                    && self.writer.role != WriterRole::Launcher
            }
            State::Transferred => {
                cause == Some(Cause::Launch)
                    && self.incarnation.is_some()
                    && self.transfer.is_some()
                    && self.phase == Phase::Held
                    && retirement.is_none()
                    && matches!(self.writer.role, WriterRole::Producer | WriterRole::None)
            }
        };
        if ok {
            Ok(())
        } else {
            fail("state, phase, writer and fields disagree")
        }
    }

    /// The exact incarnation text, when the operation has launched.
    pub fn incarnation_id(&self) -> Option<&str> {
        self.incarnation.as_ref().map(|inc| inc.id.as_str())
    }
}

/// The root a record lives under, as the verdicts describe it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RootIdentity {
    pub path: String,
    pub dev: u64,
    pub ino: u64,
}

/// `<root>/operations`.
#[derive(Debug, Clone)]
pub struct OperationStore {
    root: PathBuf,
}

/// Why a record could not be read.
#[derive(Debug)]
pub enum ReadError {
    Io(PathBuf, io::Error),
    Malformed(PathBuf, String),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Io(path, error) => write!(f, "{}: {error}", path.display()),
            ReadError::Malformed(path, error) => write!(f, "{}: {error}", path.display()),
        }
    }
}

/// Why a compare-and-swap write was refused.
#[derive(Debug)]
pub enum WriteError {
    /// The record changed since it was read, or the caller is not its writer.
    Stale {
        observed: Box<OperationRecord>,
    },
    Missing,
    Read(ReadError),
    Io(io::Error),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::Stale { observed } => write!(
                f,
                "stale write: the record is at revision {} with writer {:?}",
                observed.revision, observed.writer.role
            ),
            WriteError::Missing => write!(f, "the operation record is missing"),
            WriteError::Read(error) => write!(f, "{error}"),
            WriteError::Io(error) => write!(f, "{error}"),
        }
    }
}

/// Why an exclusive create did not happen.
#[derive(Debug)]
pub enum CreateError {
    Exists,
    Io(io::Error),
}

impl OperationStore {
    /// The store under the registry root this process resolves.
    pub fn current() -> OperationStore {
        OperationStore::at(registry::session_dir())
    }

    pub fn at(root: impl Into<PathBuf>) -> OperationStore {
        OperationStore { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn dir(&self) -> PathBuf {
        self.root.join("operations")
    }

    pub fn record_path(&self, digest: &str) -> PathBuf {
        self.dir().join(format!("{digest}.json"))
    }

    pub fn lock_path(&self, digest: &str) -> PathBuf {
        self.dir().join(format!("{digest}.lock"))
    }

    /// The root's device and inode, so a verdict says exactly which root it
    /// describes.
    pub fn root_identity(&self) -> Option<RootIdentity> {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(&self.root).ok()?;
        Some(RootIdentity {
            path: self.root.to_string_lossy().into_owned(),
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }

    /// Create `<root>/operations` (0700) when it is missing.
    pub fn ensure_dir(&self) -> io::Result<()> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.root)?;
        match std::fs::DirBuilder::new().mode(0o700).create(self.dir()) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let meta = std::fs::metadata(self.dir())?;
        if !meta.is_dir() {
            return Err(io::Error::other(format!(
                "{} is not a directory",
                self.dir().display()
            )));
        }
        if meta.permissions().mode() & 0o777 != 0o700 {
            std::fs::set_permissions(self.dir(), std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    /// Read the record. `Ok(None)` only when the file does not exist.
    pub fn read(&self, digest: &str) -> Result<Option<OperationRecord>, ReadError> {
        let path = self.record_path(digest);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(ReadError::Io(path, error)),
        };
        let record: OperationRecord = serde_json::from_slice(&bytes)
            .map_err(|error| ReadError::Malformed(path.clone(), error.to_string()))?;
        if record.schema != RECORD_SCHEMA {
            return Err(ReadError::Malformed(
                path,
                format!("unsupported schema {:?}", record.schema),
            ));
        }
        record
            .check()
            .map_err(|error| ReadError::Malformed(path, error))?;
        Ok(Some(record))
    }

    /// Create the record exclusively: temporary file, fsync, `link`, fsync
    /// of the directory. An existing record is never touched.
    pub fn create_exclusive(
        &self,
        digest: &str,
        record: &OperationRecord,
    ) -> Result<(), CreateError> {
        self.ensure_dir().map_err(CreateError::Io)?;
        // The lock exists before the record does, so every later reader can
        // open it without creating anything. Opened like every other lock
        // file descriptor, so this close cannot drop a lock of this process.
        drop(
            LockFile::open(
                &self.lock_path(digest),
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .mode(0o600)
                    .custom_flags(libc::O_CLOEXEC),
            )
            .map_err(CreateError::Io)?,
        );
        let target = self.record_path(digest);
        let tmp = registry::atomic::tmp_path_for(&target);
        let bytes = encode(record).map_err(CreateError::Io)?;
        let result = (|| -> Result<(), CreateError> {
            write_synced(&tmp, &bytes).map_err(CreateError::Io)?;
            match std::fs::hard_link(&tmp, &target) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    return Err(CreateError::Exists);
                }
                Err(error) => return Err(CreateError::Io(error)),
            }
            sync_dir(&self.dir()).map_err(CreateError::Io)
        })();
        let _ = std::fs::remove_file(&tmp);
        result
    }

    /// Move the record from `expected` to `next` by compare-and-swap. The
    /// caller holds [`OperationLock::Exclusive`] and has checked that it is
    /// the writer `expected` names (or that `expected` has no writer, for a
    /// claim). The write is refused as stale when the record on disk is not
    /// exactly `expected`.
    pub fn write_cas(
        &self,
        lock: &OperationLock,
        digest: &str,
        expected: &OperationRecord,
        next: OperationRecord,
    ) -> Result<OperationRecord, WriteError> {
        self.write_cas_as(lock, digest, expected, next, "")
    }

    /// [`OperationStore::write_cas`] for a named transition. Debug builds
    /// fail the write named by `PTY_TEST_RECORD_WRITE_FAIL`, so a test can
    /// prove that no effect follows a write that did not land.
    pub fn write_cas_as(
        &self,
        _lock: &OperationLock,
        digest: &str,
        expected: &OperationRecord,
        mut next: OperationRecord,
        label: &str,
    ) -> Result<OperationRecord, WriteError> {
        #[cfg(debug_assertions)]
        if !label.is_empty()
            && std::env::var("PTY_TEST_RECORD_WRITE_FAIL").is_ok_and(|fail| fail == label)
        {
            return Err(WriteError::Io(io::Error::other(format!(
                "injected write failure at {label}"
            ))));
        }
        #[cfg(not(debug_assertions))]
        let _ = label;
        next.check()
            .map_err(|error| WriteError::Io(io::Error::other(error)))?;
        let current = self
            .read(digest)
            .map_err(WriteError::Read)?
            .ok_or(WriteError::Missing)?;
        if current != *expected {
            return Err(WriteError::Stale {
                observed: Box::new(current),
            });
        }
        next.revision = expected.revision + 1;
        next.updated_at = registry::now_iso8601();
        let target = self.record_path(digest);
        let tmp = registry::atomic::tmp_path_for(&target);
        let bytes = encode(&next).map_err(WriteError::Io)?;
        let result = write_synced(&tmp, &bytes)
            .and_then(|()| std::fs::rename(&tmp, &target))
            .and_then(|()| sync_dir(&self.dir()));
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result.map_err(WriteError::Io)?;
        Ok(next)
    }
}

fn encode(record: &OperationRecord) -> io::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(record).map_err(io::Error::other)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// A POSIX record lock (`fcntl`) on `<h>.lock`. The kernel drops it when the
/// holder dies, which is what lets a reader tell a live writer from a dead
/// one.
///
/// **A record lock, not `flock`.** An `flock` belongs to the open file
/// description, so a forked child holding a copy of the descriptor holds the
/// lock too: a held child stopped between its fork and its descriptor
/// cleanup would keep a dead daemon's lock for ever. A record lock belongs
/// to the process and is never inherited by a fork. The one rule that comes
/// with it: closing ANY descriptor of the file drops every record lock the
/// process holds on it, and the process's own requests never conflict with
/// its own locks. [`LockFile`] keeps that rule for every thread.
#[derive(Debug)]
pub struct OperationLock {
    /// Held for the lock it carries; dropping it releases the lock.
    #[cfg_attr(not(test), allow(dead_code))]
    file: LockFile,
}

/// **One descriptor per lock file per process, opened and closed under one
/// mutex.** Every descriptor this process opens on a lock file, for any
/// purpose (the write lock, a read lock, the holder query, the creation),
/// is in [`OPEN`] from before any other thread could see it until after it
/// is closed. The file is named by its device and inode, so two spellings of
/// one file are one file. The open and its registration, and the close and
/// its removal, each happen with [`OPEN`] held, so no thread can open the
/// file between them. A second open of a registered file is refused: closing
/// that second descriptor would drop the first one's lock.
#[derive(Debug)]
struct LockFile {
    /// `None` only while the descriptor is being closed.
    file: Option<File>,
    key: FileKey,
}

/// A file's device and inode.
type FileKey = (u64, u64);

static OPEN: std::sync::Mutex<Vec<FileKey>> = std::sync::Mutex::new(Vec::new());

fn held_by_this_process() -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "this process has the operation lock file open; a second descriptor would drop its lock",
    )
}

fn file_key(metadata: &std::fs::Metadata) -> FileKey {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

impl LockFile {
    fn open(path: &Path, options: &std::fs::OpenOptions) -> io::Result<LockFile> {
        // A poisoned list may be missing a registration: refuse.
        let mut open = OPEN.lock().map_err(|_| held_by_this_process())?;
        if let Ok(metadata) = std::fs::metadata(path)
            && open.contains(&file_key(&metadata))
        {
            return Err(held_by_this_process());
        }
        let file = options.open(path)?;
        let key = match file.metadata() {
            Ok(metadata) => file_key(&metadata),
            Err(error) => {
                // Which file it is cannot be told, so closing it might drop
                // a lock: it stays open, unused.
                std::mem::forget(file);
                return Err(error);
            }
        };
        if open.contains(&key) {
            // The path came to name a file this process has open between the
            // look and the open. Closing this descriptor would drop that
            // file's lock, so it stays open, unused.
            std::mem::forget(file);
            return Err(held_by_this_process());
        }
        open.push(key);
        Ok(LockFile {
            file: Some(file),
            key,
        })
    }

    fn fd(&self) -> i32 {
        self.file
            .as_ref()
            .expect("a lock file is open until it is dropped")
            .as_raw_fd()
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        // The close and the removal as one step, even after a panic
        // elsewhere: no thread may open the file while it is still listed
        // but closing, or once it is unlisted but not yet closed.
        let mut open = OPEN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        pause_at("closing");
        drop(self.file.take());
        open.retain(|key| *key != self.key);
    }
}

/// A named point a unit test can stop one of its own threads at, to hold a
/// window open while another thread runs.
#[cfg(test)]
mod pause {
    use std::cell::RefCell;
    use std::sync::mpsc::{Receiver, SyncSender};

    thread_local! {
        pub(super) static AT: RefCell<Option<(&'static str, SyncSender<()>, Receiver<()>)>> =
            const { RefCell::new(None) };
    }
}

fn pause_at(point: &'static str) {
    #[cfg(test)]
    pause::AT.with(|at| {
        let armed = at
            .borrow()
            .as_ref()
            .is_some_and(|(name, _, _)| *name == point);
        if armed && let Some((_, reached, release)) = at.borrow_mut().take() {
            let _ = reached.send(());
            let _ = release.recv();
        }
    });
    #[cfg(not(test))]
    let _ = point;
}

impl OperationLock {
    /// The lock `reserve` created. Nothing is created here: a reader that
    /// finds no lock finds no operation.
    fn open(store: &OperationStore, digest: &str, write: bool) -> io::Result<LockFile> {
        LockFile::open(
            &store.lock_path(digest),
            std::fs::OpenOptions::new()
                .read(true)
                .write(write)
                .custom_flags(libc::O_CLOEXEC),
        )
    }

    /// Take the write lock, polling for up to `wait`. `Ok(None)` means
    /// another holder kept it for the whole wait.
    pub fn exclusive(
        store: &OperationStore,
        digest: &str,
        wait: Duration,
    ) -> io::Result<Option<OperationLock>> {
        let file = Self::open(store, digest, true)?;
        let deadline = Instant::now() + wait;
        loop {
            if try_lock(file.fd(), libc::F_WRLCK as libc::c_int)? {
                pause_at("locked");
                return Ok(Some(OperationLock { file }));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Take the read lock without waiting. `Ok(None)` means a writer holds
    /// the write lock: it is alive. It reads and creates nothing.
    pub fn shared_now(store: &OperationStore, digest: &str) -> io::Result<Option<OperationLock>> {
        let file = Self::open(store, digest, false)?;
        if try_lock(file.fd(), libc::F_RDLCK as libc::c_int)? {
            Ok(Some(OperationLock { file }))
        } else {
            Ok(None)
        }
    }

    /// The pid holding the write lock, when one does. A record lock names
    /// its holder (`F_GETLK`), so a reader can say which process is the live
    /// writer even when the record itself no longer names one.
    pub fn holder(store: &OperationStore, digest: &str) -> Option<i32> {
        let file = Self::open(store, digest, false).ok()?;
        // SAFETY: an all-zero flock is a valid value to fill in.
        let mut lock: libc::flock = unsafe { std::mem::zeroed() };
        lock.l_type = libc::F_RDLCK as _;
        lock.l_whence = libc::SEEK_SET as _;
        // SAFETY: `file` owns a valid descriptor; `lock` is a valid flock.
        if unsafe { libc::fcntl(file.fd(), libc::F_GETLK, &mut lock) } != 0 {
            return None;
        }
        (lock.l_type as libc::c_int != libc::F_UNLCK as libc::c_int).then_some(lock.l_pid as i32)
    }

    /// The descriptor, for a test's forked child to probe the lock with.
    #[cfg(test)]
    fn raw_fd(&self) -> i32 {
        self.file.fd()
    }
}

/// `F_SETLK` over the whole file.
fn try_lock(fd: i32, kind: libc::c_int) -> io::Result<bool> {
    loop {
        // SAFETY: an all-zero flock is a valid value to fill in.
        let mut lock: libc::flock = unsafe { std::mem::zeroed() };
        lock.l_type = kind as _;
        lock.l_whence = libc::SEEK_SET as _;
        lock.l_start = 0;
        lock.l_len = 0;
        // SAFETY: `fd` is a lock file's open descriptor; `lock` is a valid flock.
        if unsafe { libc::fcntl(fd, libc::F_SETLK, &lock) } == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => {}
            Some(libc::EAGAIN) | Some(libc::EACCES) => return Ok(false),
            _ => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(state: &str, token: &str) -> crate::proctable::Row {
        crate::proctable::Row {
            pid: 4242,
            ppid: 1,
            pgid: 4242,
            state: state.into(),
            rss_kb: None,
            cpu_percent: None,
            identity: Some(crate::proctable::LiveIdentity::new(token)),
        }
    }

    fn recorded() -> ProcessId {
        ProcessId {
            pid: 4242,
            start_token: "darwin:1790000000.123456".into(),
            boot_id: "boot".into(),
            uid: 501,
        }
    }

    /// **An exiting process is not gone.** It returns to no user code, but a
    /// system call already under way in it may still finish.
    #[test]
    fn an_exiting_process_is_unknown_not_gone() {
        let id = recorded();
        let table = |state| ProcTable::from_rows(vec![row(state, &id.start_token)]);
        assert_eq!(id.liveness(&table("S"), Some("boot")), Liveness::Live);
        assert_eq!(
            id.liveness(&table("E"), Some("boot")),
            Liveness::Unknown("exiting")
        );
        assert_eq!(id.liveness(&table("Z"), Some("boot")), Liveness::Gone);
        // Linux, mid-reap: the row is still there, the process is not.
        assert_eq!(id.liveness(&table("X"), Some("boot")), Liveness::Gone);
    }

    /// After an observed exit the table is read again until it shows the
    /// zombie, for at most the cap. One still exiting past the cap stays
    /// unknown; one that turns into a zombie within it is gone.
    #[test]
    fn an_observed_exit_is_gone_once_the_zombie_shows_within_the_cap() {
        let id = recorded();
        let token = id.start_token.clone();
        let cap = Duration::from_millis(5);

        let started = Instant::now();
        let reads = std::cell::Cell::new(0);
        let still_exiting = id.liveness_after_exit_with(Some("boot"), cap, || {
            reads.set(reads.get() + 1);
            ProcTable::from_rows(vec![row("E", &token)])
        });
        assert_eq!(still_exiting, Liveness::Unknown("exiting"));
        assert!(started.elapsed() >= cap, "it waited out the cap");
        assert!(reads.get() > 1, "and read again in between");

        let reads = std::cell::Cell::new(0);
        let zombie_in_time = id.liveness_after_exit_with(Some("boot"), cap, || {
            reads.set(reads.get() + 1);
            let state = if reads.get() < 3 { "E" } else { "Z" };
            ProcTable::from_rows(vec![row(state, &token)])
        });
        assert_eq!(zombie_in_time, Liveness::Gone);
        assert_eq!(reads.get(), 3, "and stopped reading once it showed");

        let reaped =
            id.liveness_after_exit_with(Some("boot"), cap, || ProcTable::from_rows(Vec::new()));
        assert_eq!(reaped, Liveness::Gone, "a complete table without it");

        let unreadable = id.liveness_after_exit_with(Some("boot"), cap, ProcTable::unreadable);
        assert_eq!(unreadable, Liveness::Unknown("process-table-unreadable"));
    }

    fn store() -> (OperationStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "pty-oprec-{}-{}",
            std::process::id(),
            registry::random_hex16()
        ));
        (OperationStore::at(&dir), dir)
    }

    fn envelope() -> super::super::Envelope {
        super::super::parse_envelope(
            br#"{"type":"envelope","version":1,"operationId":"op","owner":"o","origin":"o","subject":"s","runtimeId":"rt","expectedIncarnation":null,"bindings":{}}"#,
        )
        .unwrap()
    }

    fn someone() -> ProcessId {
        ProcessId {
            pid: 10,
            start_token: "tok:10".into(),
            boot_id: "boot".into(),
            uid: 1,
        }
    }

    fn claim(record: &mut OperationRecord) {
        record.state = State::Pending;
        record.phase = Phase::Claimed;
        record.writer = Writer::of(WriterRole::Launcher, someone());
        record.participants.launcher = Some(someone());
    }

    #[test]
    fn inconsistent_records_are_refused_on_read_and_write() {
        let (store, dir) = store();
        let envelope = envelope();
        let h = envelope.digest();
        let record = OperationRecord::reserved(&envelope);
        store.create_exclusive(&h, &record).unwrap();
        let lock = OperationLock::exclusive(&store, &h, Duration::ZERO)
            .unwrap()
            .unwrap();
        // A role with no identity would let recovery skip the liveness check.
        let mut bad = record.clone();
        claim(&mut bad);
        bad.writer.id = None;
        assert!(bad.check().is_err());
        assert!(store.write_cas(&lock, &h, &record, bad.clone()).is_err());
        // Written behind the store's back, it is refused on read.
        bad.revision = 2;
        std::fs::write(store.record_path(&h), serde_json::to_vec(&bad).unwrap()).unwrap();
        assert!(matches!(store.read(&h), Err(ReadError::Malformed(..))));
        // A launched record must say how it launched.
        let mut launched = record.clone();
        claim(&mut launched);
        launched.state = State::Launched;
        launched.writer = Writer::none();
        assert!(
            launched.check().is_err(),
            "no writer and no finished retirement"
        );
        drop(lock);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_record_is_created_once_and_never_replaced() {
        let (store, dir) = store();
        let envelope = envelope();
        let h = envelope.digest();
        let record = OperationRecord::reserved(&envelope);
        store.create_exclusive(&h, &record).unwrap();
        let first = std::fs::read(store.record_path(&h)).unwrap();
        assert!(matches!(
            store.create_exclusive(&h, &record),
            Err(CreateError::Exists)
        ));
        assert_eq!(std::fs::read(store.record_path(&h)).unwrap(), first);
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&store.dir()), 0o700);
        assert_eq!(mode(&store.record_path(&h)), 0o600);
        assert_eq!(store.read(&h).unwrap().unwrap(), record);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_stale_revision_is_refused_and_writes_nothing() {
        let (store, dir) = store();
        let envelope = envelope();
        let h = envelope.digest();
        let record = OperationRecord::reserved(&envelope);
        store.create_exclusive(&h, &record).unwrap();
        let lock = OperationLock::exclusive(&store, &h, Duration::ZERO)
            .unwrap()
            .unwrap();
        let mut next = record.clone();
        claim(&mut next);
        let written = store.write_cas(&lock, &h, &record, next.clone()).unwrap();
        assert_eq!(written.revision, 2);
        let before = std::fs::read(store.record_path(&h)).unwrap();
        // The same expectation again is now stale.
        match store.write_cas(&lock, &h, &record, next) {
            Err(WriteError::Stale { observed }) => assert_eq!(observed.revision, 2),
            other => panic!("expected a stale refusal, got {other:?}"),
        }
        assert_eq!(std::fs::read(store.record_path(&h)).unwrap(), before);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_writer_in_another_process_excludes_readers_and_a_fork_inherits_nothing() {
        let (store, dir) = store();
        let envelope = envelope();
        let h = envelope.digest();
        assert!(
            OperationLock::shared_now(&store, &h).is_err(),
            "no lock before the reservation"
        );
        assert!(!store.dir().exists(), "and nothing was created looking");
        store
            .create_exclusive(&h, &OperationRecord::reserved(&envelope))
            .unwrap();
        // Another process takes the write lock. Everything it needs is made
        // before the fork; the child only locks, reports and waits.
        let file = OperationLock::open(&store, &h, true).unwrap();
        let fd = file.fd();
        let mut pipe = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        // SAFETY: the child calls only fcntl, write and pause.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            unsafe {
                let mut lock: libc::flock = std::mem::zeroed();
                lock.l_type = libc::F_WRLCK as _;
                lock.l_whence = libc::SEEK_SET as _;
                let ok = libc::fcntl(fd, libc::F_SETLK, &lock) == 0;
                libc::write(pipe[1], [ok as u8].as_ptr().cast(), 1);
                libc::pause();
                libc::_exit(0);
            }
        }
        let mut byte = [0u8; 1];
        assert_eq!(
            unsafe { libc::read(pipe[0], byte.as_mut_ptr().cast(), 1) },
            1
        );
        assert_eq!(byte[0], 1, "the other process took the lock");
        // This process's own descriptor is no part of that lock; while it is
        // open, this process may open the file for nothing else.
        drop(file);
        assert!(OperationLock::shared_now(&store, &h).unwrap().is_none());
        assert!(
            OperationLock::exclusive(&store, &h, Duration::from_millis(20))
                .unwrap()
                .is_none()
        );
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
            libc::close(pipe[0]);
            libc::close(pipe[1]);
        }
        // Its death released it.
        let held = OperationLock::exclusive(&store, &h, Duration::ZERO)
            .unwrap()
            .expect("free once the holder died");
        // The holder may not open the lock file again to observe it: closing
        // that descriptor would drop its own lock.
        assert!(OperationLock::shared_now(&store, &h).is_err());
        assert!(OperationLock::holder(&store, &h).is_none());
        assert!(OperationLock::exclusive(&store, &h, Duration::ZERO).is_err());
        assert!(
            OperationLock::exclusive(&store, &h, Duration::ZERO).is_err(),
            "and the refusal did not release it"
        );
        // A fork of the holder does not hold it: the child sees it taken.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            unsafe {
                let mut lock: libc::flock = std::mem::zeroed();
                lock.l_type = libc::F_WRLCK as _;
                lock.l_whence = libc::SEEK_SET as _;
                let busy = libc::fcntl(held.raw_fd(), libc::F_SETLK, &lock) != 0;
                libc::_exit(if busy { 0 } else { 1 });
            }
        }
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert_eq!(
            libc::WEXITSTATUS(status),
            0,
            "a forked child must not inherit the parent's lock"
        );
        drop(held);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Can another process take the write lock now? A forked child tries.
    /// It calls only `open`, `fcntl` and `_exit`, so the fork is safe from a
    /// test with other threads running.
    fn free_to_another_process(path: &Path) -> bool {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: the child calls only async-signal-safe functions.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            unsafe {
                let fd = libc::open(path.as_ptr(), libc::O_RDWR);
                if fd < 0 {
                    libc::_exit(2);
                }
                let mut lock: libc::flock = std::mem::zeroed();
                lock.l_type = libc::F_WRLCK as _;
                lock.l_whence = libc::SEEK_SET as _;
                libc::_exit(if libc::fcntl(fd, libc::F_SETLK, &lock) == 0 {
                    1
                } else {
                    0
                });
            }
        }
        let mut status = 0;
        // SAFETY: waiting for the child forked above.
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        match libc::WEXITSTATUS(status) {
            0 => false,
            1 => true,
            other => panic!("the probe could not open the lock file ({other})"),
        }
    }

    fn reserved() -> (OperationStore, PathBuf, String) {
        let (store, dir) = store();
        let envelope = envelope();
        let h = envelope.digest();
        store
            .create_exclusive(&h, &OperationRecord::reserved(&envelope))
            .unwrap();
        (store, dir, h)
    }

    /// Stop the calling thread at `point` until the returned sender sends;
    /// the receiver hears when it got there.
    fn arm(
        point: &'static str,
    ) -> (
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::SyncSender<()>,
    ) {
        let (reached_tx, reached) = std::sync::mpsc::sync_channel(1);
        let (release, release_rx) = std::sync::mpsc::sync_channel(1);
        pause::AT.with(|at| *at.borrow_mut() = Some((point, reached_tx, release_rx)));
        (reached, release)
    }

    /// **The write lock is registered before it is taken.** Thread A is
    /// stopped holding the fresh lock, before `exclusive` returns. Every
    /// other way into the file from this process is refused meanwhile, and
    /// none of them drops A's lock.
    #[test]
    fn no_thread_can_open_a_lock_file_between_its_lock_and_its_return() {
        let (store, dir, h) = reserved();
        let (to_a, from_test) = std::sync::mpsc::sync_channel(1);
        let a = std::thread::spawn({
            let (dir, h) = (dir.clone(), h.clone());
            move || {
                let (reached, release) = arm("locked");
                to_a.send((reached, release)).unwrap();
                OperationLock::exclusive(&OperationStore::at(&dir), &h, Duration::ZERO)
                    .unwrap()
                    .unwrap()
            }
        });
        let (reached, release) = from_test.recv().unwrap();
        reached.recv().unwrap();
        let path = store.lock_path(&h);
        assert!(OperationLock::holder(&store, &h).is_none());
        assert!(OperationLock::shared_now(&store, &h).is_err());
        assert!(OperationLock::exclusive(&store, &h, Duration::ZERO).is_err());
        assert!(!free_to_another_process(&path), "A still holds its lock");
        release.send(()).unwrap();
        let lock = a.join().unwrap();
        assert!(!free_to_another_process(&path));
        drop(lock);
        assert!(free_to_another_process(&path), "dropped, it is released");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// **A descriptor is closed and unregistered as one step.** Thread A is
    /// stopped in the middle of dropping its lock. Thread B asks for the
    /// lock meanwhile, and waits: had it opened the file, A's close would
    /// drop the lock B then took. The fixed order passes in every
    /// interleaving; the 500 ms only gives the old order time to fail.
    #[test]
    fn no_thread_can_open_a_lock_file_while_its_descriptor_closes() {
        let (store, dir, h) = reserved();
        let lock = OperationLock::exclusive(&store, &h, Duration::ZERO)
            .unwrap()
            .unwrap();
        let (to_test, from_a) = std::sync::mpsc::sync_channel(1);
        let a = std::thread::spawn(move || {
            let (reached, release) = arm("closing");
            to_test.send((reached, release)).unwrap();
            drop(lock);
        });
        let (reached, release) = from_a.recv().unwrap();
        reached.recv().unwrap();
        let (done_tx, done) = std::sync::mpsc::sync_channel(1);
        let b = std::thread::spawn({
            let (dir, h) = (dir.clone(), h.clone());
            move || {
                let got =
                    OperationLock::exclusive(&OperationStore::at(&dir), &h, Duration::from_secs(5));
                let _ = done_tx.send(());
                got.unwrap().expect("free once A has closed")
            }
        });
        assert!(
            done.recv_timeout(Duration::from_millis(500)).is_err(),
            "B must wait for A's close to finish"
        );
        release.send(()).unwrap();
        a.join().unwrap();
        let held = b.join().unwrap();
        assert!(
            !free_to_another_process(&store.lock_path(&h)),
            "A's close did not take B's lock with it"
        );
        drop(held);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A read lock is dropped by a stray close exactly as a write lock is,
    /// so its descriptor is registered the same way.
    #[test]
    fn a_read_lock_is_kept_like_the_write_lock() {
        let (store, dir, h) = reserved();
        let path = store.lock_path(&h);
        let reader = OperationLock::shared_now(&store, &h).unwrap().unwrap();
        assert!(OperationLock::holder(&store, &h).is_none());
        assert!(OperationLock::shared_now(&store, &h).is_err());
        assert!(
            !free_to_another_process(&path),
            "the read lock still excludes a writer"
        );
        drop(reader);
        assert!(free_to_another_process(&path));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The registry names a file by device and inode: a symlinked root and a
    /// hard link are the same lock file.
    #[test]
    fn two_spellings_of_one_lock_file_are_one_file() {
        let (store, dir, h) = reserved();
        let alias_root = dir.with_extension("alias");
        std::os::unix::fs::symlink(&dir, &alias_root).unwrap();
        let alias = OperationStore::at(&alias_root);
        assert_ne!(alias.lock_path(&h), store.lock_path(&h));
        let linked = dir.join("linked.lock");
        std::fs::hard_link(store.lock_path(&h), &linked).unwrap();

        let lock = OperationLock::exclusive(&store, &h, Duration::ZERO)
            .unwrap()
            .unwrap();
        assert!(OperationLock::holder(&alias, &h).is_none());
        assert!(OperationLock::shared_now(&alias, &h).is_err());
        assert!(
            LockFile::open(&linked, std::fs::OpenOptions::new().read(true)).is_err(),
            "nor through a hard link"
        );
        assert!(!free_to_another_process(&store.lock_path(&h)));
        drop(lock);
        assert!(OperationLock::shared_now(&alias, &h).unwrap().is_some());
        std::fs::remove_file(alias_root).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_recorded_identity_is_gone_only_on_evidence() {
        use crate::proctable::table_from_shape;
        let id = ProcessId {
            pid: 10,
            start_token: "tok:10".into(),
            boot_id: "boot".into(),
            uid: 1,
        };
        let live = table_from_shape("10 1 10 S tok:10\n");
        assert_eq!(id.liveness(&live, Some("boot")), Liveness::Live);
        // Same pid, another start: reused.
        let reused = table_from_shape("10 1 10 S tok:other\n");
        assert_eq!(id.liveness(&reused, Some("boot")), Liveness::Gone);
        let zombie = table_from_shape("10 1 10 Z tok:10\n");
        assert_eq!(id.liveness(&zombie, Some("boot")), Liveness::Gone);
        let absent = table_from_shape("11 1 11 S\n");
        assert_eq!(id.liveness(&absent, Some("boot")), Liveness::Gone);
        assert!(matches!(
            id.liveness(&absent.clone().marked_incomplete(), Some("boot")),
            Liveness::Unknown(_)
        ));
        assert!(matches!(
            id.liveness(&ProcTable::unreadable(), Some("boot")),
            Liveness::Unknown(_)
        ));
        // Another boot: whatever has this pid now is not this process.
        assert_eq!(id.liveness(&live, Some("other-boot")), Liveness::Gone);
        // No current boot identity: a per-boot pid and token prove nothing.
        assert_eq!(
            id.liveness(&live, None),
            Liveness::Unknown("boot-identity-unavailable")
        );
        assert_eq!(
            id.liveness(&absent, None),
            Liveness::Unknown("boot-identity-unavailable")
        );
    }
}
