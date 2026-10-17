//! Node-compatible file locks: a no-replace claim, decimal holder pid,
//! one stale steal, and release by unlink. Rust builds the 0600 owner inode
//! under a sibling temporary name and hard-links it into place so the lock
//! pathname is never visible before its pid content.
//!
//! Two locks per session: `<name>.lock` (creation/metadata) and
//! `<name>.events.lock` (event log). Whenever both are taken the order is
//! event lock first, then creation lock (`with_both_locks`).
//!
//! node: src/sessions.ts:2273-2336, 2374-2386; src/events.ts:224-249
//!
//! # Rust and Node lock contenders
//!
//! Rust publishes a complete 0600 owner inode with one no-replace hard link,
//! so its lock pathname is never visible empty. When stealing a stale lock it
//! takes an advisory lock on the inode it inspected and verifies that the
//! pathname still names that inode before unlinking. A delayed Rust stealer
//! therefore cannot remove a newer owner's lock.
//!
//! Node still creates the canonical file before writing its pid and steals
//! with an unbound read-then-unlink sequence. Rust safely respects a live Node
//! lock once its complete pid is visible, and Rust-only stale recovery is
//! exclusive. If a concurrent stale-recovery path involves Node, however, a
//! delayed Node contender can unlink a newer Rust or Node claim.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::list::pid_alive;
use super::root::{ensure_session_dir, event_lock_path, lock_path};

/// The text Node throws when the event lock is held (`events.ts:229-295`).
pub fn event_busy_message(name: &str) -> String {
    format!("Session id \"{name}\" event log is busy. Retry the operation.")
}

/// The text Node throws when the creation/metadata lock is held.
pub fn metadata_busy_message(name: &str) -> String {
    format!("Session id \"{name}\" metadata is busy. Retry the operation.")
}

/// A held file lock; dropping it unlinks the lock file.
#[derive(Debug)]
pub struct LockGuard {
    path: PathBuf,
    armed: bool,
}

impl LockGuard {
    /// The lock file this guard owns.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Release now (idempotent; dropping does the same).
    pub fn release(mut self) {
        self.armed = false;
        release_file_lock(&self.path);
    }

    /// Keep the lock file on disk when this guard drops (the caller takes
    /// over the release).
    pub fn forget(mut self) {
        self.armed = false;
    }

    /// Hand this lock to another live process. The owner record is replaced
    /// in one rename by a complete record naming `pid`, so the pathname never
    /// names nobody, and this guard stops releasing it. The new owner
    /// releases it with [`release_file_lock_owned_by`]; if it dies first, the
    /// lock is stale and stealable as usual. On failure the guard is dropped,
    /// which releases the lock.
    pub fn hand_over(mut self, pid: u32) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = super::atomic::tmp_path_for(&self.path);
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut file| file.write_all(pid.to_string().as_bytes()))
            .and_then(|()| std::fs::rename(&tmp, &self.path));
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
            return written;
        }
        self.armed = false;
        Ok(())
    }
}

/// Release a lock that was handed to `pid`, only while it still names
/// `pid`. Returns whether it did.
pub fn release_file_lock_owned_by(lock_path: &Path, pid: u32) -> bool {
    let named = std::fs::read_to_string(lock_path)
        .ok()
        .and_then(|content| parse_leading_int(content.trim()));
    if named == Some(pid as i32) {
        release_file_lock(lock_path);
        true
    } else {
        false
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if self.armed {
            release_file_lock(&self.path);
        }
    }
}

/// Publish a complete lock owner record without an empty-file window.
///
/// `open(O_CREAT|O_EXCL)` followed by `write(pid)` made the pathname visible
/// before it named an owner. A racing acquirer read that empty file as stale,
/// unlinked a live holder's lock, and entered the critical section with it.
/// Build the 0600 inode under a unique temporary name and hard-link it into
/// place: link creation is no-replace and the target is complete when it
/// first exists.
fn try_create(lock_path: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::OpenOptionsExt;

    let tmp = super::atomic::tmp_path_for(lock_path);
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(std::process::id().to_string().as_bytes())?;
        drop(file);
        match std::fs::hard_link(&tmp, lock_path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e),
        }
    })();
    let _ = std::fs::remove_file(tmp);
    result
}

fn try_lock_exclusive(file: &std::fs::File) -> std::io::Result<bool> {
    loop {
        // SAFETY: `file` owns a valid descriptor for the duration of the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        match error.kind() {
            std::io::ErrorKind::Interrupted => {}
            std::io::ErrorKind::WouldBlock => return Ok(false),
            _ => return Err(error),
        }
    }
}

fn same_inode(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

/// Pins the name's creation claim through a daemon's final publication.
///
/// A launcher may own the PID claim while its daemon publishes. Pinning its
/// inode prevents stale recovery (or the launcher's release) from admitting a
/// revocation or replacement if that launcher exits during publication.
pub struct PublicationLock {
    // File drops first, releasing flock before an independently acquired claim.
    _inode: std::fs::File,
    _claim: Option<LockGuard>,
}

pub fn pin_publication(name: &str, delegated_owner: Option<i32>) -> std::io::Result<PublicationLock> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = lock_path(name);
    let me = std::process::id() as i32;
    let existing_owner = if is_lock_owned_by_pid(name, me) {
        Some(me)
    } else {
        delegated_owner.filter(|pid| is_lock_owned_by_pid(name, *pid))
    };
    let claim = if existing_owner.is_none() {
        Some(try_acquire_file_lock(&path)?.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::WouldBlock, "publication creation lock is busy")
        })?)
    } else {
        None
    };
    let expected = existing_owner.unwrap_or(me);
    let mut inode = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)?;
    if !try_lock_exclusive(&inode)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock, "publication creation lock is pinned",
        ));
    }
    let opened = inode.metadata()?;
    let current = std::fs::symlink_metadata(&path)?;
    let mut owner = String::new();
    inode.read_to_string(&mut owner)?;
    if !opened.is_file() || !same_inode(&opened, &current)
        || owner.trim().parse::<i32>().ok() != Some(expected)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied, "publication creation ownership changed",
        ));
    }
    Ok(PublicationLock { _inode: inode, _claim: claim })
}

fn holder_is_alive(file: &std::fs::File) -> bool {
    let mut contents = String::new();
    let mut reader = file;
    reader.read_to_string(&mut contents).is_ok()
        && parse_leading_int(contents.trim()).is_some_and(pid_alive)
}

fn steal_opened_lock(lock_path: &Path, stale_inode: std::fs::File) -> std::io::Result<bool> {
    if !try_lock_exclusive(&stale_inode)? {
        return Ok(false);
    }

    let opened_metadata = stale_inode.metadata()?;
    let current_metadata = match std::fs::metadata(lock_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return try_create(lock_path);
        }
        Err(error) => return Err(error),
    };
    if !same_inode(&opened_metadata, &current_metadata) || holder_is_alive(&stale_inode) {
        return Ok(false);
    }

    std::fs::remove_file(lock_path)?;
    try_create(lock_path)
}

/// Acquire an exclusive file lock at `lock_path`. `Some(guard)` when
/// acquired, `None` when another live process holds it. A lock whose holder
/// pid is dead (or unreadable/garbage) is stolen once.
///
/// Stale stealers take an advisory lock on the stale inode and verify that
/// the pathname still names it before unlinking. This prevents a delayed
/// stealer from removing the complete owner record another stealer has
/// already published.
///
/// I/O errors other than `EEXIST` are surfaced as `Err`, as Node rethrows
/// them.
///
/// node: src/sessions.ts:2293-2336
pub fn try_acquire_file_lock(lock_path: &Path) -> std::io::Result<Option<LockGuard>> {
    ensure_session_dir()?;
    let guard = |path: &Path| LockGuard {
        path: path.to_path_buf(),
        armed: true,
    };
    if try_create(lock_path)? {
        return Ok(Some(guard(lock_path)));
    }

    let stale_inode = match std::fs::OpenOptions::new().read(true).open(lock_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(try_create(lock_path)?.then(|| guard(lock_path)));
        }
        Err(error) => return Err(error),
    };
    let acquired = steal_opened_lock(lock_path, stale_inode)?;
    Ok(acquired.then(|| guard(lock_path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delayed_stale_stealer_does_not_remove_a_new_owner() {
        let dir = std::env::temp_dir().join(format!(
            "pty-lock-steal-{}-{}",
            std::process::id(),
            super::super::atomic::random_hex16()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("session.lock");
        std::fs::write(&path, "2147483646").unwrap();
        let delayed_stealer = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();

        std::fs::remove_file(&path).unwrap();
        assert!(try_create(&path).unwrap(), "new owner must publish");
        assert!(
            !steal_opened_lock(&path, delayed_stealer).unwrap(),
            "decision made from the stale inode must not remove its replacement"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            std::process::id().to_string()
        );

        std::fs::remove_dir_all(dir).unwrap();
    }
}

/// [`try_acquire_file_lock`] with I/O errors folded into `None`.
///
/// Use it only where "not taken" is the whole answer. Anything that reports
/// to a caller wants [`lock_or_refusal`] instead: folding an I/O error into
/// `None` turns a read-only registry into "the event log is busy, retry",
/// which is untrue and sends the caller round a loop that cannot end.
pub fn acquire_file_lock(lock_path: &Path) -> Option<LockGuard> {
    try_acquire_file_lock(lock_path).ok().flatten()
}

/// Why a lock was not taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockRefusal {
    /// A live process holds it. Retrying can work.
    Busy,
    /// The lock file could not be created at all: a read-only registry, a
    /// full disk, a directory this process may not write. Retrying cannot
    /// work, so the message must not ask for a retry. Node throws the same
    /// error out of `acquireFileLock` rather than reporting "busy".
    Unavailable(String),
}
/// Take `lock_path` and say why when it refuses.
///
/// node: src/sessions.ts:2293-2336 (`acquireFileLock` returns false only on
/// `EEXIST` and rethrows every other error).
///
pub fn lock_or_refusal(lock_path: &Path) -> Result<LockGuard, LockRefusal> {
    match try_acquire_file_lock(lock_path) {
        Ok(Some(guard)) => Ok(guard),
        Ok(None) => Err(LockRefusal::Busy),
        Err(e) => Err(LockRefusal::Unavailable(format!(
            "{}: {e}",
            lock_path.display()
        ))),
    }
}

/// Take `<name>.events.lock`, with Node's busy text when a live holder has
/// it and the real cause when the file cannot be created.
pub fn take_event_lock(name: &str) -> Result<LockGuard, String> {
    lock_or_refusal(&event_lock_path(name)).map_err(|r| match r {
        LockRefusal::Busy => event_busy_message(name),
        LockRefusal::Unavailable(cause) => cause,
    })
}

/// Take `<name>.lock`, with Node's busy text when a live holder has it and
/// the real cause when the file cannot be created.
pub fn take_metadata_lock(name: &str) -> Result<LockGuard, String> {
    lock_or_refusal(&lock_path(name)).map_err(|r| match r {
        LockRefusal::Busy => metadata_busy_message(name),
        LockRefusal::Unavailable(cause) => cause,
    })
}

/// Release a lock by path (unlink; missing is fine).
///
/// node: src/sessions.ts:2374-2378
pub fn release_file_lock(lock_path: &Path) {
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(inode) = std::fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(lock_path) else {
        return;
    };
    loop {
        // SAFETY: inode is open; a final publisher may still pin this claim.
        if unsafe { libc::flock(inode.as_raw_fd(), libc::LOCK_EX) } == 0 {
            break;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return;
        }
    }
    if let (Ok(opened), Ok(current)) = (inode.metadata(), std::fs::symlink_metadata(lock_path))
        && same_inode(&opened, &current)
    {
        let _ = std::fs::remove_file(lock_path);
    }
}

/// `parseInt(s, 10)`: leading integer, `None` when there is none.
pub(crate) fn parse_leading_int(s: &str) -> Option<i32> {
    let s = s.trim_start();
    let (neg, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let end = digits.bytes().take_while(u8::is_ascii_digit).count();
    if end == 0 {
        return None;
    }
    let value: i64 = digits[..end].parse().ok()?;
    let value = if neg { -value } else { value };
    i32::try_from(value).ok()
}

/// Acquire the creation/metadata lock `<name>.lock`.
pub fn acquire_lock(name: &str) -> Option<LockGuard> {
    acquire_file_lock(&lock_path(name))
}

/// Release `<name>.lock` regardless of holder (Node's `releaseLock`).
pub fn release_lock(name: &str) {
    release_file_lock(&lock_path(name));
}

/// Acquire the event lock `<name>.events.lock` without waiting.
///
/// node: src/events.ts:228-230
pub fn acquire_event_lock(name: &str) -> Option<LockGuard> {
    acquire_file_lock(&event_lock_path(name))
}

/// Release `<name>.events.lock` regardless of holder.
pub fn release_event_lock(name: &str) {
    release_file_lock(&event_lock_path(name));
}

/// How long async writers wait for the event lock (`EVENT_LOCK_WAIT_MS`).
pub const EVENT_LOCK_WAIT: Duration = Duration::from_millis(5_000);

/// Acquire the event lock, polling every 10 ms for up to `wait`. Fails with
/// Node's busy text when the deadline passes.
///
/// node: src/events.ts:237-249
pub fn wait_for_event_lock(name: &str, wait: Duration) -> Result<LockGuard, String> {
    let deadline = Instant::now() + wait;
    let path = event_lock_path(name);
    loop {
        match lock_or_refusal(&path) {
            Ok(guard) => return Ok(guard),
            // The lock file cannot be made at all. Waiting five seconds to
            // say so would be five seconds spent on an answer that will not
            // change.
            Err(LockRefusal::Unavailable(cause)) => return Err(cause),
            Err(LockRefusal::Busy) => {}
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(event_busy_message(name));
        }
        std::thread::sleep(Duration::from_millis(10).min(deadline - now));
    }
}

/// Total budget `metadata patch --id` shares between the event and
/// creation/metadata locks before reporting either one busy.
///
/// An attached child can run while `pty run` still holds `<name>.lock`;
/// waiting here makes that creation window invisible without changing the
/// historical fail-fast behavior of `rename`, `tag`, or other metadata
/// writers. A holder that outlives the budget still fails closed.
///
/// node: src/sessions.ts `METADATA_PATCH_WAIT_MS`
pub const METADATA_PATCH_LOCK_WAIT: Duration = Duration::from_millis(8_000);

/// Acquire the creation/metadata lock `<name>.lock`, polling every 10 ms
/// for up to `wait`.
///
/// Returns `Busy` when a live holder outlives the budget (the caller keeps
/// today's `metadata is busy` text) and `Unavailable` at once when the lock
/// file cannot be created at all, as waiting cannot help — the same
/// fail-closed contract as [`wait_for_event_lock`].
pub fn wait_for_metadata_lock(name: &str, wait: Duration) -> Result<LockGuard, LockRefusal> {
    let deadline = Instant::now() + wait;
    let path = lock_path(name);
    loop {
        match lock_or_refusal(&path) {
            Ok(guard) => return Ok(guard),
            // The lock file cannot be made at all. Waiting out the budget
            // to say so would be time spent on an answer that will not
            // change.
            Err(refusal @ LockRefusal::Unavailable(_)) => return Err(refusal),
            Err(LockRefusal::Busy) => {}
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(LockRefusal::Busy);
        }
        std::thread::sleep(Duration::from_millis(10).min(deadline - now));
    }
}

/// Which lock refused a two-lock operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockBusy {
    /// `<name>.events.lock` is held by a live process.
    Events,
    /// `<name>.lock` is held by a live process.
    Metadata,
    /// Neither lock file could be created. Carries the cause, because
    /// "busy, retry" would be untrue.
    Unavailable(String),
}

impl LockBusy {
    /// Node's error text for the refusing lock.
    pub fn message(&self, name: &str) -> String {
        match self {
            LockBusy::Events => event_busy_message(name),
            LockBusy::Metadata => metadata_busy_message(name),
            LockBusy::Unavailable(cause) => cause.clone(),
        }
    }
}

/// Run `f` holding both locks, taken in Node's order: event lock, then the
/// creation/metadata lock. Neither waits.
///
/// node: src/sessions.ts:2188-2202
pub fn with_both_locks<T>(name: &str, f: impl FnOnce() -> T) -> Result<T, LockBusy> {
    let events = lock_or_refusal(&event_lock_path(name)).map_err(|r| match r {
        LockRefusal::Busy => LockBusy::Events,
        LockRefusal::Unavailable(cause) => LockBusy::Unavailable(cause),
    })?;
    let metadata = lock_or_refusal(&lock_path(name)).map_err(|r| match r {
        LockRefusal::Busy => LockBusy::Metadata,
        LockRefusal::Unavailable(cause) => LockBusy::Unavailable(cause),
    })?;
    let out = f();
    drop(metadata);
    drop(events);
    Ok(out)
}

/// Is `<name>.lock` currently held by a live process? Pure observation:
/// never creates, removes, or steals the lock.
///
/// node: src/sessions.ts `isCreationLockHeld`
pub fn is_creation_lock_held(name: &str) -> bool {
    std::fs::read_to_string(lock_path(name))
        .ok()
        .and_then(|s| parse_leading_int(s.trim()))
        .is_some_and(|pid| pid > 0 && pid_alive(pid))
}

/// Verify an explicitly delegated creation lock (`PTY_CREATION_LOCK_OWNER_PID`)
/// without acquiring it: the file holds `owner_pid` and that process lives.
///
/// node: src/sessions.ts:2273-2281
pub fn is_lock_owned_by_pid(name: &str, owner_pid: i32) -> bool {
    if owner_pid <= 0 {
        return false;
    }
    std::fs::read_to_string(lock_path(name))
        .ok()
        .and_then(|s| parse_leading_int(s.trim()))
        .is_some_and(|pid| pid == owner_pid && pid_alive(owner_pid))
}
