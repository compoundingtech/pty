//! Exact-name registry observation for authority checks.
//!
//! Unlike listing, this module never opens the registry as an iterator. One
//! validated immutable id selects three companions relative to one opened root
//! descriptor. Every positive observation is read twice and bound to one live
//! process identity, so replacement and partial publication are uncertainty,
//! never absence.

use std::ffi::{CString, OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
#[cfg(target_os = "macos")]
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::list::{
    DaemonIdentityCheck, ListOptions, SessionInfo, SessionStatus, check_boot_identity,
    current_boot_id,
};
use super::metadata::SessionMetadata;
use super::names::validate_name_lexical;
use super::root::{socket_filenames, SUN_PATH_MAX};
#[cfg(test)]
use super::root::socket_path_in;
use crate::proctable::{self, Answer, LiveIdentity};

const METADATA_LIMIT: u64 = 1024 * 1024;
const PID_LIMIT: u64 = 64;
const DEFAULT_ATTEMPTS: usize = 3;
const DEFAULT_RETRY_DELAY: Duration = Duration::from_millis(2);
#[cfg(test)]
type ProbeHook = Box<dyn FnOnce()>;

#[cfg(test)]
thread_local! {
    static SOCKET_PROBE_HOOKS: std::cell::RefCell<(Option<ProbeHook>, Option<ProbeHook>)> =
        std::cell::RefCell::new((None, None));
    static SOCKET_PROBE_TRACE: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn run_socket_probe_hook(after_connect: bool) {
    SOCKET_PROBE_HOOKS.with(|hooks| {
        let hook = if after_connect {
            hooks.borrow_mut().1.take()
        } else {
            hooks.borrow_mut().0.take()
        };
        if let Some(hook) = hook {
            hook();
        }
    });
}

#[cfg(test)]
fn trace_socket_probe(event: String) {
    SOCKET_PROBE_TRACE.with(|trace| trace.borrow_mut().push(event));
}

#[cfg(test)]
fn take_socket_probe_trace() -> Vec<String> {
    SOCKET_PROBE_TRACE.with(|trace| std::mem::take(&mut *trace.borrow_mut()))
}

/// One exact-name observation. Only [`Found`](KeyedSession::Found) is
/// authority-bearing; [`Indeterminate`](KeyedSession::Indeterminate) is a
/// retryable refusal, not evidence that a session disappeared.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyedSession {
    Found(SessionInfo),
    Missing,
    Indeterminate(KeyedSessionError),
}

/// Why an exact-name observation could not be proved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyedSessionErrorKind {
    InvalidName,
    RootUnreadable,
    EntryUnreadable,
    EntryUnsafe,
    MetadataMalformed,
    PidMalformed,
    ProcessIdentity,
    SocketProbe,
    Changed,
}

/// A fail-closed exact-name observation error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyedSessionError {
    pub kind: KeyedSessionErrorKind,
    pub path: Option<PathBuf>,
    pub detail: String,
}

impl fmt::Display for KeyedSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(path) = &self.path {
            write!(formatter, "{}: {}", path.display(), self.detail)
        } else {
            formatter.write_str(&self.detail)
        }
    }
}

impl std::error::Error for KeyedSessionError {}

impl KeyedSessionError {
    fn new(kind: KeyedSessionErrorKind, path: Option<PathBuf>, detail: impl Into<String>) -> Self {
        Self {
            kind,
            path,
            detail: detail.into(),
        }
    }

    fn retryable(&self) -> bool {
        matches!(
            self.kind,
            KeyedSessionErrorKind::EntryUnreadable
                | KeyedSessionErrorKind::MetadataMalformed
                | KeyedSessionErrorKind::PidMalformed
                | KeyedSessionErrorKind::ProcessIdentity
                | KeyedSessionErrorKind::SocketProbe
                | KeyedSessionErrorKind::Changed
        )
    }
}

/// Bounded retry and socket-probe policy for [`observe_session_in`].
#[derive(Debug, Clone)]
pub struct KeyedSessionOptions {
    pub socket_probe_budget: Duration,
    pub attempts: usize,
    pub retry_delay: Duration,
}

impl Default for KeyedSessionOptions {
    fn default() -> Self {
        Self {
            socket_probe_budget: ListOptions::default().socket_probe_budget,
            attempts: DEFAULT_ATTEMPTS,
            retry_delay: DEFAULT_RETRY_DELAY,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    device: libc::dev_t,
    inode: libc::ino_t,
    mode: libc::mode_t,
    uid: libc::uid_t,
    size: libc::off_t,
}

impl Stamp {
    fn from(stat: &libc::stat) -> Self {
        Self {
            device: stat.st_dev,
            inode: stat.st_ino,
            mode: stat.st_mode,
            uid: stat.st_uid,
            size: stat.st_size,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Companion {
    stamp: Stamp,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessEvidence {
    live_identity: LiveIdentity,
}

#[derive(Debug)]
struct OpenRoot {
    fd: OwnedFd,
    path: PathBuf,
    pinned_path: PathBuf,

    device: u64,
    inode: u64,
    uid: libc::uid_t,
}

trait KeyedAccess {
    type Root;

    fn open_root(&self, root: &Path) -> Result<Option<Self::Root>, KeyedSessionError>;
    fn read_companion(
        &self,
        root: &Self::Root,
        filename: &OsStr,
        limit: u64,
    ) -> Result<Option<Companion>, KeyedSessionError>;
    fn stat_socket(
        &self,
        root: &Self::Root,
        filename: &OsStr,
    ) -> Result<Option<Stamp>, KeyedSessionError>;
    fn socket_path(
        &self,
        root: &Self::Root,
        filename: &OsStr,
    ) -> Result<PathBuf, KeyedSessionError>;
    fn process(&self, pid: i32) -> Result<Option<ProcessEvidence>, KeyedSessionError>;
    fn boot_id(&self) -> Option<String> {
        current_boot_id()
    }
    fn probe_socket(
        &self,
        root: &Self::Root,
        filename: &OsStr,
        expected: &Stamp,
        budget: Duration,
    ) -> Result<bool, KeyedSessionError>;
    fn sleep(&self, duration: Duration);
}

struct OsAccess;

impl OsAccess {
    fn cstring(
        value: &OsStr,
        kind: KeyedSessionErrorKind,
        path: &Path,
    ) -> Result<CString, KeyedSessionError> {
        CString::new(value.as_bytes()).map_err(|_| {
            KeyedSessionError::new(kind, Some(path.to_path_buf()), "path contains NUL")
        })
    }

    fn fstat(fd: i32, path: &Path) -> Result<libc::stat, KeyedSessionError> {
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut stat) } == 0 {
            Ok(stat)
        } else {
            Err(KeyedSessionError::new(
                KeyedSessionErrorKind::EntryUnreadable,
                Some(path.to_path_buf()),
                io::Error::last_os_error().to_string(),
            ))
        }
    }

    fn path(root: &OpenRoot, filename: &OsStr) -> PathBuf {
        root.path.join(filename)
    }

    fn pinned_path(fd: i32, configured: &Path) -> Result<PathBuf, KeyedSessionError> {
        #[cfg(target_os = "linux")]
        {
            return std::fs::read_link(format!("/proc/self/fd/{fd}")).map_err(|error| {
                KeyedSessionError::new(
                    KeyedSessionErrorKind::RootUnreadable,
                    Some(configured.to_path_buf()),
                    format!("cannot resolve opened registry root: {error}"),
                )
            });
        }
        #[cfg(target_os = "macos")]
        {
            let mut bytes = vec![0u8; libc::PATH_MAX as usize];
            // SAFETY: F_GETPATH writes at most PATH_MAX bytes to this live buffer.
            if unsafe { libc::fcntl(fd, libc::F_GETPATH, bytes.as_mut_ptr()) } < 0 {
                return Err(KeyedSessionError::new(
                    KeyedSessionErrorKind::RootUnreadable,
                    Some(configured.to_path_buf()),
                    format!(
                        "cannot resolve opened registry root: {}",
                        io::Error::last_os_error()
                    ),
                ));
            }
            let length = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
            return Ok(PathBuf::from(OsString::from_vec(bytes[..length].to_vec())));
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            std::fs::canonicalize(configured).map_err(|error| {
                KeyedSessionError::new(
                    KeyedSessionErrorKind::RootUnreadable,
                    Some(configured.to_path_buf()),
                    format!("cannot resolve opened registry root: {error}"),
                )
            })
        }
    }

    fn reporting_root(root: &OpenRoot) -> &Path {
        let configured_is_pinned = std::fs::metadata(&root.path).is_ok_and(|metadata| {
            metadata.dev() == root.device
                && metadata.ino() == root.inode
                && metadata.file_type().is_dir()
        });
        if configured_is_pinned {
            &root.path
        } else {
            &root.pinned_path
        }
    }

    /// A refusal is never absence: only a record with no PID is probed, and on
    /// macOS a full accept queue refuses a live listener too.
    fn probe_result(
        display_path: &Path,
        outcome: Option<&io::Result<()>>,
    ) -> Result<bool, KeyedSessionError> {
        match outcome {
            Some(Ok(())) => Ok(true),
            Some(Err(error)) if error.raw_os_error() == Some(libc::ENOENT) => Ok(false),
            Some(Err(error)) => Err(KeyedSessionError::new(
                KeyedSessionErrorKind::SocketProbe,
                Some(display_path.to_path_buf()),
                error.to_string(),
            )),
            None => Err(KeyedSessionError::new(
                KeyedSessionErrorKind::SocketProbe,
                Some(display_path.to_path_buf()),
                "socket probe deadline elapsed",
            )),
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn open_socket_leaf(
        root: &OpenRoot,
        filename: &OsStr,
        expected: &Stamp,
    ) -> Result<OwnedFd, KeyedSessionError> {
        let path = root.pinned_path.join(filename);
        let name = Self::cstring(filename, KeyedSessionErrorKind::SocketProbe, &path)?;
        #[cfg(target_os = "linux")]
        let flags = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        #[cfg(not(target_os = "linux"))]
        return Err(KeyedSessionError::new(
            KeyedSessionErrorKind::SocketProbe,
            Some(path),
            "this platform cannot pin a registry socket leaf",
        ));
        let raw = unsafe { libc::openat(root.fd.as_raw_fd(), name.as_ptr(), flags) };
        if raw < 0 {
            return Err(KeyedSessionError::new(
                KeyedSessionErrorKind::Changed,
                Some(path),
                format!(
                    "socket leaf changed before the probe: {}",
                    io::Error::last_os_error()
                ),
            ));
        }
        // SAFETY: `openat` returned a new descriptor owned by this call.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let observed = Stamp::from(&Self::fstat(fd.as_raw_fd(), &path)?);
        if &observed != expected {
            return Err(KeyedSessionError::new(
                KeyedSessionErrorKind::Changed,
                Some(path),
                format!("socket leaf changed before the probe: {expected:?} -> {observed:?}"),
            ));
        }
        Ok(fd)
    }

    /// Any entry created, removed or renamed in the pinned root fires this
    /// watch. A socket leaf cannot be swapped and restored without that.
    #[cfg(target_os = "macos")]
    fn watch_directory(fd: i32, path: &Path) -> Result<OwnedFd, KeyedSessionError> {
        let raw = unsafe { libc::kqueue() };
        if raw < 0 {
            return Err(KeyedSessionError::new(
                KeyedSessionErrorKind::SocketProbe,
                Some(path.to_path_buf()),
                io::Error::last_os_error().to_string(),
            ));
        }
        // SAFETY: `kqueue` returned a new descriptor owned by this call.
        let queue = unsafe { OwnedFd::from_raw_fd(raw) };
        let change = libc::kevent {
            ident: fd as libc::uintptr_t,
            filter: libc::EVFILT_VNODE,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: libc::NOTE_DELETE
                | libc::NOTE_WRITE
                | libc::NOTE_EXTEND
                | libc::NOTE_ATTRIB
                | libc::NOTE_LINK
                | libc::NOTE_RENAME
                | libc::NOTE_REVOKE,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        if unsafe {
            libc::kevent(
                queue.as_raw_fd(),
                &change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        } < 0
        {
            return Err(KeyedSessionError::new(
                KeyedSessionErrorKind::SocketProbe,
                Some(path.to_path_buf()),
                format!(
                    "cannot watch pinned registry root: {}",
                    io::Error::last_os_error()
                ),
            ));
        }
        Ok(queue)
    }

    #[cfg(target_os = "macos")]
    fn directory_changed(queue: &OwnedFd, path: &Path) -> Result<bool, KeyedSessionError> {
        let mut event: libc::kevent = unsafe { std::mem::zeroed() };
        let timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let count = unsafe {
            libc::kevent(
                queue.as_raw_fd(),
                std::ptr::null(),
                0,
                &mut event,
                1,
                &timeout,
            )
        };
        if count < 0 {
            return Err(KeyedSessionError::new(
                KeyedSessionErrorKind::SocketProbe,
                Some(path.to_path_buf()),
                format!(
                    "cannot read pinned registry root watch: {}",
                    io::Error::last_os_error()
                ),
            ));
        }
        Ok(count > 0)
    }
}

impl KeyedAccess for OsAccess {
    type Root = OpenRoot;

    fn open_root(&self, root: &Path) -> Result<Option<Self::Root>, KeyedSessionError> {
        let name = Self::cstring(
            root.as_os_str(),
            KeyedSessionErrorKind::RootUnreadable,
            root,
        )?;
        let raw = unsafe {
            libc::open(
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(KeyedSessionError::new(
                KeyedSessionErrorKind::RootUnreadable,
                Some(root.to_path_buf()),
                error.to_string(),
            ));
        }
        // SAFETY: `open` returned a new descriptor owned by this call.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let stat = Self::fstat(fd.as_raw_fd(), root)?;
        if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(KeyedSessionError::new(
                KeyedSessionErrorKind::RootUnreadable,
                Some(root.to_path_buf()),
                "registry root is not a directory",
            ));
        }
        let pinned_path = Self::pinned_path(fd.as_raw_fd(), root)?;
        #[cfg(test)]
        trace_socket_probe(format!(
            "open_root fd={} configured={} pinned={} dev={} ino={} uid={}",
            fd.as_raw_fd(),
            root.display(),
            pinned_path.display(),
            stat.st_dev,
            stat.st_ino,
            stat.st_uid
        ));
        Ok(Some(OpenRoot {
            fd,
            path: root.to_path_buf(),
            pinned_path,

            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
            uid: stat.st_uid,
        }))
    }

    fn read_companion(
        &self,
        root: &Self::Root,
        filename: &OsStr,
        limit: u64,
    ) -> Result<Option<Companion>, KeyedSessionError> {
        let path = Self::path(root, filename);
        let name = Self::cstring(filename, KeyedSessionErrorKind::EntryUnreadable, &path)?;
        let raw = unsafe {
            libc::openat(
                root.fd.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        };
        if raw < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::NotFound {
                return Ok(None);
            }
            let kind = if error.raw_os_error() == Some(libc::ELOOP) {
                KeyedSessionErrorKind::EntryUnsafe
            } else {
                KeyedSessionErrorKind::EntryUnreadable
            };
            return Err(KeyedSessionError::new(kind, Some(path), error.to_string()));
        }
        // SAFETY: `openat` returned a new descriptor owned by this call.
        let mut file = unsafe { File::from_raw_fd(raw) };
        let stat = Self::fstat(file.as_raw_fd(), &path)?;
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG || stat.st_uid != root.uid {
            return Err(KeyedSessionError::new(
                KeyedSessionErrorKind::EntryUnsafe,
                Some(path),
                "registry companion is not an owner-matched regular file",
            ));
        }
        if stat.st_size < 0 || stat.st_size as u64 > limit {
            return Err(KeyedSessionError::new(
                KeyedSessionErrorKind::EntryUnsafe,
                Some(path),
                format!("registry companion exceeds {limit} bytes"),
            ));
        }
        let mut bytes = Vec::with_capacity(stat.st_size as usize);
        file.by_ref()
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| {
                KeyedSessionError::new(
                    KeyedSessionErrorKind::EntryUnreadable,
                    Some(path.clone()),
                    error.to_string(),
                )
            })?;
        if bytes.len() as u64 > limit {
            return Err(KeyedSessionError::new(
                KeyedSessionErrorKind::EntryUnsafe,
                Some(path),
                format!("registry companion exceeds {limit} bytes"),
            ));
        }
        Ok(Some(Companion {
            stamp: Stamp::from(&stat),
            bytes,
        }))
    }

    fn stat_socket(
        &self,
        root: &Self::Root,
        filename: &OsStr,
    ) -> Result<Option<Stamp>, KeyedSessionError> {
        let path = Self::path(root, filename);
        let name = Self::cstring(filename, KeyedSessionErrorKind::EntryUnreadable, &path)?;
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                root.fd.as_raw_fd(),
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(KeyedSessionError::new(
                KeyedSessionErrorKind::EntryUnreadable,
                Some(path),
                error.to_string(),
            ));
        }
        if stat.st_mode & libc::S_IFMT != libc::S_IFSOCK || stat.st_uid != root.uid {
            return Err(KeyedSessionError::new(
                KeyedSessionErrorKind::EntryUnsafe,
                Some(path),
                "registry socket is not an owner-matched socket",
            ));
        }
        Ok(Some(Stamp::from(&stat)))
    }

    fn socket_path(
        &self,
        root: &Self::Root,
        filename: &OsStr,
    ) -> Result<PathBuf, KeyedSessionError> {
        let configured = Self::reporting_root(root).join(filename);
        if configured.as_os_str().len() <= SUN_PATH_MAX {
            return Ok(configured);
        }
        Ok(root.pinned_path.join(filename))
    }

    fn process(&self, pid: i32) -> Result<Option<ProcessEvidence>, KeyedSessionError> {
        match proctable::process(pid) {
            Answer::Known(row) => {
                let identity = row.identity.ok_or_else(|| {
                    KeyedSessionError::new(
                        KeyedSessionErrorKind::ProcessIdentity,
                        None,
                        format!("process {pid} has no readable birth identity"),
                    )
                })?;
                Ok(Some(ProcessEvidence {
                    live_identity: identity,
                }))
            }
            Answer::NotPresent => Ok(None),
            Answer::Unknown(reason) => Err(KeyedSessionError::new(
                KeyedSessionErrorKind::ProcessIdentity,
                None,
                format!("process {pid} identity is unknown: {reason:?}"),
            )),
        }
    }

    // XNU refuses to open a socket node (EOPNOTSUPP) and has no connectat, so
    // macOS cannot pin the leaf as Linux does. It connects by the session's
    // own address and refuses the answer if that address's directory stopped
    // naming the pinned root, or if any root entry or the leaf changed.
    #[cfg(target_os = "macos")]
    fn probe_socket(
        &self,
        root: &Self::Root,
        filename: &OsStr,
        expected: &Stamp,
        budget: Duration,
    ) -> Result<bool, KeyedSessionError> {
        let path = self.socket_path(root, filename)?;
        let watch = Self::watch_directory(root.fd.as_raw_fd(), &path)?;
        let unchanged = |when: &str| -> Result<(), KeyedSessionError> {
            let names_root = path
                .parent()
                .and_then(|parent| std::fs::metadata(parent).ok())
                .is_some_and(|metadata| {
                    metadata.dev() == root.device
                        && metadata.ino() == root.inode
                        && metadata.file_type().is_dir()
                });
            match self.stat_socket(root, filename)? {
                Some(observed) if names_root && &observed == expected => Ok(()),
                observed => Err(KeyedSessionError::new(
                    KeyedSessionErrorKind::Changed,
                    Some(path.clone()),
                    format!(
                        "socket address changed {when} the probe: names pinned root={names_root}; {expected:?} -> {observed:?}"
                    ),
                )),
            }
        };
        unchanged("before")?;
        #[cfg(test)]
        trace_socket_probe(format!(
            "pin_socket root_fd={} root={}:{} leaf={expected:?} path={}",
            root.fd.as_raw_fd(),
            root.device,
            root.inode,
            path.display()
        ));
        #[cfg(test)]
        run_socket_probe_hook(false);
        let outcomes = super::list::probe_socket_results(std::slice::from_ref(&path), budget);
        #[cfg(test)]
        run_socket_probe_hook(true);
        unchanged("during")?;
        if Self::directory_changed(&watch, &path)? {
            return Err(KeyedSessionError::new(
                KeyedSessionErrorKind::Changed,
                Some(path),
                "registry root entries changed during the probe",
            ));
        }
        #[cfg(test)]
        trace_socket_probe(format!("probe_complete outcome={:?}", outcomes.get(&path)));
        Self::probe_result(&path, outcomes.get(&path))
    }

    #[cfg(not(target_os = "macos"))]
    fn probe_socket(
        &self,
        root: &Self::Root,
        filename: &OsStr,
        expected: &Stamp,
        budget: Duration,
    ) -> Result<bool, KeyedSessionError> {
        let display_path = root.pinned_path.join(filename);
        let leaf = Self::open_socket_leaf(root, filename, expected)?;
        let path = PathBuf::from(format!("/proc/self/fd/{}", leaf.as_raw_fd()));
        #[cfg(test)]
        trace_socket_probe(format!(
            "pin_socket root_fd={} leaf_fd={} root={}:{} leaf={expected:?} path={}",
            root.fd.as_raw_fd(),
            leaf.as_raw_fd(),
            root.device,
            root.inode,
            display_path.display()
        ));
        #[cfg(test)]
        run_socket_probe_hook(false);
        let outcomes =
            super::list::probe_socket_results(std::slice::from_ref(&path), budget);
        #[cfg(test)]
        run_socket_probe_hook(true);
        let observed = Stamp::from(&Self::fstat(leaf.as_raw_fd(), &display_path)?);
        if &observed != expected {
            return Err(KeyedSessionError::new(
                KeyedSessionErrorKind::Changed,
                Some(display_path),
                format!("socket leaf changed during the probe: {expected:?} -> {observed:?}"),
            ));
        }
        #[cfg(test)]
        trace_socket_probe(format!(
            "probe_complete leaf_fd={} outcome={:?}",
            leaf.as_raw_fd(),
            outcomes.get(&path)
        ));
        Self::probe_result(&display_path, outcomes.get(&path))
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Capture {
    metadata: Option<Companion>,
    pid: Option<Companion>,
    readable_socket: Option<Stamp>,
    bounded_socket: Option<Stamp>,
}

fn filenames(name: &str) -> (String, String, String, String) {
    let metadata = format!("{name}.json");
    let pid = format!("{name}.pid");
    let (readable_socket, bounded_socket) = socket_filenames(name);
    (metadata, pid, readable_socket, bounded_socket)
}

fn capture<A: KeyedAccess>(
    access: &A,
    root: &A::Root,
    metadata: &OsStr,
    pid: &OsStr,
    readable_socket: &OsStr,
    bounded_socket: &OsStr,
) -> Result<Capture, KeyedSessionError> {
    Ok(Capture {
        metadata: access.read_companion(root, metadata, METADATA_LIMIT)?,
        pid: access.read_companion(root, pid, PID_LIMIT)?,
        readable_socket: access.stat_socket(root, readable_socket)?,
        bounded_socket: access.stat_socket(root, bounded_socket)?,
    })
}

fn selected_socket<A: KeyedAccess>(
    access: &A,
    root: &A::Root,
    capture: &Capture,
    readable: &OsStr,
    bounded: &OsStr,
) -> Result<OsString, KeyedSessionError> {
    if capture.bounded_socket.is_some() {
        return Ok(bounded.to_os_string());
    }
    if capture.readable_socket.is_some() {
        return Ok(readable.to_os_string());
    }
    let readable_path = access.socket_path(root, readable)?;
    Ok(if readable_path.as_os_str().len() <= SUN_PATH_MAX {
        readable.to_os_string()
    } else {
        bounded.to_os_string()
    })
}

fn parse_metadata(
    companion: Option<&Companion>,
    path: &Path,
) -> Result<Option<SessionMetadata>, KeyedSessionError> {
    let Some(companion) = companion else {
        return Ok(None);
    };
    let value: serde_json::Value = serde_json::from_slice(&companion.bytes).map_err(|error| {
        KeyedSessionError::new(
            KeyedSessionErrorKind::MetadataMalformed,
            Some(path.to_path_buf()),
            error.to_string(),
        )
    })?;
    if !value.is_object() {
        return Err(KeyedSessionError::new(
            KeyedSessionErrorKind::MetadataMalformed,
            Some(path.to_path_buf()),
            "metadata must be a JSON object",
        ));
    }
    serde_json::from_value(value).map(Some).map_err(|error| {
        KeyedSessionError::new(
            KeyedSessionErrorKind::MetadataMalformed,
            Some(path.to_path_buf()),
            error.to_string(),
        )
    })
}

fn parse_pid(companion: Option<&Companion>, path: &Path) -> Result<Option<i32>, KeyedSessionError> {
    let Some(companion) = companion else {
        return Ok(None);
    };
    let content = std::str::from_utf8(&companion.bytes).map_err(|error| {
        KeyedSessionError::new(
            KeyedSessionErrorKind::PidMalformed,
            Some(path.to_path_buf()),
            error.to_string(),
        )
    })?;
    let content = content.trim();
    if content.is_empty() || !content.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(KeyedSessionError::new(
            KeyedSessionErrorKind::PidMalformed,
            Some(path.to_path_buf()),
            "expected a positive decimal PID",
        ));
    }
    let pid = content.parse::<i32>().map_err(|error| {
        KeyedSessionError::new(
            KeyedSessionErrorKind::PidMalformed,
            Some(path.to_path_buf()),
            error.to_string(),
        )
    })?;
    if pid <= 0 {
        return Err(KeyedSessionError::new(
            KeyedSessionErrorKind::PidMalformed,
            Some(path.to_path_buf()),
            "expected a positive decimal PID",
        ));
    }
    Ok(Some(pid))
}

fn attempt<A: KeyedAccess>(
    access: &A,
    root_path: &Path,
    root: &A::Root,
    name: &str,
    options: &KeyedSessionOptions,
) -> Result<Option<SessionInfo>, KeyedSessionError> {
    let (metadata_name, pid_name, readable_socket, bounded_socket) = filenames(name);
    let first = capture(
        access,
        root,
        OsStr::new(&metadata_name),
        OsStr::new(&pid_name),
        OsStr::new(&readable_socket),
        OsStr::new(&bounded_socket),
    )?;
    let socket_name = selected_socket(
        access,
        root,
        &first,
        OsStr::new(&readable_socket),
        OsStr::new(&bounded_socket),
    )?;
    let socket_path = access.socket_path(root, &socket_name)?;
    if socket_path.as_os_str().len() > SUN_PATH_MAX {
        return Err(KeyedSessionError::new(
            KeyedSessionErrorKind::InvalidName,
            Some(socket_path),
            format!("session socket address exceeds the {SUN_PATH_MAX}-byte kernel limit"),
        ));
    }
    if first.metadata.is_none()
        && first.pid.is_none()
        && first.readable_socket.is_none()
        && first.bounded_socket.is_none()
    {
        let second = capture(
            access,
            root,
            OsStr::new(&metadata_name),
            OsStr::new(&pid_name),
            OsStr::new(&readable_socket),
            OsStr::new(&bounded_socket),
        )?;
        if first == second {
            return Ok(None);
        }
        return Err(KeyedSessionError::new(
            KeyedSessionErrorKind::Changed,
            Some(root_path.to_path_buf()),
            "session artifacts appeared during observation",
        ));
    }

    let metadata_path = root_path.join(&metadata_name);
    let pid_path = root_path.join(&pid_name);
    let metadata = parse_metadata(first.metadata.as_ref(), &metadata_path)?;
    let sidecar_pid = parse_pid(first.pid.as_ref(), &pid_path)?;
    if let (Some(sidecar), Some(recorded)) =
        (sidecar_pid, metadata.as_ref().and_then(|m| m.daemon_pid))
        && sidecar != recorded
    {
        return Err(KeyedSessionError::new(
            KeyedSessionErrorKind::ProcessIdentity,
            Some(pid_path),
            format!("PID sidecar {sidecar} disagrees with metadata daemon PID {recorded}"),
        ));
    }
    let pid = sidecar_pid.or_else(|| metadata.as_ref().and_then(|m| m.daemon_pid));
    let process_first = match pid {
        Some(pid) => access.process(pid)?,
        None => None,
    };
    // As in list.rs, a PID sidecar with no record or socket is not a session.
    if metadata.is_none() && first.readable_socket.is_none() && first.bounded_socket.is_none() {
        return Err(KeyedSessionError::new(
            KeyedSessionErrorKind::ProcessIdentity,
            Some(root_path.to_path_buf()),
            "present session artifacts do not identify a session",
        ));
    }
    // The registry's identity rule (list.rs): a live leader's start token, then
    // for a match its recorded boot; a positively absent PID is gone; legacy
    // metadata naming no PID is gone only when no PID or socket remains.
    let identity = match (pid, process_first.as_ref()) {
        (Some(pid), Some(process)) => {
            let (record, expected) = metadata
                .as_ref()
                .and_then(|record| Some((record, record.daemon_start_token()?)))
                .ok_or_else(|| {
                    KeyedSessionError::new(
                        KeyedSessionErrorKind::ProcessIdentity,
                        Some(metadata_path.clone()),
                        format!("live process {pid} has no registry start identity"),
                    )
                })?;
            if !process.live_identity.matches_registry_start_token(expected) {
                return Err(KeyedSessionError::new(
                    KeyedSessionErrorKind::ProcessIdentity,
                    Some(metadata_path.clone()),
                    format!("process {pid} birth identity does not match the registry"),
                ));
            }
            check_boot_identity(record, || access.boot_id())
        }
        (Some(_), None) => DaemonIdentityCheck::Gone,
        (None, _)
            if metadata.is_some()
                && first.readable_socket.is_none()
                && first.bounded_socket.is_none() =>
        {
            DaemonIdentityCheck::Gone
        }
        (None, _) => DaemonIdentityCheck::Unknown,
    };

    // Socket reachability is transport evidence, never process identity: a
    // gone or mismatched PID is never rescued by a listener. Only a record
    // naming no PID at all is probed, and the probe can confirm what list
    // reports for it or fail closed, never turn it into a loss.
    let status = if metadata
        .as_ref()
        .is_some_and(|metadata| metadata.has_exited() || metadata.exit_code.is_some())
    {
        SessionStatus::Exited
    } else {
        match identity {
            DaemonIdentityCheck::Match => SessionStatus::Running,
            DaemonIdentityCheck::Mismatch | DaemonIdentityCheck::Gone => SessionStatus::Vanished,
            DaemonIdentityCheck::Unknown if pid.is_some() => SessionStatus::Running,
            DaemonIdentityCheck::Unknown => {
                let expected = if socket_name == OsStr::new(&bounded_socket) {
                    first.bounded_socket.as_ref()
                } else {
                    first.readable_socket.as_ref()
                }
                .expect("a record naming no PID that is not gone has a socket");
                if !access.probe_socket(
                    root,
                    &socket_name,
                    expected,
                    options.socket_probe_budget,
                )? {
                    return Err(KeyedSessionError::new(
                        KeyedSessionErrorKind::Changed,
                        Some(socket_path),
                        "session socket vanished during observation",
                    ));
                }
                SessionStatus::Running
            }
        }
    };
    // As in list.rs, only a retained identity names its PID.
    let observed_pid = matches!(
        identity,
        DaemonIdentityCheck::Match | DaemonIdentityCheck::Unknown
    )
    .then_some(pid)
    .flatten();

    let second = capture(
        access,
        root,
        OsStr::new(&metadata_name),
        OsStr::new(&pid_name),
        OsStr::new(&readable_socket),
        OsStr::new(&bounded_socket),
    )?;
    if first != second {
        return Err(KeyedSessionError::new(
            KeyedSessionErrorKind::Changed,
            Some(root_path.to_path_buf()),
            "session artifacts changed during observation",
        ));
    }
    if let Some(pid) = pid {
        let process_second = access.process(pid)?;
        if process_first != process_second {
            return Err(KeyedSessionError::new(
                KeyedSessionErrorKind::Changed,
                None,
                format!("process {pid} changed during observation"),
            ));
        }
    }
    Ok(Some(SessionInfo {
        name: name.to_owned(),
        socket_path,
        pid: observed_pid,
        status,
        identity,
        metadata,
    }))
}

/// Observe one immutable session id without enumerating the registry.
///
/// A stable absent target is [`Missing`](KeyedSession::Missing). Any unreadable,
/// malformed, unsafe, or changing evidence is a retryable
/// [`Indeterminate`](KeyedSession::Indeterminate); callers must never interpret
/// it as loss.
pub fn observe_session_in(root: &Path, name: &str, options: &KeyedSessionOptions) -> KeyedSession {
    if let Err(detail) = validate_name_lexical(name) {
        return KeyedSession::Indeterminate(KeyedSessionError::new(
            KeyedSessionErrorKind::InvalidName,
            None,
            detail,
        ));
    }
    let access = OsAccess;
    observe_with(&access, root, name, options)
}

fn observe_with<A: KeyedAccess>(
    access: &A,
    root_path: &Path,
    name: &str,
    options: &KeyedSessionOptions,
) -> KeyedSession {
    let root = match access.open_root(root_path) {
        Ok(Some(root)) => root,
        Ok(None) => return KeyedSession::Missing,
        Err(error) => return KeyedSession::Indeterminate(error),
    };
    let attempts = options.attempts.max(1);
    for index in 0..attempts {
        match attempt(access, root_path, &root, name, options) {
            Ok(Some(session)) => return KeyedSession::Found(session),
            Ok(None) => return KeyedSession::Missing,
            Err(error) if error.retryable() && index + 1 < attempts => {
                access.sleep(options.retry_delay);
            }
            Err(error) => return KeyedSession::Indeterminate(error),
        }
    }
    unreachable!("at least one keyed observation attempt runs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;
    use std::os::unix::fs::{symlink, FileTypeExt as _};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Root(PathBuf);

    impl Root {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "pty-keyed-{}-{label}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn metadata(&self, name: &str, value: serde_json::Value) {
            std::fs::write(self.0.join(format!("{name}.json")), value.to_string()).unwrap();
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn live_metadata() -> serde_json::Value {
        let pid = std::process::id() as i32;
        let token = super::super::list::read_process_start_token(pid);
        serde_json::json!({
            "createdAt": "2026-10-16T00:00:00Z",
            "daemonPid": pid,
            "daemonStartToken": token.clone(),
            "recovery": {"processStartToken": token},
            "tags": {"st3.subject": "agent/example"}
        })
    }

    fn fast() -> KeyedSessionOptions {
        KeyedSessionOptions {
            socket_probe_budget: Duration::from_millis(20),
            attempts: 2,
            retry_delay: Duration::ZERO,
        }
    }

    fn live_metadata_on_boot(boot: &str) -> serde_json::Value {
        let mut metadata = live_metadata();
        metadata["daemonBootId"] = serde_json::Value::String(boot.into());
        metadata
    }

    #[test]
    fn stable_fixed_inputs_match_the_full_snapshot_oracle() {
        use DaemonIdentityCheck::{Gone, Match, Mismatch, Unknown};
        let boot = super::super::list::read_boot_id().expect("this host reports a boot identity");
        for (label, value, expected, identity) in [
            ("live", live_metadata(), SessionStatus::Running, Unknown),
            (
                "same-boot",
                live_metadata_on_boot(&boot),
                SessionStatus::Running,
                Match,
            ),
            (
                "other-boot",
                live_metadata_on_boot(&format!("{boot}-other")),
                SessionStatus::Vanished,
                Mismatch,
            ),
            (
                "gone-pid",
                serde_json::json!({"createdAt":"2026-10-16T00:00:00Z","daemonPid":i32::MAX}),
                SessionStatus::Vanished,
                Gone,
            ),
            (
                "exited",
                serde_json::json!({"createdAt":"2026-10-16T00:00:00Z","exitedAt":"2026-10-16T00:01:00Z","exitCode":0}),
                SessionStatus::Exited,
                Gone,
            ),
            (
                "vanished",
                serde_json::json!({"createdAt":"2026-10-16T00:00:00Z"}),
                SessionStatus::Vanished,
                Gone,
            ),
            (
                "exit-code-only",
                serde_json::json!({"createdAt":"2026-10-16T00:00:00Z","exitCode":0}),
                SessionStatus::Exited,
                Gone,
            ),
        ] {
            let root = Root::new(label);
            root.metadata("target", value);
            let keyed = observe_session_in(&root.0, "target", &fast());
            let listed = super::super::list::list_sessions_in(&root.0, &ListOptions::default())
                .into_iter()
                .find(|session| session.name == "target")
                .unwrap();
            let KeyedSession::Found(found) = keyed else {
                panic!("stable {label} target was not found: {keyed:?}");
            };
            assert_eq!(found, listed, "{label}");
            assert_eq!(
                (found.status, found.identity),
                (expected, identity),
                "{label}"
            );
        }
        let root = Root::new("missing");
        assert_eq!(
            observe_session_in(&root.0, "target", &fast()),
            KeyedSession::Missing
        );
    }

    #[test]
    fn stable_socket_inputs_match_the_full_snapshot_oracle() {
        for metadata in [None, Some(live_metadata())] {
            let root = Root::new(if metadata.is_some() {
                "socket-with-metadata"
            } else {
                "socket-only"
            });
            if let Some(metadata) = metadata {
                root.metadata("target", metadata);
                std::fs::write(root.0.join("target.pid"), std::process::id().to_string()).unwrap();
            }
            let listener = UnixListener::bind(socket_path_in(&root.0, "target")).unwrap();
            let keyed = observe_session_in(&root.0, "target", &fast());
            let listed = super::super::list::list_sessions_in(&root.0, &ListOptions::default())
                .into_iter()
                .find(|session| session.name == "target")
                .unwrap();
            let KeyedSession::Found(found) = keyed else {
                panic!("stable socket target was not found: {keyed:?}");
            };
            assert_eq!(found, listed);
            assert_eq!(found.status, SessionStatus::Running);
            drop(listener);
        }
    }

    fn listed(root: &Root) -> SessionInfo {
        super::super::list::list_sessions_in(&root.0, &ListOptions::default())
            .into_iter()
            .find(|session| session.name == "target")
            .unwrap()
    }

    #[test]
    fn a_listener_never_rescues_a_gone_pid() {
        for accepting in [true, false] {
            let root = Root::new(if accepting {
                "gone-listener"
            } else {
                "gone-stale"
            });
            root.metadata(
                "target",
                serde_json::json!({"createdAt":"2026-10-16T00:00:00Z","daemonPid":i32::MAX}),
            );
            let listener = UnixListener::bind(socket_path_in(&root.0, "target")).unwrap();
            let listener = accepting.then_some(listener);
            let _ = take_socket_probe_trace();
            let keyed = observe_session_in(&root.0, "target", &fast());
            let trace = take_socket_probe_trace();
            let KeyedSession::Found(found) = keyed else {
                panic!("gone PID with accepting={accepting} was not found: {keyed:?}");
            };
            assert_eq!(found, listed(&root), "accepting={accepting}");
            assert_eq!(
                (found.status, found.identity),
                (SessionStatus::Vanished, DaemonIdentityCheck::Gone),
                "accepting={accepting}"
            );
            assert!(
                trace.iter().all(|event| !event.starts_with("pin_socket")),
                "a gone PID's socket was probed: {trace:?}"
            );
            drop(listener);
        }
    }

    #[test]
    fn a_record_naming_no_pid_is_probed_but_never_turned_into_a_loss() {
        let root = Root::new("no-pid");
        root.metadata(
            "target",
            serde_json::json!({"createdAt":"2026-10-16T00:00:00Z"}),
        );
        let listener = UnixListener::bind(socket_path_in(&root.0, "target")).unwrap();
        let keyed = observe_session_in(&root.0, "target", &fast());
        let KeyedSession::Found(found) = keyed else {
            panic!("an accepting socket for a record naming no PID was not found: {keyed:?}");
        };
        assert_eq!(found, listed(&root));
        assert_eq!(
            (found.status, found.identity),
            (SessionStatus::Running, DaemonIdentityCheck::Unknown)
        );
        drop(listener);
        // A refusing socket still lists as Running; this observer cannot
        // prove the record either way, so it fails closed.
        assert_eq!(listed(&root).status, SessionStatus::Running);
        let refused = observe_session_in(&root.0, "target", &fast());
        assert!(
            matches!(
                refused,
                KeyedSession::Indeterminate(KeyedSessionError {
                    kind: KeyedSessionErrorKind::SocketProbe,
                    ..
                })
            ),
            "{refused:?}"
        );
    }

    #[test]
    fn operator_configured_symlink_root_is_followed_but_entry_symlinks_are_not() {
        let root = Root::new("symlink-root");
        root.metadata("target", live_metadata());
        let link = root.0.with_extension("configured-link");
        symlink(&root.0, &link).unwrap();
        let KeyedSession::Found(found) = observe_session_in(&link, "target", &fast()) else {
            panic!("the configured symlink root should remain observable")
        };
        assert_eq!(found.socket_path, link.join("target.sock"));
        std::fs::remove_file(link).unwrap();
    }
    #[test]
    fn overlong_configured_symlink_reports_the_usable_pinned_address() {
        let root = Root::new("long-configured-root");
        root.metadata("target", live_metadata());
        let link = std::env::temp_dir().join(format!(
            "pty-keyed-long-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            "x".repeat(SUN_PATH_MAX)
        ));
        symlink(&root.0, &link).unwrap();
        let KeyedSession::Found(found) = observe_session_in(&link, "target", &fast()) else {
            panic!("the long configured symlink root should remain observable")
        };
        assert_eq!(
            found.socket_path,
            std::fs::canonicalize(&root.0)
                .unwrap()
                .join("target.sock")
        );
        assert!(found.socket_path.as_os_str().len() <= SUN_PATH_MAX);
        std::fs::remove_file(link).unwrap();
    }

    #[test]
    fn invalid_names_and_symlinked_companions_fail_closed() {
        let root = Root::new("unsafe");
        for name in ["", ".", "..", "a/b", "a\\b", "nul\0name", &"x".repeat(210)] {
            assert!(matches!(
                observe_session_in(&root.0, name, &fast()),
                KeyedSession::Indeterminate(KeyedSessionError {
                    kind: KeyedSessionErrorKind::InvalidName,
                    ..
                })
            ));
        }
        let outside = root.0.with_extension("outside");
        std::fs::write(&outside, "{}").unwrap();
        for suffix in ["json", "pid", "sock"] {
            let target = root.0.join(format!("target.{suffix}"));
            symlink(&outside, &target).unwrap();
            assert!(matches!(
                observe_session_in(&root.0, "target", &fast()),
                KeyedSession::Indeterminate(KeyedSessionError {
                    kind: KeyedSessionErrorKind::EntryUnsafe,
                    ..
                })
            ));
            std::fs::remove_file(target).unwrap();
        }
        std::fs::remove_file(outside).unwrap();
    }

    #[test]
    fn invalid_socket_parent_is_indeterminate_not_absent() {
        let outcome = Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        assert!(matches!(
            OsAccess::probe_result(Path::new("/pinned/target.sock"), Some(&outcome)),
            Err(KeyedSessionError {
                kind: KeyedSessionErrorKind::SocketProbe,
                ..
            })
        ));
    }
    #[test]
    fn uncorroborated_connection_refusal_is_indeterminate() {
        let refused = Err(io::Error::from_raw_os_error(libc::ECONNREFUSED));
        assert!(matches!(
            OsAccess::probe_result(Path::new("/pinned/target.sock"), Some(&refused)),
            Err(KeyedSessionError {
                kind: KeyedSessionErrorKind::SocketProbe,
                ..
            })
        ));
    }

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    struct Counts {
        roots: usize,
        companions: usize,
        sockets: usize,
        processes: usize,
        probes: usize,
        probe_budget: Duration,
        sleep_time: Duration,
    }

    struct Counted {
        inner: OsAccess,
        counts: RefCell<Counts>,
    }

    impl Counted {
        fn new() -> Self {
            Self {
                inner: OsAccess,
                counts: RefCell::new(Counts::default()),
            }
        }
    }

    impl KeyedAccess for Counted {
        type Root = OpenRoot;

        fn open_root(&self, root: &Path) -> Result<Option<Self::Root>, KeyedSessionError> {
            self.counts.borrow_mut().roots += 1;
            self.inner.open_root(root)
        }
        fn read_companion(
            &self,
            root: &Self::Root,
            filename: &OsStr,
            limit: u64,
        ) -> Result<Option<Companion>, KeyedSessionError> {
            self.counts.borrow_mut().companions += 1;
            self.inner.read_companion(root, filename, limit)
        }
        fn stat_socket(
            &self,
            root: &Self::Root,
            filename: &OsStr,
        ) -> Result<Option<Stamp>, KeyedSessionError> {
            self.counts.borrow_mut().sockets += 1;
            self.inner.stat_socket(root, filename)
        }
        fn socket_path(
            &self,
            root: &Self::Root,
            filename: &OsStr,
        ) -> Result<PathBuf, KeyedSessionError> {
            self.inner.socket_path(root, filename)
        }
        fn process(&self, pid: i32) -> Result<Option<ProcessEvidence>, KeyedSessionError> {
            self.counts.borrow_mut().processes += 1;
            self.inner.process(pid)
        }
        fn probe_socket(
            &self,
            root: &Self::Root,
            filename: &OsStr,
            expected: &Stamp,
            budget: Duration,
        ) -> Result<bool, KeyedSessionError> {
            let mut counts = self.counts.borrow_mut();
            counts.probes += 1;
            counts.probe_budget += budget;
            drop(counts);
            self.inner.probe_socket(root, filename, expected, budget)
        }
        fn sleep(&self, duration: Duration) {
            self.counts.borrow_mut().sleep_time += duration;
            self.inner.sleep(duration);
        }
    }

    struct Mutating {
        inner: OsAccess,
        process_calls: Cell<usize>,
        replacement: RefCell<Option<(PathBuf, Vec<u8>)>>,
        recycle_between_reads: bool,
    }

    impl Mutating {
        fn rewrite_once(path: PathBuf, replacement: Vec<u8>) -> Self {
            Self {
                inner: OsAccess,
                process_calls: Cell::new(0),
                replacement: RefCell::new(Some((path, replacement))),
                recycle_between_reads: false,
            }
        }

        fn recycle_between_reads() -> Self {
            Self {
                inner: OsAccess,
                process_calls: Cell::new(0),
                replacement: RefCell::new(None),
                recycle_between_reads: true,
            }
        }
    }

    impl KeyedAccess for Mutating {
        type Root = OpenRoot;

        fn open_root(&self, root: &Path) -> Result<Option<Self::Root>, KeyedSessionError> {
            self.inner.open_root(root)
        }
        fn read_companion(
            &self,
            root: &Self::Root,
            filename: &OsStr,
            limit: u64,
        ) -> Result<Option<Companion>, KeyedSessionError> {
            self.inner.read_companion(root, filename, limit)
        }
        fn stat_socket(
            &self,
            root: &Self::Root,
            filename: &OsStr,
        ) -> Result<Option<Stamp>, KeyedSessionError> {
            self.inner.stat_socket(root, filename)
        }
        fn socket_path(
            &self,
            root: &Self::Root,
            filename: &OsStr,
        ) -> Result<PathBuf, KeyedSessionError> {
            self.inner.socket_path(root, filename)
        }
        fn process(&self, pid: i32) -> Result<Option<ProcessEvidence>, KeyedSessionError> {
            let call = self.process_calls.get() + 1;
            self.process_calls.set(call);
            let mut observed = self.inner.process(pid)?;
            if call == 1
                && let Some((path, replacement)) = self.replacement.borrow_mut().take()
            {
                std::fs::write(path, replacement).unwrap();
            }
            if self.recycle_between_reads
                && call % 2 == 0
                && let Some(process) = &mut observed
            {
                process.live_identity = LiveIdentity::new("recycled-between-reads");
            }
            Ok(observed)
        }
        fn probe_socket(
            &self,
            root: &Self::Root,
            filename: &OsStr,
            expected: &Stamp,
            budget: Duration,
        ) -> Result<bool, KeyedSessionError> {
            self.inner.probe_socket(root, filename, expected, budget)
        }
        fn sleep(&self, duration: Duration) {
            self.inner.sleep(duration);
        }
    }

    struct RetargetingRoot {
        inner: OsAccess,
        link: PathBuf,
        target: PathBuf,
        retargeted: Cell<bool>,
        retarget_after_open: bool,
    }

    impl RetargetingRoot {
        fn retarget_once(&self) {
            if self.retargeted.replace(true) {
                return;
            }
            std::fs::remove_file(&self.link).unwrap();
            symlink(&self.target, &self.link).unwrap();
        }
    }

    impl KeyedAccess for RetargetingRoot {
        type Root = OpenRoot;

        fn open_root(&self, root: &Path) -> Result<Option<Self::Root>, KeyedSessionError> {
            let opened = self.inner.open_root(root)?;
            if self.retarget_after_open {
                self.retarget_once();
            }
            Ok(opened)
        }
        fn read_companion(
            &self,
            root: &Self::Root,
            filename: &OsStr,
            limit: u64,
        ) -> Result<Option<Companion>, KeyedSessionError> {
            if !self.retarget_after_open {
                self.retarget_once();
            }
            self.inner.read_companion(root, filename, limit)
        }
        fn stat_socket(
            &self,
            root: &Self::Root,
            filename: &OsStr,
        ) -> Result<Option<Stamp>, KeyedSessionError> {
            self.inner.stat_socket(root, filename)
        }
        fn socket_path(
            &self,
            root: &Self::Root,
            filename: &OsStr,
        ) -> Result<PathBuf, KeyedSessionError> {
            self.inner.socket_path(root, filename)
        }
        fn process(&self, pid: i32) -> Result<Option<ProcessEvidence>, KeyedSessionError> {
            self.inner.process(pid)
        }
        fn probe_socket(
            &self,
            root: &Self::Root,
            filename: &OsStr,
            expected: &Stamp,
            budget: Duration,
        ) -> Result<bool, KeyedSessionError> {
            self.inner.probe_socket(root, filename, expected, budget)
        }
        fn sleep(&self, duration: Duration) {
            self.inner.sleep(duration);
        }
    }

    #[test]
    fn retargeted_symlink_root_cannot_borrow_another_roots_listener() {
        let first = Root::new("retarget-first");
        let second = Root::new("retarget-second");
        first.metadata(
            "target",
            serde_json::json!({"createdAt":"2026-10-16T00:00:00Z"}),
        );
        let first_socket = socket_path_in(&first.0, "target");
        let stale = UnixListener::bind(&first_socket).unwrap();
        drop(stale);
        let second_socket = socket_path_in(&second.0, "target");
        let live = UnixListener::bind(&second_socket).unwrap();
        let link = first.0.with_extension("retarget-link");
        symlink(&first.0, &link).unwrap();
        let access = RetargetingRoot {
            inner: OsAccess,
            link: link.clone(),
            target: second.0.clone(),
            retargeted: Cell::new(false),
            retarget_after_open: false,
        };
        let _ = take_socket_probe_trace();
        let observed = observe_with(&access, &link, "target", &fast());
        let trace = take_socket_probe_trace();
        assert!(
            matches!(
                observed,
                KeyedSession::Indeterminate(KeyedSessionError {
                    kind: KeyedSessionErrorKind::SocketProbe,
                    ..
                })
            ),
            "uncorroborated refusal did not fail closed: observed={observed:?}; trace={trace:?}; first_root={:?}; second_root={:?}; first_socket={:?}; second_socket={:?}; first_connect={:?}; second_connect={:?}; metadata={{createdAt:2026-10-16T00:00:00Z,daemonPid:None,startIdentity:None}}; interleaving=[open-first-root, retarget-configured-link, read-first-root, pin-first-socket-leaf, probe-pinned-leaf]",
            std::fs::metadata(&first.0).map(|m| (m.dev(), m.ino())),
            std::fs::metadata(&second.0).map(|m| (m.dev(), m.ino())),
            std::fs::symlink_metadata(&first_socket).map(|m| (m.dev(), m.ino(), m.file_type().is_socket())),
            std::fs::symlink_metadata(&second_socket).map(|m| (m.dev(), m.ino(), m.file_type().is_socket())),
            std::os::unix::net::UnixStream::connect(&first_socket).map(|_| ()),
            std::os::unix::net::UnixStream::connect(&second_socket).map(|_| ()),
        );
        std::fs::remove_file(link).unwrap();
        drop(live);
    }
    #[test]
    fn socket_leaf_swap_cannot_borrow_an_external_listener() {
        let root = Root::new("leaf-swap-root");
        root.metadata(
            "target",
            serde_json::json!({"createdAt":"2026-10-16T00:00:00Z"}),
        );
        let socket = socket_path_in(&root.0, "target");
        let stale = UnixListener::bind(&socket).unwrap();
        drop(stale);
        let original = std::fs::symlink_metadata(&socket)
            .map(|metadata| (metadata.dev(), metadata.ino()))
            .unwrap();
        let external = Root::new("leaf-swap-external");
        let external_socket = socket_path_in(&external.0, "target");
        let external_listener = UnixListener::bind(&external_socket).unwrap();
        let held = root.0.join("target.held.sock");
        let before_socket = socket.clone();
        let before_external = external_socket.clone();
        let before_held = held.clone();
        let after_socket = socket.clone();
        let after_held = held.clone();
        SOCKET_PROBE_HOOKS.with(|hooks| {
            *hooks.borrow_mut() = (
                Some(Box::new(move || {
                    std::fs::rename(&before_socket, &before_held).unwrap();
                    symlink(&before_external, &before_socket).unwrap();
                })),
                Some(Box::new(move || {
                    std::fs::remove_file(&after_socket).unwrap();
                    std::fs::rename(&after_held, &after_socket).unwrap();
                })),
            );
        });
        let mut options = fast();
        options.attempts = 1;
        let _ = take_socket_probe_trace();
        let observed = observe_session_in(&root.0, "target", &options);
        let trace = take_socket_probe_trace();
        assert!(
            !matches!(
                observed,
                KeyedSession::Found(SessionInfo {
                    status: SessionStatus::Running,
                    ..
                })
            ),
            "leaf swap borrowed external listener: observed={observed:?}; trace={trace:?}; root={:?}; original_socket={original:?}; restored_socket={:?}; external_root={:?}; external_socket={:?}; metadata={{createdAt:2026-10-16T00:00:00Z,daemonPid:None,startIdentity:None}}; hook_order=[capture, pin-leaf, rename-original, install-symlink, connect, remove-symlink, restore-original, recapture]",
            std::fs::metadata(&root.0).map(|m| (m.dev(), m.ino())),
            std::fs::symlink_metadata(&socket).map(|m| (m.dev(), m.ino(), m.file_type().is_socket())),
            std::fs::metadata(&external.0).map(|m| (m.dev(), m.ino())),
            std::fs::symlink_metadata(&external_socket).map(|m| (m.dev(), m.ino(), m.file_type().is_socket())),
        );
        assert_eq!(
            std::fs::symlink_metadata(&socket)
                .map(|metadata| (metadata.dev(), metadata.ino()))
                .unwrap(),
            original
        );
        drop(external_listener);
    }

    #[test]
    fn socket_basename_is_selected_from_the_root_pinned_before_retargeting() {
        let first = Root::new("early-retarget-first");
        let second = Root::new("early-retarget-second");
        let readable = first.0.join("target.sock");
        let first_listener = UnixListener::bind(&readable).unwrap();
        let (_, bounded) = socket_filenames("target");
        let second_listener = UnixListener::bind(second.0.join(bounded)).unwrap();
        let link = first.0.with_extension("early-retarget-link");
        symlink(&first.0, &link).unwrap();
        let access = RetargetingRoot {
            inner: OsAccess,
            link: link.clone(),
            target: second.0.clone(),
            retargeted: Cell::new(false),
            retarget_after_open: true,
        };
        let KeyedSession::Found(found) = observe_with(&access, &link, "target", &fast()) else {
            panic!("the socket-only target in the pinned root should remain observable")
        };
        assert_eq!(found.status, SessionStatus::Running);
        assert_eq!(
            found.socket_path,
            std::fs::canonicalize(&first.0)
                .unwrap()
                .join("target.sock")
        );
        std::fs::remove_file(link).unwrap();
        drop(first_listener);
        drop(second_listener);
    }

    #[test]
    fn explicit_root_lookup_does_not_consult_the_ambient_registry() {
        let root = Root::new("explicit-root");
        root.metadata("target", live_metadata());
        let unrelated = PathBuf::from(format!("/tmp/{}", "x".repeat(SUN_PATH_MAX)));
        let observed = super::super::root::with_root(&unrelated, || {
            observe_session_in(&root.0, "target", &fast())
        });
        assert!(matches!(observed, KeyedSession::Found(_)));
    }


    #[cfg(target_os = "macos")]
    #[test]
    fn keyed_lookup_uses_native_birth_identity_without_spawning_ps() {
        let root = Root::new("native-birth-no-spawn");
        root.metadata("target", live_metadata());
        let before =
            super::super::list::PROCESS_START_TOKEN_SPAWNS.with(std::cell::Cell::get);
        assert!(matches!(
            observe_session_in(&root.0, "target", &fast()),
            KeyedSession::Found(_)
        ));
        assert_eq!(
            super::super::list::PROCESS_START_TOKEN_SPAWNS.with(std::cell::Cell::get),
            before
        );
    }

    #[test]
    fn unrelated_registry_growth_does_not_change_actual_work() {
        let mut expected = None;
        for unrelated in [0, 10, 1000] {
            let root = Root::new(&format!("growth-{unrelated}"));
            root.metadata("target", live_metadata());
            for index in 0..unrelated {
                std::fs::write(
                    root.0.join(format!("other-{index}.json")),
                    if index % 2 == 0 { "{" } else { "{}" },
                )
                .unwrap();
            }
            let access = Counted::new();
            assert!(matches!(
                observe_with(&access, &root.0, "target", &fast()),
                KeyedSession::Found(_)
            ));
            let counts = *access.counts.borrow();
            assert_eq!(
                *expected.get_or_insert(counts),
                counts,
                "work changed with {unrelated} unrelated PTYs"
            );
            assert_eq!(
                counts,
                Counts {
                    roots: 1,
                    companions: 4,
                    sockets: 4,
                    processes: 2,
                    probes: 0,
                    probe_budget: Duration::ZERO,
                    sleep_time: Duration::ZERO,
                }
            );
            assert!(
                super::super::list::list_sessions_strict_in(&root.0, &ListOptions::default())
                    .errors
                    .len()
                    >= unrelated / 2
            );
        }
    }

    #[test]
    fn permanently_malformed_target_is_indeterminate() {
        let root = Root::new("malformed");
        std::fs::write(root.0.join("target.json"), "{").unwrap();
        assert!(matches!(
            observe_session_in(&root.0, "target", &fast()),
            KeyedSession::Indeterminate(KeyedSessionError {
                kind: KeyedSessionErrorKind::MetadataMalformed,
                ..
            })
        ));
    }

    #[test]
    fn record_changed_mid_read_is_found_only_after_a_stable_retry() {
        let root = Root::new("rewrite");
        root.metadata("target", live_metadata());
        let mut replacement = live_metadata();
        replacement["tags"]["st3.subject"] = serde_json::Value::String("agent/replacement".into());
        let access = Mutating::rewrite_once(
            root.0.join("target.json"),
            replacement.to_string().into_bytes(),
        );
        let KeyedSession::Found(found) = observe_with(&access, &root.0, "target", &fast()) else {
            panic!("stable retry did not recover the rewritten record")
        };
        assert_eq!(
            found
                .metadata
                .and_then(|metadata| metadata.tags)
                .and_then(|tags| tags.get("st3.subject").cloned())
                .as_deref(),
            Some("agent/replacement")
        );
        assert_eq!(access.process_calls.get(), 3);
    }

    #[test]
    fn process_birth_replaced_between_reads_never_returns_found() {
        let root = Root::new("birth-replaced");
        root.metadata("target", live_metadata());
        let access = Mutating::recycle_between_reads();
        assert!(matches!(
            observe_with(&access, &root.0, "target", &fast()),
            KeyedSession::Indeterminate(KeyedSessionError {
                kind: KeyedSessionErrorKind::Changed,
                ..
            })
        ));
    }

    #[test]
    fn pid_reuse_identity_mismatch_is_not_found() {
        let root = Root::new("reused");
        let mut metadata = live_metadata();
        metadata["daemonStartToken"] = serde_json::Value::String("linux:previous".into());
        root.metadata("target", metadata);
        assert!(matches!(
            observe_session_in(&root.0, "target", &fast()),
            KeyedSession::Indeterminate(KeyedSessionError {
                kind: KeyedSessionErrorKind::ProcessIdentity,
                ..
            })
        ));
    }

    #[test]
    fn observation_keeps_metadata_fields_for_authority() {
        let root = Root::new("fields");
        root.metadata("target", live_metadata());
        let KeyedSession::Found(session) = observe_session_in(&root.0, "target", &fast()) else {
            panic!()
        };
        let metadata = session.metadata.unwrap();
        assert_eq!(
            metadata
                .tags
                .unwrap()
                .into_iter()
                .collect::<BTreeMap<_, _>>()["st3.subject"],
            "agent/example"
        );
        assert_eq!(metadata.created_at, "2026-10-16T00:00:00Z");
    }
}
