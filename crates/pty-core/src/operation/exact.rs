//! Signals that can only reach the process they were meant for, and waits
//! that end when that process does.
//!
//! A pid alone names whatever process holds it now. Recovery signals a
//! process it did not start, so it pins the process first and checks the
//! pinned one is the recorded identity before it signals:
//!
//! - **Linux:** a `pidfd`. Once open it names one process for good; a
//!   signal sent through it cannot reach a later holder of the pid.
//! - **macOS:** an audit token, taken from the process's task name port. The
//!   kernel delivers `proc_signal_with_audittoken` only while the token's
//!   pid version still names the same process.
//!
//! Where neither exists (a Linux kernel without `pidfd_open`, a macOS
//! without the call, a task port that is refused) nothing is signalled: the
//! caller reports the process as unknown and leaves it to a person.
//!
//! Waiting is reactive: a `pidfd` becomes readable, and a kqueue
//! `EVFILT_PROC`/`NOTE_EXIT` filter fires, when the process exits.

use std::time::Duration;

use super::record::{Liveness, ProcessId};

/// Why a process could not be pinned or signalled safely.
pub const UNSUPPORTED: &str = "identity-safe-signal-unavailable";

/// A process pinned so that a signal can only reach it.
pub struct Exact {
    #[cfg(target_os = "linux")]
    pidfd: std::os::fd::OwnedFd,
    #[cfg(target_os = "macos")]
    token: darwin::AuditToken,
    pid: i32,
}

/// Debug builds: a test can make identity-safe signalling unavailable, to
/// prove that nothing is then signalled at all.
fn unsupported_for_test() -> bool {
    cfg!(debug_assertions) && std::env::var("PTY_TEST_EXACT_SIGNAL").as_deref() == Ok("unsupported")
}

impl Exact {
    /// Pin `id`'s process. `Ok(None)` when it is already gone (or the pid
    /// now names another process); `Err` when it cannot be pinned safely.
    pub fn pin(id: &ProcessId, boot: Option<&str>) -> Result<Option<Exact>, &'static str> {
        let pid = id.pid as i32;
        let pinned = Self::pin_pid(pid)?;
        let Some(pinned) = pinned else {
            return Ok(None);
        };
        // Checked after pinning: a pid reused before the pin fails here, and
        // one reused after it cannot be reached through the pin.
        match id.liveness_now(boot) {
            Liveness::Live => Ok(Some(pinned)),
            Liveness::Gone => Ok(None),
            Liveness::Unknown(reason) => Err(reason),
        }
    }

    #[cfg(target_os = "linux")]
    fn pin_pid(pid: i32) -> Result<Option<Exact>, &'static str> {
        use std::os::fd::FromRawFd;
        // A test can stand in for a kernel without pidfd_open.
        let (raw, error) = if unsupported_for_test() {
            (-1, Some(libc::ENOSYS))
        } else {
            // SAFETY: pidfd_open returns a new descriptor or -1.
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
            (raw, std::io::Error::last_os_error().raw_os_error())
        };
        if raw >= 0 {
            // SAFETY: a descriptor this call just received.
            let pidfd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw as i32) };
            return Ok(Some(Exact { pidfd, pid }));
        }
        // No pidfd, no signal: a raw pid is never signalled instead.
        match error {
            Some(libc::ESRCH) => Ok(None),
            Some(libc::ENOSYS) => Err(UNSUPPORTED),
            _ => Err("pidfd-unavailable"),
        }
    }

    #[cfg(target_os = "macos")]
    fn pin_pid(pid: i32) -> Result<Option<Exact>, &'static str> {
        // Without the call there is no signal at all, never a raw-pid one.
        if unsupported_for_test() || !darwin::available() {
            return Err(UNSUPPORTED);
        }
        match darwin::token_of(pid) {
            Ok(token) => Ok(Some(Exact { token, pid })),
            Err(darwin::TokenError::Gone) => Ok(None),
            Err(darwin::TokenError::Refused) => Err("task-port-refused"),
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn pin_pid(_pid: i32) -> Result<Option<Exact>, &'static str> {
        Err(UNSUPPORTED)
    }

    /// Signal the pinned process. A process that has already exited is not
    /// an error.
    pub fn signal(&self, signal: i32) -> Result<(), &'static str> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: a signal through our own pidfd.
            let sent = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.pidfd.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
            if sent == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                Ok(())
            } else {
                Err("signal-refused")
            }
        }
        #[cfg(target_os = "macos")]
        {
            match darwin::signal(&self.token, signal) {
                0 => Ok(()),
                libc::ESRCH => Ok(()),
                _ => Err("signal-refused"),
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = signal;
            Err(UNSUPPORTED)
        }
    }

    /// Wait until the pinned process exits, for at most `timeout`. `true`
    /// when it did.
    pub fn wait_exit(&self, timeout: Duration) -> bool {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            poll_readable(self.pidfd.as_raw_fd(), timeout)
        }
        #[cfg(not(target_os = "linux"))]
        {
            wait_for_exit(self.pid, timeout) == Exit::Exited
        }
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }
}

#[cfg(target_os = "linux")]
fn poll_readable(fd: i32, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd on our own descriptor.
        let ready =
            unsafe { libc::poll(&mut pfd, 1, left.as_millis().min(i32::MAX as u128) as i32) };
        if ready > 0 {
            return true;
        }
        if ready == 0 {
            return false;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return false;
        }
    }
}

/// How a wait for a pid ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// It exited (or was never there).
    Exited,
    /// Still running when the time ran out.
    Running,
    /// This platform cannot wait for a process it did not start.
    Unsupported,
}

/// Wait, reactively, for `pid` to exit. A zombie has exited.
pub fn wait_for_exit(pid: i32, timeout: Duration) -> Exit {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::{AsRawFd, FromRawFd};
        // SAFETY: pidfd_open returns a new descriptor or -1.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        if raw < 0 {
            return match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::ESRCH) => Exit::Exited,
                _ => Exit::Unsupported,
            };
        }
        // SAFETY: a descriptor this call just received.
        let pidfd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw as i32) };
        if poll_readable(pidfd.as_raw_fd(), timeout) {
            Exit::Exited
        } else {
            Exit::Running
        }
    }
    #[cfg(target_os = "macos")]
    {
        darwin::wait_exit(pid, timeout)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (pid, timeout);
        Exit::Unsupported
    }
}

#[cfg(target_os = "macos")]
mod darwin {
    use super::Exit;
    use std::time::Duration;

    /// `audit_token_t`: eight words; word 5 is the pid, word 7 its version.
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct AuditToken {
        val: [u32; 8],
    }

    /// `<mach/task_info.h>`: TASK_AUDIT_TOKEN.
    const TASK_AUDIT_TOKEN: libc::task_flavor_t = 15;
    const TASK_AUDIT_TOKEN_COUNT: libc::mach_msg_type_number_t = 8;
    /// `<dlfcn.h>`: RTLD_DEFAULT on Darwin.
    const RTLD_DEFAULT: *mut libc::c_void = -2isize as *mut libc::c_void;

    unsafe extern "C" {
        /// What the C `mach_task_self()` macro reads.
        static mach_task_self_: libc::mach_port_t;
        fn task_name_for_pid(
            target: libc::mach_port_t,
            pid: libc::c_int,
            name: *mut libc::mach_port_t,
        ) -> libc::kern_return_t;
        fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
    }

    type SignalWithToken = unsafe extern "C" fn(*mut AuditToken, libc::c_int) -> libc::c_int;

    /// `proc_signal_with_audittoken` (macOS 13 and later), looked up at run
    /// time so a binary that lacks it does not fail to load: it refuses.
    fn signal_fn() -> Option<SignalWithToken> {
        // SAFETY: dlsym with a NUL-terminated name.
        let symbol = unsafe { libc::dlsym(RTLD_DEFAULT, c"proc_signal_with_audittoken".as_ptr()) };
        // SAFETY: the symbol has this signature where it exists.
        (!symbol.is_null())
            .then(|| unsafe { std::mem::transmute::<*mut libc::c_void, SignalWithToken>(symbol) })
    }

    pub fn available() -> bool {
        signal_fn().is_some()
    }

    pub enum TokenError {
        Gone,
        Refused,
    }

    pub fn token_of(pid: i32) -> Result<AuditToken, TokenError> {
        let mut name: libc::mach_port_t = 0;
        // SAFETY: mach calls with storage for their outputs.
        unsafe {
            let task = mach_task_self_;
            if task_name_for_pid(task, pid, &mut name) != 0 {
                // Refused for another user's process, or gone: tell apart.
                return Err(match crate::proctable::process(pid) {
                    crate::proctable::Answer::NotPresent => TokenError::Gone,
                    _ => TokenError::Refused,
                });
            }
            let mut token = AuditToken { val: [0; 8] };
            let mut count = TASK_AUDIT_TOKEN_COUNT;
            let kr = libc::task_info(
                name,
                TASK_AUDIT_TOKEN,
                token.val.as_mut_ptr() as libc::task_info_t,
                &mut count,
            );
            mach_port_deallocate(task, name);
            if kr != 0 || token.val[5] as i32 != pid {
                return Err(TokenError::Refused);
            }
            Ok(token)
        }
    }

    /// 0 when delivered, else the error number.
    pub fn signal(token: &AuditToken, signal: i32) -> i32 {
        let Some(send) = signal_fn() else {
            return libc::ENOSYS;
        };
        let mut token = *token;
        // SAFETY: a valid token and a signal number.
        let result = unsafe { send(&mut token, signal) };
        if result == -1 {
            std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO)
        } else {
            result
        }
    }

    pub fn wait_exit(pid: i32, timeout: Duration) -> Exit {
        // SAFETY: kqueue and kevent on storage owned here.
        unsafe {
            let kq = libc::kqueue();
            if kq < 0 {
                return Exit::Unsupported;
            }
            let change = libc::kevent {
                ident: pid as libc::uintptr_t,
                filter: libc::EVFILT_PROC,
                flags: libc::EV_ADD | libc::EV_ONESHOT,
                fflags: libc::NOTE_EXIT,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            let mut event: libc::kevent = std::mem::zeroed();
            let span = libc::timespec {
                tv_sec: timeout.as_secs() as libc::time_t,
                tv_nsec: timeout.subsec_nanos() as libc::c_long,
            };
            let n = libc::kevent(kq, &change, 1, &mut event, 1, &span);
            let error = std::io::Error::last_os_error().raw_os_error();
            libc::close(kq);
            if n < 0 {
                return if error == Some(libc::ESRCH) {
                    Exit::Exited
                } else {
                    Exit::Unsupported
                };
            }
            if n == 0 {
                return Exit::Running;
            }
            if event.flags & libc::EV_ERROR != 0 {
                return if event.data as i32 == libc::ESRCH {
                    Exit::Exited
                } else {
                    Exit::Unsupported
                };
            }
            Exit::Exited
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_pinned_process_is_signalled_and_its_exit_is_seen() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let id = ProcessId::of(child.id()).unwrap();
        let boot = crate::boot::read_boot_identity();
        let pinned = Exact::pin(&id, boot.as_deref()).unwrap().expect("live");
        assert!(!pinned.wait_exit(Duration::from_millis(50)));
        pinned.signal(libc::SIGKILL).unwrap();
        assert!(pinned.wait_exit(Duration::from_secs(5)));
        let _ = child.wait();
        assert!(
            Exact::pin(&id, boot.as_deref()).unwrap().is_none(),
            "gone after reaping"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_pid_with_another_start_identity_is_never_pinned() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let mut id = ProcessId::of(child.id()).unwrap();
        id.start_token = "linux:1".into();
        let boot = crate::boot::read_boot_identity();
        assert!(Exact::pin(&id, boot.as_deref()).unwrap().is_none());
        assert!(child.try_wait().unwrap().is_none());
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn waiting_for_an_exit_is_reactive() {
        let mut child = std::process::Command::new("sleep")
            .arg("0.2")
            .spawn()
            .unwrap();
        let started = std::time::Instant::now();
        assert_eq!(
            wait_for_exit(child.id() as i32, Duration::from_secs(10)),
            Exit::Exited
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        let _ = child.wait();
    }
}
