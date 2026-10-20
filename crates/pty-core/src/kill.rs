//! The only call to `kill(2)` in pty.
//!
//! `kill(2)` reads its pid argument as more than a process id. 0 is the
//! caller's own process group. -1 is every process the caller may signal: for
//! an ordinary user, everything that user runs, including a systemd user
//! manager, which stops every one of that user's services on SIGTERM. Any
//! other negative value is a process group, so a group id of 1 is the same
//! broadcast. Pid 1 is init. pty never means any of these, but a pid read from
//! a file, parsed from JSON or cast from `u32` can still come out as one, and
//! one such signal stops every seat on the host.
//!
//! So every signal pty sends goes through [`send`], which refuses those
//! targets before the system call, whatever the caller computed. A refusal is
//! `EINVAL`, the error `kill(2)` itself gives for a signal it will not send. A
//! test in this module fails if any other file in the workspace calls
//! `kill(2)` directly.
//!
//! [`send`] is async-signal-safe: it allocates nothing, so the held child of
//! a launch operation can call it between fork and exec.

use std::io;

/// What a signal is for: one process, or one process group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Process(libc::pid_t),
    Group(libc::pid_t),
}

/// The pid argument `kill(2)` gets for `target`, or `None` when that
/// argument would reach more than the one process or group meant.
///
/// Signal 0 sends nothing; it only asks whether a pid exists. So signal 0 may
/// still ask about init, and liveness checks keep their answer for pid 1.
/// It may not ask about 0 or below, which name groups.
fn raw_target(target: Target, signal: libc::c_int) -> Option<libc::pid_t> {
    match target {
        Target::Process(pid) if pid > 1 || (pid == 1 && signal == 0) => Some(pid),
        Target::Group(pgid) if pgid > 1 => Some(-pgid),
        _ => None,
    }
}

/// `kill(2)` on `target`, refusing pid 1 and below, and group 1 and below,
/// with `EINVAL`.
pub fn send(target: Target, signal: libc::c_int) -> io::Result<()> {
    let Some(raw) = raw_target(target, signal) else {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    };
    // SAFETY: kill(2) has no memory-safety preconditions; `raw_target`
    // limited it to one process or one group.
    if unsafe { libc::kill(raw, signal) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// [`send`] to one process.
pub fn process(pid: libc::pid_t, signal: libc::c_int) -> io::Result<()> {
    send(Target::Process(pid), signal)
}

/// [`send`] to one process group.
pub fn group(pgid: libc::pid_t, signal: libc::c_int) -> io::Result<()> {
    send(Target::Group(pgid), signal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const SIGNALS: [libc::c_int; 6] = [
        libc::SIGTERM,
        libc::SIGKILL,
        libc::SIGINT,
        libc::SIGHUP,
        libc::SIGSTOP,
        libc::SIGCONT,
    ];

    /// Decided without a system call: a broken guard here must fail an
    /// assertion, not signal everything the test's user runs.
    #[test]
    fn broadcasts_groups_and_init_are_never_targets() {
        for signal in SIGNALS {
            for pid in [i32::MIN, -2, -1, 0, 1] {
                assert_eq!(raw_target(Target::Process(pid), signal), None, "pid {pid}");
                assert_eq!(raw_target(Target::Group(pid), signal), None, "group {pid}");
            }
            assert_eq!(raw_target(Target::Process(2), signal), Some(2));
            assert_eq!(raw_target(Target::Group(2), signal), Some(-2));
            assert_eq!(raw_target(Target::Group(i32::MAX), signal), Some(-i32::MAX));
        }
        assert_eq!(raw_target(Target::Process(1), 0), Some(1));
        for pid in [-1, 0] {
            assert_eq!(raw_target(Target::Process(pid), 0), None);
        }
        assert_eq!(raw_target(Target::Group(1), 0), None);
    }

    /// What a pid stored as `u32` becomes when it is cast for `kill(2)`.
    #[test]
    fn a_wrapped_unsigned_pid_is_refused() {
        assert_eq!(u32::MAX as libc::pid_t, -1);
        assert_eq!(raw_target(Target::Process(u32::MAX as libc::pid_t), libc::SIGTERM), None);
        assert_eq!(raw_target(Target::Group(u32::MAX as libc::pid_t), libc::SIGTERM), None);
    }

    /// Signal 0 only: even if the guard failed, nothing would be delivered.
    #[test]
    fn a_refusal_is_einval_before_any_system_call() {
        for target in [
            Target::Process(-1),
            Target::Process(0),
            Target::Group(1),
            Target::Group(0),
        ] {
            let error = send(target, 0).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::EINVAL), "{target:?}");
        }
        // Init still answers a liveness question, as `kill(1, 0)` always did.
        let init = process(1, 0);
        assert!(
            init.is_ok() || init.as_ref().unwrap_err().raw_os_error() == Some(libc::EPERM),
            "{init:?}"
        );
    }

    #[test]
    fn one_process_and_one_group_are_reached() {
        let mut alone = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        process(alone.id() as libc::pid_t, libc::SIGKILL).unwrap();
        assert!(!alone.wait().unwrap().success());

        use std::os::unix::process::CommandExt;
        let mut leader = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        group(leader.id() as libc::pid_t, libc::SIGKILL).unwrap();
        assert!(!leader.wait().unwrap().success());
        let gone = group(leader.id() as libc::pid_t, 0).unwrap_err();
        assert_eq!(gone.raw_os_error(), Some(libc::ESRCH));
    }

    /// The regression test for the guard itself: a direct `kill(2)` anywhere
    /// else in the workspace bypasses it.
    #[test]
    fn no_other_file_calls_kill_directly() {
        let this = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/kill.rs");
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let mut found = Vec::new();
        let mut scanned = 0;
        let mut pending = vec![crates.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|name| name != "target") {
                        pending.push(path);
                    }
                    continue;
                }
                if path.extension().is_none_or(|ext| ext != "rs") || path == this {
                    continue;
                }
                scanned += 1;
                let text = std::fs::read_to_string(&path).unwrap();
                for (number, line) in text.lines().enumerate() {
                    if ["libc::kill", "SYS_kill", "signal::kill"]
                        .iter()
                        .any(|call| line.contains(call))
                    {
                        found.push(format!("{}:{}: {}", path.display(), number + 1, line.trim()));
                    }
                }
            }
        }
        // The scan must have read the workspace, or it proves nothing.
        assert!(scanned > 100, "scanned only {scanned} files under {}", crates.display());
        assert!(found.is_empty(), "call pty_core::kill instead:\n{}", found.join("\n"));
    }
}
