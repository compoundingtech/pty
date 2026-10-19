//! The held child of a launch operation: a native fork of the daemon that
//! prepares everything a session child needs, acknowledges `prepared`, and
//! then waits on its gate. It runs the user's command only when the owner's
//! transfer has committed and the daemon writes the release byte. End of file
//! on the gate (the daemon died, or a cancel closed it) makes it exit without
//! running anything.
//!
//! **Parity with today's spawn.** Today `portable-pty` spawns the session
//! child through `std::process::Command`. What that child ends up with is
//! reproduced here exactly, and a test compares the two:
//!
//! - argv `/bin/sh -c 'exec "$@"' sh <command> <args…>` from `/bin/sh`;
//! - the environment: `SHELL` from `CommandBuilder::get_shell`, then the
//!   session environment over it;
//! - the working directory, or the home directory when it is not one;
//! - the daemon's umask, which `portable-pty` leaves alone;
//! - a new session and process group with the slave as controlling terminal
//!   and as descriptors 0, 1 and 2, and no other descriptor;
//! - an empty signal mask, and the dispositions an `exec` from the daemon
//!   would leave: `SIGCHLD`, `SIGHUP`, `SIGINT`, `SIGQUIT`, `SIGTERM`,
//!   `SIGALRM` (portable-pty) and `SIGPIPE` (std) at their defaults, any
//!   other ignored signal still ignored, and everything else at its default.
//!
//! **Everything is built before the fork.** The child of a multithreaded
//! process may call only async-signal-safe functions, so the child side here
//! allocates nothing and touches no lock.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;

use std::collections::BTreeMap;

/// The session environment, as the daemon builds it.
pub type EnvMap = BTreeMap<String, String>;

/// The acknowledgement byte the child writes when it is prepared.
pub const PREPARED: u8 = b'P';
/// The byte that releases the child into `exec`.
pub const RELEASE: u8 = b'R';
/// The first byte of a failure acknowledgement: then the step, then errno.
pub const FAILED: u8 = b'F';
/// The child's exit status when its gate closes without a release.
pub const NOT_RELEASED: i32 = 70;
/// The child's exit status when a preparation step fails.
pub const PREPARE_FAILED: i32 = 71;

/// A preparation step, as a failure acknowledgement names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Setsid = 1,
    OpenSlave = 2,
    ControllingTty = 3,
    Stdio = 4,
    Chdir = 5,
}

impl Step {
    pub fn name(code: u8) -> &'static str {
        match code {
            1 => "setsid",
            2 => "open-slave",
            3 => "controlling-tty",
            4 => "stdio",
            5 => "chdir",
            _ => "unknown",
        }
    }
}

/// Everything the child uses, built before the fork.
pub struct HeldSpec {
    path: CString,
    /// Owns the strings `argv_ptrs` points into.
    #[allow(dead_code)]
    argv: Vec<CString>,
    argv_ptrs: Vec<*const libc::c_char>,
    /// Owns the strings `envp_ptrs` points into.
    #[allow(dead_code)]
    envp: Vec<CString>,
    envp_ptrs: Vec<*const libc::c_char>,
    cwd: CString,
    umask: libc::mode_t,
    slave: CString,
    dispositions: Vec<(libc::c_int, libc::sighandler_t)>,
    fd_limit: libc::c_int,
    /// Debug builds: stop just before acknowledging, so a test can inspect
    /// or kill a prepared child that never reached `prepared`.
    stop_before_ack: bool,
    /// Debug builds: stop at once after the fork, still holding every
    /// descriptor of the daemon's and still in its process group.
    stop_after_fork: bool,
}

// SAFETY: the raw pointers point into the CStrings this struct owns, which
// never move or change once built; the struct is only read after that.
unsafe impl Send for HeldSpec {}

fn cstring(bytes: &[u8], what: &str) -> Result<CString, String> {
    CString::new(bytes).map_err(|_| format!("{what} contains a NUL byte"))
}

impl HeldSpec {
    /// Build the child's spec from the same inputs today's spawn uses.
    pub fn build(
        command: &str,
        args: &[String],
        cwd: &str,
        child_env: &EnvMap,
        umask: libc::mode_t,
        slave: &std::path::Path,
    ) -> Result<HeldSpec, String> {
        // The builder today's spawn hands portable-pty, for the two facts it
        // derives from it: the argv and the shell.
        let mut builder = pty_spawn::shell_exec(command, args);
        builder.cwd(cwd);
        builder.env_clear();
        for (key, value) in child_env {
            builder.env(key, value);
        }
        let argv: Vec<CString> = builder
            .get_argv()
            .iter()
            .map(|arg| cstring(arg.as_bytes(), "an argument"))
            .collect::<Result<_, _>>()?;
        let path = argv.first().cloned().ok_or("empty argv")?;

        // `SHELL` first, then the session environment over it, sorted by
        // key as std's `Command` passes it.
        let mut env: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
        env.insert("SHELL".into(), builder.get_shell());
        for (key, value) in builder.iter_full_env_as_str() {
            env.insert(key.to_string(), value.to_string());
        }
        let envp: Vec<CString> = env
            .iter()
            .map(|(key, value)| {
                cstring(format!("{key}={value}").as_bytes(), "an environment entry")
            })
            .collect::<Result<_, _>>()?;

        // The working directory, or the home directory when it is not a
        // directory, as portable-pty chooses.
        let dir = if std::path::Path::new(cwd).is_dir() {
            cwd.to_string()
        } else {
            home_dir(child_env)
        };

        let argv_ptrs = argv
            .iter()
            .map(|arg| arg.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();
        let envp_ptrs = envp
            .iter()
            .map(|entry| entry.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();
        Ok(HeldSpec {
            path,
            argv,
            argv_ptrs,
            envp,
            envp_ptrs,
            cwd: cstring(dir.as_bytes(), "the working directory")?,
            umask,
            slave: cstring(slave.as_os_str().as_bytes(), "the terminal path")?,
            dispositions: exec_dispositions(),
            fd_limit: descriptor_limit(),
            stop_before_ack: cfg!(debug_assertions)
                && std::env::var("PTY_TEST_HELD_CHILD").as_deref() == Ok("stop-before-ack"),
            stop_after_fork: cfg!(debug_assertions)
                && std::env::var("PTY_TEST_HELD_CHILD").as_deref() == Ok("stop-after-fork"),
        })
    }

    #[cfg(test)]
    pub fn argv(&self) -> Vec<String> {
        self.argv
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[cfg(test)]
    pub fn envp(&self) -> Vec<String> {
        self.envp
            .iter()
            .map(|entry| entry.to_string_lossy().into_owned())
            .collect()
    }
}

/// `HOME` from the session environment, else the password database, else
/// `/`: portable-pty's `get_home_dir`.
fn home_dir(child_env: &EnvMap) -> String {
    if let Some(home) = child_env.get("HOME") {
        return home.clone();
    }
    // SAFETY: getpwuid returns a pointer into static storage or null; it is
    // read at once, before any other call could reuse it.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if entry.is_null() || (*entry).pw_dir.is_null() {
            return "/".into();
        }
        std::ffi::CStr::from_ptr((*entry).pw_dir)
            .to_string_lossy()
            .into_owned()
    }
}

/// The signals an `exec` from this daemon's spawn path resets: portable-pty's
/// list, plus `SIGPIPE`, which std's `Command` resets because the Rust
/// runtime ignores it.
const RESET_SIGNALS: [libc::c_int; 7] = [
    libc::SIGCHLD,
    libc::SIGHUP,
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGTERM,
    libc::SIGALRM,
    libc::SIGPIPE,
];

#[cfg(target_os = "linux")]
const SIGNAL_LIMIT: libc::c_int = 65;
#[cfg(not(target_os = "linux"))]
const SIGNAL_LIMIT: libc::c_int = 32;

/// For every catchable signal, the disposition the session child would have
/// after today's spawn: a caught signal becomes the default across `exec`,
/// an ignored one stays ignored unless it is in [`RESET_SIGNALS`].
fn exec_dispositions() -> Vec<(libc::c_int, libc::sighandler_t)> {
    let mut out = Vec::new();
    for signal in 1..SIGNAL_LIMIT {
        if signal == libc::SIGKILL || signal == libc::SIGSTOP {
            continue;
        }
        // SAFETY: querying a disposition into a zeroed struct.
        let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
        if unsafe { libc::sigaction(signal, std::ptr::null(), &mut current) } != 0 {
            continue;
        }
        let handler = if !RESET_SIGNALS.contains(&signal) && current.sa_sigaction == libc::SIG_IGN {
            libc::SIG_IGN
        } else {
            libc::SIG_DFL
        };
        out.push((signal, handler));
    }
    out
}

/// One past the highest descriptor the child could have inherited: the
/// soft limit, or the highest descriptor open now, whichever is larger.
fn descriptor_limit() -> libc::c_int {
    const CAP: u64 = 1 << 20;
    // SAFETY: getrlimit fills the struct passed.
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    let soft = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
        (limit.rlim_cur as u64).min(CAP)
    } else {
        CAP
    };
    let highest = ["/proc/self/fd", "/dev/fd"]
        .iter()
        .find_map(|dir| std::fs::read_dir(dir).ok())
        .map(|dir| {
            dir.filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u64>().ok())
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0);
    soft.max(highest + 1) as libc::c_int
}

/// The daemon's end of a forked held child's gate.
pub struct Forked {
    pub pid: libc::pid_t,
    pub gate: OwnedFd,
}

/// Fork the held child. Every signal is blocked in this thread across the
/// fork, so no handler of the daemon's can run in the child before the
/// child has reset its dispositions.
pub fn fork_held(spec: &HeldSpec) -> io::Result<Forked> {
    let (daemon_end, child_end) = gate_pair()?;
    let child_fd = child_end.as_raw_fd();
    // SAFETY: sigset operations on local storage; fork in a process whose
    // child runs only async-signal-safe code (see `held_child`).
    unsafe {
        let mut all: libc::sigset_t = std::mem::zeroed();
        let mut previous: libc::sigset_t = std::mem::zeroed();
        libc::sigfillset(&mut all);
        libc::pthread_sigmask(libc::SIG_SETMASK, &all, &mut previous);
        let pid = libc::fork();
        if pid == 0 {
            held_child(spec, child_fd);
        }
        let error = io::Error::last_os_error();
        libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
        drop(child_end);
        if pid < 0 {
            return Err(error);
        }
        Ok(Forked {
            pid,
            gate: daemon_end,
        })
    }
}

/// A connected `AF_UNIX` stream pair, both ends close-on-exec at 10 or
/// above, so the child's end can never be 0, 1 or 2.
fn gate_pair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    // Close-on-exec from creation where the platform allows it, so no other
    // thread's fork and exec can inherit an end before it is re-homed.
    #[cfg(target_os = "linux")]
    let kind = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let kind = libc::SOCK_STREAM;
    // SAFETY: socketpair fills two descriptors.
    if unsafe { libc::socketpair(libc::AF_UNIX, kind, 0, fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let rehome = |fd: RawFd| -> io::Result<OwnedFd> {
        // SAFETY: fcntl and close on descriptors this call created.
        unsafe {
            let moved = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10);
            libc::close(fd);
            if moved < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(OwnedFd::from_raw_fd(moved))
        }
    };
    let first = rehome(fds[0]);
    let second = rehome(fds[1]);
    Ok((first?, second?))
}

/// Close every descriptor except `keep`.
///
/// # Safety
///
/// Async-signal-safe; called only in the forked child.
unsafe fn close_all_except(keep: RawFd, limit: libc::c_int) {
    #[cfg(target_os = "linux")]
    unsafe {
        let below = if keep > 0 {
            libc::syscall(libc::SYS_close_range, 0u32, (keep - 1) as u32, 0u32)
        } else {
            0
        };
        let above = libc::syscall(libc::SYS_close_range, (keep + 1) as u32, u32::MAX, 0u32);
        if below == 0 && above == 0 {
            return;
        }
    }
    for fd in 0..limit {
        if fd != keep {
            // SAFETY: close(2) is async-signal-safe.
            unsafe { libc::close(fd) };
        }
    }
}

/// Write the whole buffer to `fd`, retrying on `EINTR`.
///
/// # Safety
///
/// Async-signal-safe; called only in the forked child.
unsafe fn write_all_raw(fd: RawFd, bytes: &[u8]) -> bool {
    let mut at = 0;
    while at < bytes.len() {
        // SAFETY: writing from a live buffer.
        let n = unsafe { libc::write(fd, bytes[at..].as_ptr().cast(), bytes.len() - at) };
        if n > 0 {
            at += n as usize;
        } else if n < 0 && unsafe { errno() } == libc::EINTR {
            continue;
        } else {
            return false;
        }
    }
    true
}

#[cfg(target_os = "linux")]
unsafe fn errno() -> libc::c_int {
    unsafe { *libc::__errno_location() }
}

#[cfg(not(target_os = "linux"))]
unsafe fn errno() -> libc::c_int {
    unsafe { *libc::__error() }
}

/// Report a failed step on the gate and exit without running anything.
///
/// # Safety
///
/// Async-signal-safe; called only in the forked child.
unsafe fn fail(gate: RawFd, step: Step) -> ! {
    let code = unsafe { errno() }.to_le_bytes();
    let message = [FAILED, step as u8, code[0], code[1], code[2], code[3]];
    unsafe {
        write_all_raw(gate, &message);
        libc::_exit(PREPARE_FAILED);
    }
}

/// The child side. Only async-signal-safe calls, in the contract's order.
///
/// # Safety
///
/// Called only in the child of `fork_held`.
unsafe fn held_child(spec: &HeldSpec, gate: RawFd) -> ! {
    unsafe {
        if spec.stop_after_fork {
            let _ = pty_core::kill::process(libc::getpid(), libc::SIGSTOP);
        }
        // 1. Dispositions as an exec would leave them, then an empty mask.
        for &(signal, handler) in &spec.dispositions {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handler;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
        let mut empty: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut empty);
        libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());

        // 2. Nothing of the daemon's survives: not the control channel, the
        //    lock, the listener, the master, nor the gate's other end.
        close_all_except(gate, spec.fd_limit);

        // 3. A session and process group of its own.
        if libc::setsid() < 0 {
            fail(gate, Step::Setsid);
        }

        // 4. The slave as controlling terminal and as 0, 1 and 2.
        let tty = libc::open(spec.slave.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
        if tty < 0 {
            fail(gate, Step::OpenSlave);
        }
        #[allow(clippy::cast_lossless)]
        if libc::ioctl(tty, libc::TIOCSCTTY as _, 0) < 0 {
            fail(gate, Step::ControllingTty);
        }
        for target in 0..3 {
            if tty != target && libc::dup2(tty, target) < 0 {
                fail(gate, Step::Stdio);
            }
        }
        if tty > 2 {
            libc::close(tty);
        }

        // 5. Today's umask and working directory.
        libc::umask(spec.umask);
        if libc::chdir(spec.cwd.as_ptr()) < 0 {
            fail(gate, Step::Chdir);
        }

        // 6. Prepared.
        if spec.stop_before_ack {
            let _ = pty_core::kill::process(libc::getpid(), libc::SIGSTOP);
        }
        if !write_all_raw(gate, &[PREPARED]) {
            libc::_exit(NOT_RELEASED);
        }

        // 7. Held until released. Anything else ends it without exec.
        loop {
            let mut byte = 0u8;
            let n = libc::read(gate, (&mut byte as *mut u8).cast(), 1);
            if n == 1 && byte == RELEASE {
                break;
            }
            if n < 0 && errno() == libc::EINTR {
                continue;
            }
            libc::_exit(NOT_RELEASED);
        }
        libc::close(gate);
        libc::execve(
            spec.path.as_ptr(),
            spec.argv_ptrs.as_ptr(),
            spec.envp_ptrs.as_ptr(),
        );
        libc::_exit(127);
    }
}

/// What the child said on its gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ack {
    Prepared,
    Failed {
        step: &'static str,
        errno: i32,
    },
    /// End of file, or anything unreadable, before an acknowledgement.
    Lost,
}

/// Read the child's acknowledgement. Blocks until the child writes, exits,
/// or the daemon shuts the gate down.
pub fn read_ack(gate: &OwnedFd) -> Ack {
    let mut buf = [0u8; 6];
    let mut have = 0;
    loop {
        // SAFETY: reading into the unread tail of a local buffer.
        let n = unsafe {
            libc::read(
                gate.as_raw_fd(),
                buf[have..].as_mut_ptr().cast(),
                buf.len() - have,
            )
        };
        if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Ack::Lost;
        }
        if n == 0 {
            return Ack::Lost;
        }
        have += n as usize;
        match buf[0] {
            PREPARED => return Ack::Prepared,
            FAILED if have >= 6 => {
                return Ack::Failed {
                    step: Step::name(buf[1]),
                    errno: i32::from_le_bytes([buf[2], buf[3], buf[4], buf[5]]),
                };
            }
            FAILED => continue,
            _ => return Ack::Lost,
        }
    }
}

/// Write the release byte. `true` only means the byte was written.
pub fn release(gate: &OwnedFd) -> bool {
    let byte = [RELEASE];
    loop {
        // SAFETY: one byte from a local buffer to our own descriptor.
        let n = unsafe { libc::send(gate.as_raw_fd(), byte.as_ptr().cast(), 1, send_flags()) };
        if n == 1 {
            return true;
        }
        if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return false;
    }
}

#[cfg(target_os = "linux")]
fn send_flags() -> libc::c_int {
    libc::MSG_NOSIGNAL
}

#[cfg(not(target_os = "linux"))]
fn send_flags() -> libc::c_int {
    0
}

/// Close the gate in both directions: a held or preparing child reads end of
/// file and exits without exec, and a reader blocked in [`read_ack`] wakes.
pub fn shut(gate: &OwnedFd) {
    // SAFETY: shutdown on our own socket.
    unsafe {
        libc::shutdown(gate.as_raw_fd(), libc::SHUT_RDWR);
    }
}

/// The held child as `portable-pty`'s `Child`, so the session substrate can
/// own it exactly like a spawned one: the substrate only ever asks for its
/// pid and waits on it itself.
#[derive(Debug)]
pub struct HeldChild {
    pub pid: u32,
}

impl portable_pty::ChildKiller for HeldChild {
    fn kill(&mut self) -> io::Result<()> {
        // A signal to our own unreaped child.
        pty_core::kill::process(self.pid as i32, libc::SIGKILL)
    }

    fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
        Box::new(HeldChild { pid: self.pid })
    }
}

impl portable_pty::Child for HeldChild {
    fn try_wait(&mut self) -> io::Result<Option<portable_pty::ExitStatus>> {
        let mut status = 0;
        // SAFETY: waitpid on our own child.
        match unsafe { libc::waitpid(self.pid as i32, &mut status, libc::WNOHANG) } {
            0 => Ok(None),
            pid if pid > 0 => Ok(Some(exit_status(status))),
            _ => Err(io::Error::last_os_error()),
        }
    }

    fn wait(&mut self) -> io::Result<portable_pty::ExitStatus> {
        let mut status = 0;
        loop {
            // SAFETY: waitpid on our own child.
            let pid = unsafe { libc::waitpid(self.pid as i32, &mut status, 0) };
            if pid > 0 {
                return Ok(exit_status(status));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    fn process_id(&self) -> Option<u32> {
        Some(self.pid)
    }
}

fn exit_status(status: libc::c_int) -> portable_pty::ExitStatus {
    if libc::WIFSIGNALED(status) {
        portable_pty::ExitStatus::with_signal(&libc::WTERMSIG(status).to_string())
    } else {
        portable_pty::ExitStatus::with_exit_code(libc::WEXITSTATUS(status) as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spec_reproduces_todays_argv_and_environment() {
        let mut env = EnvMap::new();
        env.insert("PATH".into(), "/usr/bin:/bin".into());
        env.insert("HOME".into(), "/nonexistent-home".into());
        let spec = HeldSpec::build(
            "echo",
            &["a b".to_string()],
            "/",
            &env,
            0o022,
            std::path::Path::new("/dev/null"),
        )
        .unwrap();
        assert_eq!(
            spec.argv(),
            vec!["/bin/sh", "-c", "exec \"$@\"", "sh", "echo", "a b"]
        );
        let envp = spec.envp();
        assert!(envp.contains(&"PATH=/usr/bin:/bin".to_string()));
        assert!(envp.iter().any(|entry| entry.starts_with("SHELL=")));
        let mut sorted = envp.clone();
        sorted.sort();
        assert_eq!(envp, sorted);
    }

    #[test]
    fn a_session_shell_wins_over_the_looked_up_one() {
        let mut env = EnvMap::new();
        env.insert("SHELL".into(), "/not/a/shell".into());
        let spec = HeldSpec::build(
            "true",
            &[],
            "/",
            &env,
            0o022,
            std::path::Path::new("/dev/null"),
        )
        .unwrap();
        assert!(spec.envp().contains(&"SHELL=/not/a/shell".to_string()));
    }

    #[test]
    fn caught_signals_reset_and_ignored_ones_stay_ignored() {
        let table = exec_dispositions();
        let of = |signal| table.iter().find(|(s, _)| *s == signal).map(|(_, h)| *h);
        // The Rust runtime ignores SIGPIPE; std's Command resets it.
        assert_eq!(of(libc::SIGPIPE), Some(libc::SIG_DFL));
        assert_eq!(of(libc::SIGTERM), Some(libc::SIG_DFL));
        assert!(of(libc::SIGKILL).is_none());
        assert!(of(libc::SIGSTOP).is_none());
    }
}
