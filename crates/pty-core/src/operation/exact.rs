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
//! `EVFILT_PROC`/`NOTE_EXIT` filter fires, when the process exits. Both are
//! armed while the process still runs, before anything that can end it (see
//! [`ExitWatch`]).

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
    /// Armed at the pin, before any signal can end the process.
    #[cfg(target_os = "macos")]
    watch: ExitWatch,
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
        let token = match darwin::token_of(pid) {
            Ok(token) => token,
            Err(darwin::TokenError::Gone) => return Ok(None),
            Err(darwin::TokenError::Refused) => return Err("task-port-refused"),
        };
        // A process that cannot be watched is not signalled: its exit could
        // never be seen, only guessed at.
        let watch = ExitWatch::arm(pid);
        match watch.state {
            WatchState::Armed(_) => Ok(Some(Exact { token, watch, pid })),
            WatchState::Ended => Ok(None),
            WatchState::Unsupported => Err("exit-watch-unavailable"),
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
    /// only when the exit itself was observed, on the pidfd or on the watch
    /// armed with the pin (see [`ExitWatch`]).
    pub fn wait_exit(&self, timeout: Duration) -> bool {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            poll_readable(self.pidfd.as_raw_fd(), timeout)
        }
        #[cfg(target_os = "macos")]
        {
            self.watch.wait(timeout) == Exit::Exited
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
///
/// The watch is armed by this call, on whatever process holds `pid` then,
/// and a process that began to exit before it reads as exited at once. A
/// caller about to end a process arms an [`ExitWatch`] first, and takes its
/// [`ExitWatch::observed`] as the exit.
pub fn wait_for_exit(pid: i32, timeout: Duration) -> Exit {
    ExitWatch::arm(pid).wait(timeout)
}

/// A watch on one process's exit, armed while the process still runs.
///
/// **Arm it before the action that can end the process, and take what it
/// observes as the exit.** The event is the kernel's own: a Linux `pidfd`
/// becomes readable, and a macOS `NOTE_EXIT` is posted, only once every
/// thread of the process has terminated (in XNU, from `proc_exit`, which the
/// last thread runs). A watch armed on the running process is bound to it,
/// whichever process holds the pid by the time anyone waits.
///
/// Two weaker answers are not an observed exit, and [`ExitWatch::observed`]
/// keeps them apart:
///
/// - **A watch armed too late.** On macOS a filter attached to a process
///   that has begun to exit is refused with `ESRCH` from the moment its exit
///   is prepared, while its other threads may still be finishing system
///   calls. Measured on macOS 27 (2026-10-14), that refusal came in most
///   rounds once 20 µs separated a `SIGKILL` from the attach.
/// - **The process table.** On macOS it says `SRUN` from the start of an
///   exit until `SZOMB`, which is set a moment after `NOTE_EXIT`: right after
///   an exit was seen, `p_stat` still read `SRUN` in 1,265 of 2,400 rounds,
///   and in 1 of 300 even after the filter had fired. Re-reading it after
///   the event can call an exited process live.
pub struct ExitWatch {
    state: WatchState,
}

enum WatchState {
    #[cfg(target_os = "linux")]
    Armed(std::os::fd::OwnedFd),
    #[cfg(target_os = "macos")]
    Armed(darwin::Watch),
    /// It had exited, or begun to, before the watch was armed.
    Ended,
    /// This platform cannot watch a process it did not start.
    Unsupported,
}

impl ExitWatch {
    /// Arm a watch on `pid`'s exit.
    pub fn arm(pid: i32) -> ExitWatch {
        let state = Self::arm_state(pid);
        ExitWatch { state }
    }

    #[cfg(target_os = "linux")]
    fn arm_state(pid: i32) -> WatchState {
        use std::os::fd::FromRawFd;
        // SAFETY: pidfd_open returns a new descriptor or -1.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        if raw < 0 {
            return match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::ESRCH) => WatchState::Ended,
                _ => WatchState::Unsupported,
            };
        }
        // SAFETY: a descriptor this call just received.
        WatchState::Armed(unsafe { std::os::fd::OwnedFd::from_raw_fd(raw as i32) })
    }

    #[cfg(target_os = "macos")]
    fn arm_state(pid: i32) -> WatchState {
        match darwin::Watch::arm(pid) {
            Ok(watch) => WatchState::Armed(watch),
            Err(Exit::Exited) => WatchState::Ended,
            Err(_) => WatchState::Unsupported,
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn arm_state(_pid: i32) -> WatchState {
        WatchState::Unsupported
    }

    /// Has this watch observed the exit itself? Only a watch armed on the
    /// running process can: one armed too late answers `false` here, though
    /// [`ExitWatch::wait`] reads it as exited.
    pub fn observed(&self) -> bool {
        !matches!(self.state, WatchState::Ended | WatchState::Unsupported)
            && self.wait(Duration::ZERO) == Exit::Exited
    }

    /// Wait at most `timeout` for the exit. A watch that has seen it says so
    /// again at once, and so does one armed too late.
    pub fn wait(&self, timeout: Duration) -> Exit {
        match &self.state {
            #[cfg(target_os = "linux")]
            WatchState::Armed(pidfd) => {
                use std::os::fd::AsRawFd;
                if poll_readable(pidfd.as_raw_fd(), timeout) {
                    Exit::Exited
                } else {
                    Exit::Running
                }
            }
            #[cfg(target_os = "macos")]
            WatchState::Armed(watch) => watch.wait(timeout),
            WatchState::Ended => Exit::Exited,
            WatchState::Unsupported => {
                let _ = timeout;
                Exit::Unsupported
            }
        }
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

    /// A kqueue with a `NOTE_EXIT` filter attached to one process.
    pub struct Watch {
        kq: std::os::fd::OwnedFd,
        fired: std::sync::atomic::AtomicBool,
    }

    impl Watch {
        /// Attach the filter now. `Err(Exit::Exited)` when the process has
        /// exited or begun to: the kernel refuses the filter with `ESRCH`.
        pub fn arm(pid: i32) -> Result<Watch, Exit> {
            use std::os::fd::{AsRawFd, FromRawFd};
            // SAFETY: kqueue returns a new descriptor or -1.
            let raw = unsafe { libc::kqueue() };
            if raw < 0 {
                return Err(Exit::Unsupported);
            }
            // SAFETY: a descriptor this call just received.
            let kq = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
            let change = libc::kevent {
                ident: pid as libc::uintptr_t,
                filter: libc::EVFILT_PROC,
                flags: libc::EV_ADD | libc::EV_ONESHOT,
                fflags: libc::NOTE_EXIT,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            // With no room for events, a change that fails is returned as
            // -1 and errno.
            // SAFETY: one change on our own kqueue, no events read.
            let n = unsafe {
                libc::kevent(
                    kq.as_raw_fd(),
                    &change,
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                )
            };
            if n < 0 {
                return Err(match std::io::Error::last_os_error().raw_os_error() {
                    Some(libc::ESRCH) => Exit::Exited,
                    _ => Exit::Unsupported,
                });
            }
            Ok(Watch {
                kq,
                fired: std::sync::atomic::AtomicBool::new(false),
            })
        }

        pub fn wait(&self, timeout: Duration) -> Exit {
            use std::os::fd::AsRawFd;
            use std::sync::atomic::Ordering;
            if self.fired.load(Ordering::SeqCst) {
                return Exit::Exited;
            }
            let deadline = std::time::Instant::now() + timeout;
            loop {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                let span = libc::timespec {
                    tv_sec: left.as_secs() as libc::time_t,
                    tv_nsec: left.subsec_nanos() as libc::c_long,
                };
                // SAFETY: an all-zero kevent is a valid value to overwrite.
                let mut event: libc::kevent = unsafe { std::mem::zeroed() };
                // SAFETY: reads at most one event from our own kqueue.
                let n = unsafe {
                    libc::kevent(
                        self.kq.as_raw_fd(),
                        std::ptr::null(),
                        0,
                        &mut event,
                        1,
                        &span,
                    )
                };
                if n > 0 {
                    self.fired.store(true, Ordering::SeqCst);
                    return Exit::Exited;
                }
                if n == 0 {
                    return Exit::Running;
                }
                if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    return Exit::Unsupported;
                }
            }
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

    /// **A watch armed before the gate shuts observes the child's exit**:
    /// the daemon's order. The child is `cat`, which leaves at once on end of
    /// file, as a held child does. Armed after the gate shuts instead, the
    /// watch is often refused because the exit has begun, and observes
    /// nothing.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_watch_armed_before_the_gate_shuts_observes_the_exit() {
        use std::io::BufRead;
        for round in 0..300 {
            let mut child = std::process::Command::new("sh")
                .args(["-c", "echo held; exec cat"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut prepared = String::new();
            std::io::BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut prepared)
                .unwrap();
            let watch = ExitWatch::arm(child.id() as i32);
            drop(child.stdin.take());
            assert_eq!(
                watch.wait(Duration::from_secs(5)),
                Exit::Exited,
                "round {round}"
            );
            assert!(watch.observed(), "round {round}");
            let _ = child.wait();
        }
    }

    /// A watch armed on the running process observes its exit, and says so
    /// again at once. One armed on a pid already reaped reads as exited but
    /// observed nothing.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_watch_armed_before_the_end_observes_it() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let watch = ExitWatch::arm(child.id() as i32);
        assert_eq!(watch.wait(Duration::from_millis(50)), Exit::Running);
        assert!(!watch.observed());
        child.kill().unwrap();
        assert_eq!(watch.wait(Duration::from_secs(5)), Exit::Exited);
        assert!(watch.observed());
        let started = std::time::Instant::now();
        assert_eq!(watch.wait(Duration::from_secs(5)), Exit::Exited);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "seen once is seen"
        );
        let _ = child.wait();
        let late = ExitWatch::arm(child.id() as i32);
        assert_eq!(late.wait(Duration::from_secs(1)), Exit::Exited);
        assert!(!late.observed(), "a reaped pid is no observed exit");
    }
}
