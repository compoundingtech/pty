//! Non-blocking AF_UNIX connects for callers that multiplex many session
//! sockets on one thread under one deadline: the registry liveness probe and
//! the batch STATUS query.
//!
//! A blocking `connect(2)` to a listener whose accept queue is full waits on
//! Linux until the daemon accepts, which a wedged daemon never does. Parking
//! one thread per socket bounded the caller's wait but not the threads: each
//! stuck connect kept its thread (and its malloc arena) alive. Here nothing
//! blocks; the caller retries a full queue until its own deadline.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

/// How often a connect that found the accept queue full is retried. Linux
/// offers no readiness event for "the queue has room again". Callers record
/// `now + RETRY_TICK` per busy socket and reconnect only once it has passed,
/// so another socket that stays ready cannot turn the retry into a spin.
pub const RETRY_TICK: Duration = Duration::from_millis(10);

std::thread_local! {
    static BUSY_CONNECTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How many connects on the calling thread have found an accept queue full
/// ([`Connect::Busy`]). Test-only observability for the retry throttle; not a
/// stable API.
#[doc(hidden)]
pub fn busy_connects_on_this_thread() -> u64 {
    BUSY_CONNECTS.with(|n| n.get())
}

/// One non-blocking connect attempt.
pub enum Connect {
    /// Connected; the stream is non-blocking.
    Connected(UnixStream),
    /// EINPROGRESS: poll for POLLOUT, then read the outcome with
    /// [`UnixStream::take_error`].
    InProgress(UnixStream),
    /// EAGAIN: the listener's accept queue is full (Linux). A blocking connect
    /// would wait for room, so the caller retries until its deadline. macOS
    /// refuses a full queue with ECONNREFUSED instead, for blocking and
    /// non-blocking connects alike, so there it arrives as [`Connect::Failed`]
    /// exactly as a blocking connect would report it.
    Busy,
    /// The final error a blocking [`UnixStream::connect`] would have returned.
    Failed(io::Error),
}

/// Start a non-blocking connect to `path`.
pub fn connect(path: &Path) -> Connect {
    let (addr, len) = match sockaddr(path) {
        Ok(addr) => addr,
        Err(e) => return Connect::Failed(e),
    };
    let fd = match nonblocking_socket() {
        Ok(fd) => fd,
        Err(e) => return Connect::Failed(e),
    };
    // SAFETY: `addr` is an initialised sockaddr_un and `len` does not exceed
    // its size; `fd` is an open socket this function owns.
    let rc = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&addr as *const libc::sockaddr_un).cast(),
            len,
        )
    };
    if rc == 0 {
        return Connect::Connected(UnixStream::from(fd));
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EINPROGRESS) => Connect::InProgress(UnixStream::from(fd)),
        // EINTR leaves the attempt in an unspecified state; a fresh socket on
        // the next round is simpler than resuming this one.
        Some(libc::EAGAIN) | Some(libc::EINTR) => {
            BUSY_CONNECTS.with(|n| n.set(n.get() + 1));
            Connect::Busy
        }
        _ => Connect::Failed(err),
    }
}

/// Wait on `fds` until one is ready, `deadline` passes, or `next_retry` (the
/// earliest instant a busy connect is due again) arrives. Returns false when
/// the deadline has passed or poll(2) failed with anything but EINTR: the
/// caller stops and reports whatever is still pending as unanswered.
pub fn poll_until(
    fds: &mut [libc::pollfd],
    deadline: Instant,
    next_retry: Option<Instant>,
) -> bool {
    let now = Instant::now();
    if now >= deadline {
        return false;
    }
    let wake = next_retry.map_or(deadline, |at| at.min(deadline));
    let wait = wake.saturating_duration_since(now);
    // Round up so a sub-millisecond remainder does not become a busy spin.
    let timeout_ms = wait
        .as_micros()
        .div_ceil(1000)
        .min(libc::c_int::MAX as u128) as libc::c_int;
    // SAFETY: `fds` is a valid, exclusively borrowed slice of pollfd for the
    // duration of the call and its length is passed alongside.
    let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
    if rc < 0 {
        // revents are unspecified after a failed poll.
        for pfd in fds.iter_mut() {
            pfd.revents = 0;
        }
        return io::Error::last_os_error().raw_os_error() == Some(libc::EINTR);
    }
    true
}

/// The address `UnixStream::connect` would build, with its error texts.
fn sockaddr(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    // SAFETY: all-zero is a valid sockaddr_un.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "paths must not contain interior null bytes",
        ));
    }
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path must be shorter than SUN_LEN",
        ));
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = *src as libc::c_char;
    }
    let len = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    Ok((addr, len as libc::socklen_t))
}

/// A close-on-exec, non-blocking AF_UNIX stream socket.
fn nonblocking_socket() -> io::Result<OwnedFd> {
    #[cfg(target_os = "linux")]
    let kind = libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let kind = libc::SOCK_STREAM;
    // SAFETY: socket(2) takes no pointers.
    let raw = unsafe { libc::socket(libc::AF_UNIX, kind, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a freshly created descriptor nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: fcntl on a descriptor this function owns; no pointers.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: as above.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: as above.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // std sets SO_NOSIGPIPE on every Apple socket so a write to a closed peer
    // fails with EPIPE instead of killing the process; match it.
    #[cfg(target_vendor = "apple")]
    {
        let one: libc::c_int = 1;
        // SAFETY: `one` outlives the call and its size is passed alongside.
        let rc = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&one as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(fd)
}
