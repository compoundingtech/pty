//! Terminal and descriptor helpers for the interactive client operations:
//! tty detection, window size, Node-compatible raw mode, a SIGWINCH self-pipe,
//! and blocking fd read/write primitives built on `poll`.

use std::io;
use std::os::unix::io::RawFd;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, Ordering};

/// A pipe with `FD_CLOEXEC` on both ends, and `O_NONBLOCK` too when asked.
///
/// Every platform we build for has `pipe2` except Apple's, which has no such
/// system call at all — it is missing from the kernel, not from a packaging
/// of `libc`. So there are two implementations and the difference matters:
/// `pipe2` sets the flags as it creates the descriptors, and the fallback
/// sets them afterwards with `fcntl`. In the gap between the two calls a
/// `fork` on another thread inherits descriptors that are not yet
/// close-on-exec. That is the whole reason `pipe2` exists, and it is why the
/// atomic form is used wherever it is offered rather than using the fallback
/// everywhere for the sake of one code path.
///
/// The exposure here is small: the pipes this makes are created early and on
/// one thread. It is not nothing, so it is written down rather than smoothed
/// over.
pub fn cloexec_pipe(nonblocking: bool) -> io::Result<[RawFd; 2]> {
    let mut fds = [0 as RawFd; 2];

    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    {
        let mut flags = libc::O_CLOEXEC;
        if nonblocking {
            flags |= libc::O_NONBLOCK;
        }
        // SAFETY: `pipe2` writes two descriptors into a two-element array.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), flags) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        // SAFETY: `pipe` writes two descriptors into a two-element array.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        for fd in fds {
            // Read the flags before adding to them. A fresh pipe carries
            // none of these, but a helper that clears what it did not set is
            // the kind of thing somebody reuses and regrets.
            let set = |get: libc::c_int, set: libc::c_int, bit: libc::c_int| -> bool {
                // SAFETY: `fd` is a descriptor this function just created.
                let current = unsafe { libc::fcntl(fd, get) };
                current >= 0 && unsafe { libc::fcntl(fd, set, current | bit) } == 0
            };
            let ok = set(libc::F_GETFD, libc::F_SETFD, libc::FD_CLOEXEC)
                && (!nonblocking || set(libc::F_GETFL, libc::F_SETFL, libc::O_NONBLOCK));
            if !ok {
                let err = io::Error::last_os_error();
                // SAFETY: both descriptors are ours and still open.
                unsafe {
                    libc::close(fds[0]);
                    libc::close(fds[1]);
                }
                return Err(err);
            }
        }
    }

    Ok(fds)
}

/// Is `fd` a terminal?
pub fn is_tty(fd: RawFd) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}
/// The controlling terminal path for the input descriptor, when available.
pub fn tty_name(fd: RawFd) -> Option<String> {
    if !is_tty(fd) {
        return None;
    }
    let mut buf: [libc::c_char; 256] = [0; 256];
    // SAFETY: ttyname_r writes at most buf.len() bytes, including the NUL.
    if unsafe { libc::ttyname_r(fd, buf.as_mut_ptr(), buf.len()) } != 0 {
        return None;
    }
    // SAFETY: successful ttyname_r NUL-terminates the buffer.
    unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
        .to_str()
        .ok()
        .map(str::to_owned)
}


/// The window size `(rows, cols)` of a tty fd, or `None` when it is not a
/// terminal (or the ioctl fails).
pub fn window_size(fd: RawFd) -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 {
        Some((ws.ws_row, ws.ws_col))
    } else {
        None
    }
}

/// `stdout.rows ?? 24`, `stdout.columns ?? 80`: the size of `fd` when it is a
/// tty, else the 24×80 default (`client.ts:581-582`).
pub fn size_or_default(fd: RawFd) -> (u16, u16) {
    window_size(fd).unwrap_or((24, 80))
}

/// RAII guard that puts a tty into raw mode and restores the original
/// termios on drop. The mode mirrors Node's `setRawMode(true)`
/// (libuv `UV_TTY_MODE_RAW`): it keeps output post-processing (ONLCR) on,
/// unlike `cfmakeraw`.
pub struct RawMode {
    fd: RawFd,
    original: libc::termios,
}

impl RawMode {
    /// Enable raw mode on `fd`. Fails when `fd` is not a tty.
    pub fn enable(fd: RawFd) -> io::Result<RawMode> {
        unsafe {
            let mut termios: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut termios) != 0 {
                return Err(io::Error::last_os_error());
            }
            let original = termios;
            termios.c_iflag &=
                !(libc::BRKINT | libc::ICRNL | libc::INPCK | libc::ISTRIP | libc::IXON);
            termios.c_oflag |= libc::ONLCR;
            termios.c_cflag |= libc::CS8;
            termios.c_lflag &= !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG);
            termios.c_cc[libc::VMIN] = 1;
            termios.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(fd, libc::TCSADRAIN, &termios) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(RawMode { fd, original })
        }
    }

    /// Enable raw mode only when `fd` is a tty (Node: `if (stdin.isTTY)`).
    pub fn enable_if_tty(fd: RawFd) -> Option<RawMode> {
        if is_tty(fd) {
            RawMode::enable(fd).ok()
        } else {
            None
        }
    }

    fn restore(&self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSADRAIN, &self.original);
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Wait until `fds` are ready or `timeout_ms` (-1 = forever) elapses; returns
/// the number of ready descriptors (0 on timeout). EINTR restarts the wait.
pub fn poll(fds: &mut [libc::pollfd], timeout_ms: i32) -> io::Result<usize> {
    loop {
        let r = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        if r >= 0 {
            return Ok(r as usize);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// One `read(2)` on `fd`; `Ok(0)` is end of file. EINTR restarts.
pub fn read_fd(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Write all of `buf` to `fd`, waiting for writability on EAGAIN (an inherited
/// descriptor may be non-blocking) and restarting on EINTR. The fd is never
/// closed.
pub fn write_all_fd(fd: RawFd, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let n = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if n >= 0 {
            buf = &buf[n as usize..];
            continue;
        }
        let err = io::Error::last_os_error();
        match err.kind() {
            io::ErrorKind::Interrupted => {}
            io::ErrorKind::WouldBlock => {
                let mut pfd = [libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                }];
                poll(&mut pfd, -1)?;
            }
            _ => return Err(err),
        }
    }
    Ok(())
}

/// `io::Write` over a borrowed raw descriptor (stdout/stderr or a test pipe).
/// Never closes the fd.
#[derive(Debug, Clone, Copy)]
pub struct FdWriter(pub RawFd);

impl io::Write for FdWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        write_all_fd(self.0, buf)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

static SIGWINCH_FD: AtomicI32 = AtomicI32::new(-1);
static SIGWINCH_LOCK: Mutex<()> = Mutex::new(());

extern "C" fn on_sigwinch(_: libc::c_int) {
    let fd = SIGWINCH_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        // A write to a non-blocking pipe is async-signal-safe; a full pipe
        // just drops the wake-up (one pending byte is enough).
        unsafe {
            libc::write(fd, b"w".as_ptr() as *const libc::c_void, 1);
        }
    }
}

/// A self-pipe that becomes readable whenever SIGWINCH arrives. Only one may
/// be live per process; the previous disposition is restored on drop.
pub struct SigwinchPipe {
    read_fd: RawFd,
    write_fd: RawFd,
    previous: libc::sigaction,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl SigwinchPipe {
    /// Install the handler.
    pub fn install() -> io::Result<SigwinchPipe> {
        let guard = SIGWINCH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let fds = cloexec_pipe(true)?;
        SIGWINCH_FD.store(fds[1], Ordering::SeqCst);
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = on_sigwinch as extern "C" fn(libc::c_int) as usize;
        action.sa_flags = libc::SA_RESTART;
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(libc::SIGWINCH, &action, &mut previous) != 0 {
                let err = io::Error::last_os_error();
                SIGWINCH_FD.store(-1, Ordering::SeqCst);
                libc::close(fds[0]);
                libc::close(fds[1]);
                return Err(err);
            }
        }
        Ok(SigwinchPipe {
            read_fd: fds[0],
            write_fd: fds[1],
            previous,
            _guard: guard,
        })
    }

    /// The readable end to include in a `poll` set.
    pub fn fd(&self) -> RawFd {
        self.read_fd
    }

    /// Consume pending wake-ups; returns whether any arrived.
    pub fn drain(&self) -> bool {
        let mut buf = [0u8; 64];
        let mut any = false;
        loop {
            let n = unsafe {
                libc::read(
                    self.read_fd,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n > 0 {
                any = true;
            } else {
                return any;
            }
        }
    }
}

impl Drop for SigwinchPipe {
    fn drop(&mut self) {
        unsafe {
            libc::sigaction(libc::SIGWINCH, &self.previous, std::ptr::null_mut());
            SIGWINCH_FD.store(-1, Ordering::SeqCst);
            libc::close(self.read_fd);
            libc::close(self.write_fd);
        }
    }
}

/// Replace the Kitty keyboard-protocol encoding of Ctrl+\ (`ESC[92;5u`) with
/// the legacy byte so the detach logic works with one representation
/// (`client.ts:20-31`).
pub fn normalize_detach_key(data: &[u8]) -> Vec<u8> {
    const KITTY: &[u8] = b"\x1b[92;5u";
    if !data.windows(KITTY.len()).any(|w| w == KITTY) {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i..].starts_with(KITTY) {
            out.push(DETACH_KEY);
            i += KITTY.len();
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

/// Detach key: Ctrl+\ (0x1c).
pub const DETACH_KEY: u8 = 0x1c;

/// A second Ctrl+\ within this window forwards a literal 0x1c instead of
/// detaching (`client.ts:544`).
pub const DOUBLE_TAP_MS: u64 = 300;
