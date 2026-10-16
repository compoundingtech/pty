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

use super::list::{ListOptions, SessionInfo, SessionStatus};
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
    descriptor_path: Option<PathBuf>,
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
    fn probe_socket(
        &self,
        root: &Self::Root,
        filename: &OsStr,
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

    fn descriptor_path(fd: i32) -> Option<PathBuf> {
        #[cfg(target_os = "linux")]
        {
            return Some(PathBuf::from(format!("/proc/self/fd/{fd}")));
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = fd;
            None
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


    #[cfg(target_os = "macos")]
    fn verify_pinned_root(root: &OpenRoot) -> Result<(), KeyedSessionError> {
        let metadata = std::fs::metadata(&root.pinned_path).map_err(|error| {
            KeyedSessionError::new(
                KeyedSessionErrorKind::SocketProbe,
                Some(root.pinned_path.clone()),
                format!("opened registry root path is unavailable: {error}"),
            )
        })?;
        if metadata.dev() != root.device
            || metadata.ino() != root.inode
            || !metadata.file_type().is_dir()
        {
            return Err(KeyedSessionError::new(
                KeyedSessionErrorKind::SocketProbe,
                Some(root.pinned_path.clone()),
                "opened registry root path no longer identifies the pinned directory",
            ));
        }
        Ok(())
    }

    fn probe_result(
        display_path: &Path,
        outcome: Option<&io::Result<()>>,
    ) -> Result<bool, KeyedSessionError> {
        match outcome {
            Some(Ok(())) => Ok(true),
            Some(Err(error))
                if matches!(
                    error.raw_os_error(),
                    Some(libc::ENOENT) | Some(libc::ECONNREFUSED)
                ) =>
            {
                Ok(false)
            }
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
        let descriptor_path = Self::descriptor_path(fd.as_raw_fd());
        Ok(Some(OpenRoot {
            fd,
            path: root.to_path_buf(),
            pinned_path,
            descriptor_path,
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
        Ok(Self::reporting_root(root).join(filename))
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

    fn probe_socket(
        &self,
        root: &Self::Root,
        filename: &OsStr,
        budget: Duration,
    ) -> Result<bool, KeyedSessionError> {
        let display_path = root.pinned_path.join(filename);
        let path = match root.descriptor_path.as_ref() {
            Some(descriptor) => descriptor.join(filename),
            None => {
                #[cfg(target_os = "macos")]
                {
                    Self::verify_pinned_root(root)?;
                    display_path.clone()
                }
                #[cfg(not(target_os = "macos"))]
                {
                    return Err(KeyedSessionError::new(
                        KeyedSessionErrorKind::SocketProbe,
                        Some(display_path),
                        "this platform cannot bind a socket probe to the opened directory",
                    ));
                }
            }
        };
        let outcomes =
            super::list::probe_socket_results(std::slice::from_ref(&path), budget);
        let result = Self::probe_result(&display_path, outcomes.get(&path));
        #[cfg(target_os = "macos")]
        Self::verify_pinned_root(root)?;
        result
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
    if let (Some(pid), Some(process)) = (pid, process_first.as_ref()) {
        let expected = metadata
            .as_ref()
            .and_then(SessionMetadata::daemon_start_token)
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
    }

    let socket_reachable = if (first.readable_socket.is_some()
        || first.bounded_socket.is_some())
        && process_first.is_none()
    {
        access.probe_socket(root, &socket_name, options.socket_probe_budget)?
    } else {
        false
    };
    let status = if metadata
        .as_ref()
        .is_some_and(|metadata| metadata.has_exited() || metadata.exit_code.is_some())
    {
        SessionStatus::Exited
    } else if process_first.is_some() || socket_reachable {
        SessionStatus::Running
    } else if metadata.is_some() {
        SessionStatus::Vanished
    } else {
        return Err(KeyedSessionError::new(
            KeyedSessionErrorKind::ProcessIdentity,
            Some(root_path.to_path_buf()),
            "present session artifacts do not identify a session",
        ));
    };
    let observed_pid = (process_first.is_some()).then_some(pid).flatten();

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
    use std::os::unix::fs::symlink;
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

    #[test]
    fn stable_fixed_inputs_match_the_full_snapshot_oracle() {
        for (label, value, expected) in [
            ("live", live_metadata(), SessionStatus::Running),
            (
                "exited",
                serde_json::json!({"createdAt":"2026-10-16T00:00:00Z","exitedAt":"2026-10-16T00:01:00Z","exitCode":0}),
                SessionStatus::Exited,
            ),
            (
                "vanished",
                serde_json::json!({"createdAt":"2026-10-16T00:00:00Z"}),
                SessionStatus::Vanished,
            ),
            (
                "exit-code-only",
                serde_json::json!({"createdAt":"2026-10-16T00:00:00Z","exitCode":0}),
                SessionStatus::Exited,
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
            assert_eq!(found, listed);
            assert_eq!(found.status, expected);
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
            budget: Duration,
        ) -> Result<bool, KeyedSessionError> {
            let mut counts = self.counts.borrow_mut();
            counts.probes += 1;
            counts.probe_budget += budget;
            drop(counts);
            self.inner.probe_socket(root, filename, budget)
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
            budget: Duration,
        ) -> Result<bool, KeyedSessionError> {
            self.inner.probe_socket(root, filename, budget)
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
            budget: Duration,
        ) -> Result<bool, KeyedSessionError> {
            self.inner.probe_socket(root, filename, budget)
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
        let stale = UnixListener::bind(socket_path_in(&first.0, "target")).unwrap();
        drop(stale);
        let live = UnixListener::bind(socket_path_in(&second.0, "target")).unwrap();
        let link = first.0.with_extension("retarget-link");
        symlink(&first.0, &link).unwrap();
        let access = RetargetingRoot {
            inner: OsAccess,
            link: link.clone(),
            target: second.0.clone(),
            retargeted: Cell::new(false),
            retarget_after_open: false,
        };
        let KeyedSession::Found(found) = observe_with(&access, &link, "target", &fast()) else {
            panic!("the pinned first root should remain observable")
        };
        assert_eq!(found.status, SessionStatus::Vanished);
        assert_eq!(
            found.socket_path,
            socket_path_in(&std::fs::canonicalize(&first.0).unwrap(), "target")
        );
        std::fs::remove_file(link).unwrap();
        drop(live);
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
        assert_eq!(found.socket_path, readable);
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
