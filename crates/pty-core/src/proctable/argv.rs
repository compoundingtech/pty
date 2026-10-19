//! The argument vectors of one user's processes, read in one pass.
//!
//! The launch-operation closure proof needs to find every process that still
//! carries an operation's birth marker in its argv, including a forked child
//! that has not exec'd yet and so still shows its parent's arguments. This is
//! that read: every live process of one uid, with its start identity and its
//! argv, and an honest account of what could not be read. A live process of
//! that uid whose argv cannot be read is reported as unreadable, never
//! skipped, because it might be exactly the process the caller looks for.
//!
//! **The domain is processes whose real and effective uid are both `uid`.**
//! No participant changes uid before its release, so a process with any other
//! effective uid is not one. That matters on macOS, where a setuid process
//! such as `login` (real uid the user's, effective uid root) refuses both
//! `proc_pidinfo` and its argv to the user: inside the domain it would make
//! every scan unknown.

use super::LiveIdentity;

/// One live process of the scanned uid.
#[derive(Debug, Clone, PartialEq)]
pub struct ArgvRow {
    pub pid: i32,
    pub identity: Option<LiveIdentity>,
    pub argv: Vec<String>,
}

/// One pass over the process table for one uid.
#[derive(Debug, Clone, PartialEq)]
pub struct ArgvScan {
    pub uid: u32,
    /// `/proc/<pid>/cmdline` or `KERN_PROCARGS2`.
    pub source: &'static str,
    /// The listing itself could be read.
    pub readable: bool,
    /// Every listed process was classified: no listing entry was lost and no
    /// process's owner could not be told.
    pub complete: bool,
    /// Processes of any uid the listing produced.
    pub listed: usize,
    /// Zombies of the scanned uid: exited, so never a match.
    pub zombies: usize,
    /// Kernel threads of the scanned uid (root's, on Linux): no argv, and
    /// never a participant, so never a match.
    pub kernel_threads: usize,
    /// Live processes of the scanned uid whose argv could not be read.
    pub unreadable: Vec<i32>,
    pub rows: Vec<ArgvRow>,
}

impl ArgvScan {
    fn unreadable_listing(uid: u32, source: &'static str) -> ArgvScan {
        ArgvScan {
            uid,
            source,
            readable: false,
            complete: false,
            listed: 0,
            zombies: 0,
            kernel_threads: 0,
            unreadable: Vec::new(),
            rows: Vec::new(),
        }
    }

    /// Readable, complete, and nothing in the domain was unreadable.
    pub fn is_whole(&self) -> bool {
        self.readable && self.complete && self.unreadable.is_empty()
    }
}

/// Scan every live process whose real and effective uid are `uid`.
pub fn scan_argv(uid: u32) -> ArgvScan {
    #[cfg(target_os = "linux")]
    {
        linux::scan(uid)
    }
    #[cfg(target_os = "macos")]
    {
        macos::scan(uid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        ArgvScan::unreadable_listing(uid, "unsupported")
    }
}

/// Split a NUL-separated argument block. A trailing NUL ends the last
/// argument; it does not start an empty one.
pub fn split_nul_args(bytes: &[u8]) -> Vec<String> {
    let trimmed = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed
        .split(|b| *b == 0)
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect()
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    pub(super) fn scan(uid: u32) -> ArgvScan {
        const SOURCE: &str = "/proc/<pid>/cmdline";
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return ArgvScan::unreadable_listing(uid, SOURCE);
        };
        let mut scan = ArgvScan {
            uid,
            source: SOURCE,
            readable: true,
            complete: true,
            listed: 0,
            zombies: 0,
            kernel_threads: 0,
            unreadable: Vec::new(),
            rows: Vec::new(),
        };
        let own = std::process::id() as i32;
        let mut saw_own = false;
        for entry in dir {
            let Ok(entry) = entry else {
                scan.complete = false;
                continue;
            };
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<i32>().ok())
            else {
                continue;
            };
            scan.listed += 1;
            saw_own |= pid == own;
            match classify(pid, uid) {
                Class::Gone | Class::OtherUser => {}
                Class::Zombie => scan.zombies += 1,
                Class::KernelThread => scan.kernel_threads += 1,
                Class::Live(row) => scan.rows.push(row),
                Class::Unreadable => scan.unreadable.push(pid),
                Class::OwnerUnknown => scan.complete = false,
            }
        }
        if !saw_own {
            return ArgvScan::unreadable_listing(uid, SOURCE);
        }
        scan.rows.sort_by_key(|row| row.pid);
        scan.unreadable.sort_unstable();
        scan
    }

    enum Class {
        Gone,
        OtherUser,
        Zombie,
        KernelThread,
        Live(ArgvRow),
        Unreadable,
        OwnerUnknown,
    }

    fn gone(error: &std::io::Error) -> bool {
        error.kind() == std::io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
    }

    /// Whether both the real and the effective uid are `uid`, the state,
    /// and, where the kernel says (`Kthread:`), whether it is a kernel
    /// thread, from `/proc/<pid>/status`.
    fn owner_and_state(pid: i32, uid: u32) -> Result<(bool, char, Option<bool>), std::io::Error> {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
        let mut owned = None;
        let mut state = None;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("Uid:") {
                let mut ids = rest.split_whitespace().map(|v| v.parse::<u32>().ok());
                owned = match (ids.next().flatten(), ids.next().flatten()) {
                    (Some(real), Some(effective)) => Some(real == uid && effective == uid),
                    _ => None,
                };
            } else if let Some(rest) = line.strip_prefix("State:") {
                state = rest.trim_start().chars().next();
            }
        }
        match (owned, state) {
            (Some(owned), Some(state)) => Ok((owned, state, status_kernel_thread(&status))),
            _ => Err(std::io::Error::other("status without Uid or State")),
        }
    }

    /// The `Kthread:` line of `/proc/<pid>/status`, which newer kernels
    /// print. `None` where the line is absent or unparsable.
    pub(super) fn status_kernel_thread(status: &str) -> Option<bool> {
        let rest = status
            .lines()
            .find_map(|line| line.strip_prefix("Kthread:"))?;
        match rest.trim() {
            "1" => Some(true),
            "0" => Some(false),
            _ => None,
        }
    }

    /// `PF_KTHREAD` in the flags of `/proc/<pid>/stat` (field 9), which every
    /// kernel prints. The kernel sets it; a user process cannot.
    pub(super) fn stat_kernel_thread(stat: &str) -> Option<bool> {
        const PF_KTHREAD: u64 = 0x0020_0000;
        let tail = &stat[stat.rfind(')')? + 1..];
        // tail's first field is field 3 (state), so field 9 is its seventh.
        let flags: u64 = tail.split_whitespace().nth(6)?.parse().ok()?;
        Some(flags & PF_KTHREAD != 0)
    }

    fn classify(pid: i32, uid: u32) -> Class {
        let (owned, state, kthread) = match owner_and_state(pid, uid) {
            Ok(found) => found,
            Err(error) if gone(&error) => return Class::Gone,
            Err(_) => return Class::OwnerUnknown,
        };
        if !owned {
            return Class::OtherUser;
        }
        if state == 'Z' {
            return Class::Zombie;
        }
        let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => Some(stat),
            Err(error) if gone(&error) => return Class::Gone,
            Err(_) => None,
        };
        // A kernel thread (root's) has an empty cmdline and is never a
        // participant: outside the domain, not unreadable. Only the kernel's
        // own word counts; a process neither source can place stays in it.
        if kthread.or_else(|| stat.as_deref().and_then(stat_kernel_thread)) == Some(true) {
            return Class::KernelThread;
        }
        let identity = stat
            .as_deref()
            .and_then(|stat| crate::proctable::parse_proc_stat(pid, stat))
            .and_then(|row| row.identity);
        match std::fs::read(format!("/proc/{pid}/cmdline")) {
            Ok(bytes) if !bytes.is_empty() => Class::Live(ArgvRow {
                pid,
                identity,
                argv: split_nul_args(&bytes),
            }),
            // An empty cmdline from a live user process is a process on its
            // way out (its memory already released). Ask again what it is.
            Ok(_) => match owner_and_state(pid, uid) {
                Err(error) if gone(&error) => Class::Gone,
                Ok((_, 'Z', _)) => Class::Zombie,
                _ => Class::Unreadable,
            },
            Err(error) if gone(&error) => Class::Gone,
            Err(_) => Class::Unreadable,
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;

    /// <sys/proc.h>: SZOMB.
    const SZOMB: u32 = 5;

    pub(super) fn scan(uid: u32) -> ArgvScan {
        const SOURCE: &str = "KERN_PROCARGS2";
        let Some((pids, listed_whole)) = crate::proctable::list_all_pids() else {
            return ArgvScan::unreadable_listing(uid, SOURCE);
        };
        let own = std::process::id() as i32;
        if !pids.contains(&own) {
            return ArgvScan::unreadable_listing(uid, SOURCE);
        }
        let mut scan = ArgvScan {
            uid,
            source: SOURCE,
            readable: true,
            complete: listed_whole,
            listed: 0,
            zombies: 0,
            kernel_threads: 0,
            unreadable: Vec::new(),
            rows: Vec::new(),
        };
        let arg_max = arg_max();
        for pid in pids.into_iter().filter(|&p| p > 0) {
            scan.listed += 1;
            let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
            let want = size_of::<libc::proc_bsdinfo>() as i32;
            // SAFETY: `info` is the struct `PROC_PIDTBSDINFO` fills.
            let got = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTBSDINFO,
                    0,
                    &mut info as *mut _ as *mut libc::c_void,
                    want,
                )
            };
            let identity = if got == want {
                if info.pbi_ruid != uid || info.pbi_uid != uid {
                    continue;
                }
                if info.pbi_status == SZOMB {
                    scan.zombies += 1;
                    continue;
                }
                Some(LiveIdentity::new(format!(
                    "darwin:{}.{:06}",
                    info.pbi_start_tvsec, info.pbi_start_tvusec
                )))
            } else {
                // `proc_pidinfo` refuses an unreaped child and a process
                // running with another effective uid (a setuid `login`, say);
                // the kinfo reader still answers for both.
                match crate::proctable::kinfo_owner(pid) {
                    crate::proctable::Answer::NotPresent => continue,
                    crate::proctable::Answer::Known((ruid, euid, _))
                        if ruid != uid || euid != uid =>
                    {
                        continue;
                    }
                    crate::proctable::Answer::Known((_, _, true)) => {
                        scan.zombies += 1;
                        continue;
                    }
                    crate::proctable::Answer::Known(_) => crate::proctable::process(pid)
                        .known()
                        .and_then(|row| row.identity),
                    crate::proctable::Answer::Unknown(_) => {
                        scan.complete = false;
                        continue;
                    }
                }
            };
            match procargs(pid, arg_max) {
                Ok(argv) => scan.rows.push(ArgvRow {
                    pid,
                    identity,
                    argv,
                }),
                Err(libc::ESRCH) => {}
                Err(_) => {
                    // EINVAL is also how a process that just became a zombie
                    // answers. Ask once more what it is.
                    match crate::proctable::process(pid) {
                        crate::proctable::Answer::Known(row) if row.is_zombie() => {
                            scan.zombies += 1
                        }
                        crate::proctable::Answer::NotPresent => {}
                        _ => scan.unreadable.push(pid),
                    }
                }
            }
        }
        scan.rows.sort_by_key(|row| row.pid);
        scan.unreadable.sort_unstable();
        scan
    }

    fn arg_max() -> usize {
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        let mut value: libc::c_int = 0;
        let mut len = size_of::<libc::c_int>();
        // SAFETY: a two-element MIB and an int-sized buffer.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                &mut value as *mut _ as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc == 0 && value > 0 {
            value as usize
        } else {
            1 << 20
        }
    }

    /// `KERN_PROCARGS2`: `int argc`, the exec path, NUL padding, then `argc`
    /// NUL-terminated arguments (then the environment, which is not read).
    fn procargs(pid: i32, arg_max: usize) -> Result<Vec<String>, i32> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut buf = vec![0u8; arg_max];
        let mut len = buf.len();
        // SAFETY: a three-element MIB and a buffer whose length is passed.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr() as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO));
        }
        buf.truncate(len);
        parse_procargs2(&buf).ok_or(libc::EINVAL)
    }

    pub(super) fn parse_procargs2(buf: &[u8]) -> Option<Vec<String>> {
        let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?);
        if argc < 0 {
            return None;
        }
        let mut at = 4;
        // The exec path, then its NUL padding.
        while *buf.get(at)? != 0 {
            at += 1;
        }
        while at < buf.len() && buf[at] == 0 {
            at += 1;
        }
        let mut argv = Vec::with_capacity(argc as usize);
        for _ in 0..argc {
            let start = at;
            while *buf.get(at)? != 0 {
                at += 1;
            }
            argv.push(String::from_utf8_lossy(&buf[start..at]).into_owned());
            at += 1;
        }
        Some(argv)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nul_separated_arguments_split_exactly() {
        assert_eq!(split_nul_args(b"pty\0run\0--\0"), vec!["pty", "run", "--"]);
        assert_eq!(split_nul_args(b"a\0\0b\0"), vec!["a", "", "b"]);
        assert!(split_nul_args(b"").is_empty());
    }

    /// `kthreadd`'s own lines on Linux 7.0, and a user process's.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_kernel_thread_is_told_by_the_kernels_own_word() {
        use super::linux::{stat_kernel_thread, status_kernel_thread};
        let kthreadd_status =
            "Name:\tkthreadd\nState:\tS (sleeping)\nUid:\t0\t0\t0\t0\nKthread:\t1\n";
        let kthreadd_stat = "2 (kthreadd) S 0 0 0 0 -1 2129984 0 0 0 0 0 321 0 0 20 0 1 0 15 0 0 18446744073709551615 0";
        assert_eq!(status_kernel_thread(kthreadd_status), Some(true));
        assert_eq!(stat_kernel_thread(kthreadd_stat), Some(true));

        // Older kernels print no `Kthread:` line; the stat flags still say.
        let old_status = "Name:\tkworker/0:1\nState:\tI (idle)\nUid:\t0\t0\t0\t0\n";
        assert_eq!(status_kernel_thread(old_status), None);

        let user_status = "Name:\tsh\nState:\tS (sleeping)\nUid:\t0\t0\t0\t0\nKthread:\t0\n";
        let user_stat = "4242 (sh) S 1 4242 4242 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 99 0 0";
        assert_eq!(status_kernel_thread(user_status), Some(false));
        assert_eq!(stat_kernel_thread(user_stat), Some(false));

        // A name with a parenthesis and spaces cannot shift the flags field.
        let tricky = "4243 (a) 0 0 0 0 0 2129984 (b) S 1 1 1 0 -1 4194560 0 0";
        assert_eq!(stat_kernel_thread(tricky), Some(false));
        assert_eq!(stat_kernel_thread("4244 (truncated"), None);
    }

    /// **Root's scan leaves kernel threads out of the domain.** They are
    /// live, uid 0, and have an empty cmdline; counted as unreadable they
    /// would keep every root operation from settling closed.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_scan_of_root_counts_kernel_threads_outside_the_domain() {
        let kernel: Vec<i32> = std::fs::read_dir("/proc")
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
            .filter(|pid| {
                std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .ok()
                    .and_then(|stat| super::linux::stat_kernel_thread(&stat))
                    == Some(true)
            })
            .collect();
        if std::fs::read_to_string("/proc/2/comm").is_ok_and(|comm| comm.trim() == "kthreadd") {
            assert!(
                !kernel.is_empty(),
                "kthreadd is pid 2, so kernel threads are visible"
            );
        } else {
            eprintln!(
                "no kthreadd at pid 2 (a pid namespace): only the absence of kernel threads among the unreadable is checked"
            );
        }
        let scan = scan_argv(0);
        assert!(scan.readable);
        let unreadable_kernel: Vec<_> = scan
            .unreadable
            .iter()
            .filter(|pid| kernel.contains(pid))
            .collect();
        assert!(
            unreadable_kernel.is_empty(),
            "kernel threads read as unreadable: {unreadable_kernel:?}"
        );
        assert!(scan.rows.iter().all(|row| !kernel.contains(&row.pid)));
        if !kernel.is_empty() {
            assert!(scan.kernel_threads > 0);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_scan_of_this_user_finds_this_process_with_its_argv() {
        // SAFETY: getuid(2) has no preconditions.
        let uid = unsafe { libc::getuid() };
        let scan = scan_argv(uid);
        assert!(scan.readable);
        let own = std::process::id() as i32;
        let row = scan
            .rows
            .iter()
            .find(|row| row.pid == own)
            .expect("the scanning process is a live process of its own uid");
        let expected: Vec<String> = std::env::args().collect();
        assert_eq!(row.argv, expected);
        assert!(row.identity.is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_forked_child_shows_its_parents_argv_until_it_execs() {
        // SAFETY: the child only closes descriptors and sleeps in pause(2),
        // and is killed below.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            unsafe {
                // A fork copies every descriptor of every test thread, flock
                // holders included. Keep none of them alive in this child.
                for fd in 3..4096 {
                    libc::close(fd);
                }
                libc::pause();
                libc::_exit(0);
            }
        }
        let uid = unsafe { libc::getuid() };
        let scan = scan_argv(uid);
        let expected: Vec<String> = std::env::args().collect();
        let found = scan
            .rows
            .iter()
            .find(|row| row.pid == pid)
            .map(|row| row.argv.clone());
        crate::kill::process(pid, libc::SIGKILL).unwrap();
        unsafe {
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
        assert_eq!(found, Some(expected));
    }
}
