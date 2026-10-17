//! Node-compatible file locks: a no-replace claim, decimal holder PID prefix,
//! one stale steal, and release by unlink. Rust adds a random nonce after the
//! PID for explicit per-claim delegation. The complete 0600 owner inode is
//! hard-linked into place, never exposing an empty owner record.
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

/// A held PID claim; dropping it unlinks only while its owner still matches.
#[derive(Debug)]
pub struct LockGuard {
    path: PathBuf,
    owner_pid: u32,
    nonce: String,
    delegated: std::sync::atomic::AtomicBool,
    armed: bool,
}

impl LockGuard {
    /// The lock file this guard owns.
    pub fn path(&self) -> &Path {
        &self.path
    }


    /// Explicit authority for this individual claim, never inferred from a PID.
    /// Lazily publishes the delegation token; ordinary event appends do not
    /// create tokens. Token I/O failure must be reported before launch.
    pub fn delegate(&self) -> std::io::Result<CreationClaimDelegate> {
        if !self.delegated.load(std::sync::atomic::Ordering::Relaxed) {
            activate_claim(&self.path, &self.nonce)?;
            self.delegated.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(CreationClaimDelegate { owner_pid: self.owner_pid as i32, nonce: self.nonce.clone() })
    }

    /// Nonblocking inode pin while this guard remains held. Drop the pin first.
    pub fn pin(&self) -> std::io::Result<PublicationLock> {
        pin_claim(&self.path, &self.delegate()?, None, false)
    }
    /// Release now (idempotent; dropping does the same).
    pub fn release(mut self) {
        self.armed = false;
        self.withdraw_delegation();
        release_file_lock_checked_nonce(&self.path, Some(self.owner_pid), Some(&self.nonce));
    }

    /// Keep the lock file on disk when this guard drops (the caller takes
    /// over the release).
    pub fn forget(mut self) {
        self.armed = false;
    }

    fn withdraw_delegation(&self) {
        // The external-PID bridge only addresses metadata claims, and can
        // activate their token in another process. Ordinary event appends
        // incur no token I/O unless their guard is explicitly delegated.
        if self.delegated.load(std::sync::atomic::Ordering::Relaxed)
            || !self.path.file_name().is_some_and(|name| name.as_encoded_bytes().ends_with(b".events.lock"))
        {
            withdraw_claim(&self.path, &self.nonce);
        }
    }

    /// Hand this lock to another live process. The owner record is replaced
    /// in one rename by a complete record naming `pid` with the same nonce,
    /// and this guard stops releasing it. The new owner receives that nonce
    /// explicitly before handover and releases with [`release_file_lock_owned_by`].
    /// If it dies first, the lock is stale and stealable as usual. On failure
    /// the guard is dropped, which releases the lock.
    pub fn hand_over(mut self, pid: u32) -> std::io::Result<()> {
        let pin = self.pin()?;
        replace_owner_record(&self.path, &format!("{pid} {}", self.nonce))?;
        drop(pin);
        self.armed = false;
        Ok(())
    }

    /// Return only while the original guard still authorizes this claim.
    pub fn hand_back(mut self, delegate: &CreationClaimDelegate) -> std::io::Result<()> {
        if !claim_active(&self.path, &delegate.nonce) {
            let token = claim_token(&self.path, &delegate.nonce);
            self.release();
            let _ = std::fs::remove_file(token);
            return Ok(());
        }
        // Serialize replacement with every validator/cleanup holder of the
        // daemon's current inode. A busy pin is a nonfatal hand-back failure.
        let pin = self.pin()?;
        let record = if delegate.nonce.starts_with("legacy:") {
            delegate.owner_pid.to_string()
        } else {
            format!("{} {}", delegate.owner_pid, delegate.nonce)
        };
        replace_owner_record(&self.path, &record)?;
        drop(pin);
        let _ = std::fs::remove_file(claim_token(&self.path, &self.nonce));
        self.armed = false;
        // Release can withdraw while rename is in flight. Never leave the
        // restored live-PID record behind after that withdrawal.
        if !claim_active(&self.path, &delegate.nonce) {
            let expected_nonce = (!delegate.nonce.starts_with("legacy:")).then_some(delegate.nonce.as_str());
            if release_file_lock_checked_nonce(&self.path, Some(delegate.owner_pid as u32), expected_nonce) {
                let _ = std::fs::remove_file(claim_token(&self.path, &delegate.nonce));
            }
        } else if delegate.nonce.starts_with("legacy:") {
            // The restored PID-only record cannot carry this nonce again.
            // Legacy coordinators release by pathname, not by token.
            let _ = std::fs::remove_file(claim_token(&self.path, &delegate.nonce));
        }
        Ok(())
    }
}

/// Release a lock that was handed to `pid`, only while it still names
/// `pid`. Returns whether it did.
pub fn release_file_lock_owned_by(lock_path: &Path, pid: u32) -> bool {
    release_file_lock_checked(lock_path, Some(pid))
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if self.armed {
            self.withdraw_delegation();
            release_file_lock_checked_nonce(&self.path, Some(self.owner_pid), Some(&self.nonce));
        }
    }
}

/// Replace a complete owner record without exposing an empty PID claim.

fn replace_owner_record(path: &Path, record: &str) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = super::atomic::tmp_path_for(path);
    let result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut file| file.write_all(record.as_bytes()))
        .and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

// Token state only changes from active to withdrawn. Keeping the withdrawn
// inode until canonical cleanup prevents an external bridge reactivating it.
fn claim_token(path: &Path, nonce: &str) -> PathBuf {
    use std::fmt::Write as _;
    let mut suffix = String::from("claim-");
    for byte in nonce.bytes() {
        let _ = write!(suffix, "{byte:02x}");
    }
    path.with_extension(suffix)
}

fn publish_claim_token(token: &Path, value: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = super::atomic::tmp_path_for(token);
    let result = (|| {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true)
            .mode(0o600).open(&tmp)?;
        file.write_all(value)?;
        match std::fs::hard_link(&tmp, token) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error),
        }
    })();
    let _ = std::fs::remove_file(tmp);
    result
}

fn activate_claim(path: &Path, nonce: &str) -> std::io::Result<()> {
    publish_claim_token(&claim_token(path, nonce), b"1")?;
    if claim_active(path, nonce) {
        Ok(())
    } else {
        Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "creation claim was released"))
    }
}

fn claim_active(path: &Path, nonce: &str) -> bool {
    let mut value = [0];
    std::fs::File::open(claim_token(path, nonce))
        .and_then(|mut file| file.read_exact(&mut value)).is_ok() && value == *b"1"
}

fn withdraw_claim(path: &Path, nonce: &str) {
    let token = claim_token(path, nonce);
    let mut file = match std::fs::OpenOptions::new().write(true).open(&token) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if publish_claim_token(&token, b"0").is_err() {
                return;
            }
            let Ok(file) = std::fs::OpenOptions::new().write(true).open(&token) else {
                return;
            };
            file
        }
        Err(_) => return,
    };
    let _ = file.write_all(b"0");
}

// Every advisory-lock holder, including rejected validation attempts, must
// finish cancellation before unlocking. Otherwise a transient rejected pin
// can make a nonblocking release miss the final cleanup opportunity.
struct WithdrawnClaimCleanup<'a> {
    inode: &'a std::fs::File,
    path: &'a Path,
    legacy_nonce: Option<(i32, &'a str)>,
    armed: bool,
}

impl Drop for WithdrawnClaimCleanup<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let (Ok(opened), Ok(current)) =
            (self.inode.metadata(), std::fs::symlink_metadata(self.path)) else {
                return;
            };
        if !same_inode(&opened, &current) {
            return;
        }
        use std::io::{Seek, SeekFrom};
        let mut reader = self.inode;
        let mut record = String::new();
        if reader.seek(SeekFrom::Start(0)).is_err()
            || reader.read_to_string(&mut record).is_err()
        {
            return;
        }
        let Some(nonce) = record_nonce(&record).or_else(|| {
            self.legacy_nonce.filter(|(pid, _)| parse_leading_int(&record) == Some(*pid))
                .map(|(_, nonce)| nonce)
        }) else { return };
        let token = claim_token(self.path, nonce);
        let mut value = [1];
        if std::fs::File::open(&token).and_then(|mut file| file.read_exact(&mut value)).is_ok()
            && value == *b"0"
            && std::fs::remove_file(self.path).is_ok()
        {
            let _ = std::fs::remove_file(token);
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
    let nonce = super::atomic::random_hex16();
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(format!("{} {nonce}", std::process::id()).as_bytes())?;
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

/// A per-claim capability carried only across an authorized launch boundary.
#[derive(Debug, Clone)]
pub struct CreationClaimDelegate {
    pub owner_pid: i32,
    pub nonce: String,
}

fn record_nonce(record: &str) -> Option<&str> {
    record.split_whitespace().nth(1)
}

/// Upgrade the legacy external-PID CLI contract to an explicit claim nonce.
/// Never admits this process's ambient install/gc claim. The inode is pinned
/// during validation and upgrade, and its PID prefix remains Node-compatible.
pub fn delegate_external_claim(name: &str, owner_pid: i32) -> std::io::Result<CreationClaimDelegate> {
    use std::os::unix::fs::OpenOptionsExt;
    if owner_pid <= 0 || owner_pid == std::process::id() as i32 || !pid_alive(owner_pid) {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "not an external creation owner"));
    }
    let path = lock_path(name);
    let inode = std::fs::OpenOptions::new().read(true).write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&path)?;
    let mut record = String::new();
    (&inode).read_to_string(&mut record)?;
    if parse_leading_int(&record) != Some(owner_pid) {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "creation claim changed"));
    }
    let nonce = record_nonce(&record).map(str::to_owned)
        .unwrap_or_else(|| format!("legacy:{}", super::atomic::random_hex16()));
    // Activate before taking flock: a concurrent Rust guard release must
    // either withdraw this token or unlink the still-unpinned original inode.
    activate_claim(&path, &nonce)?;
    if !try_lock_exclusive(&inode)? {
        return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "creation claim is pinned"));
    }
    let cleanup = WithdrawnClaimCleanup {
        inode: &inode, path: &path, legacy_nonce: Some((owner_pid, &nonce)), armed: true,
    };
    if !same_inode(&inode.metadata()?, &std::fs::symlink_metadata(&path)?)
        || !claim_active(&path, &nonce)
    {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "creation claim changed"));
    }
    use std::io::{Seek, SeekFrom};
    (&inode).seek(SeekFrom::Start(0))?;
    let mut pinned_record = String::new();
    (&inode).read_to_string(&mut pinned_record)?;
    if pinned_record != record {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "creation claim changed"));
    }
    if record_nonce(&record).is_none() {
        // Preserve the PID prefix and restore its original encoding on return.
        write!(&inode, " {nonce}")?;
    }
    drop(cleanup);
    Ok(CreationClaimDelegate { owner_pid, nonce })
}
/// Pins the name's creation claim through a daemon's final publication.
///
/// A launcher may own the PID claim while its daemon publishes. Pinning its
/// inode prevents stale recovery (or the launcher's release) from admitting a
/// revocation or replacement if that launcher exits during publication.
pub struct PublicationLock {
    // The pin is closed before the independently acquired guard drops.
    _inode: std::fs::File,
    _claim: Option<LockGuard>,
    path: PathBuf,
    delegate: Option<CreationClaimDelegate>,
    owner: CreationClaimDelegate,
}

impl PublicationLock {
    /// The explicit coordinator claim to restore after the child has spawned.
    pub fn delegate(&self) -> Option<CreationClaimDelegate> {
        self.delegate.clone()
    }

    /// Transfer the pinned claim to this daemon before it forks. The old
    /// launcher's expected-owner release cannot remove the new PID claim.
    /// No open lock descriptor survives into the ordinary child's fork.
    pub fn into_daemon_claim(mut self) -> std::io::Result<LockGuard> {
        let opened = self._inode.metadata()?;
        let current = std::fs::symlink_metadata(&self.path)?;
        if !same_inode(&opened, &current) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "publication claim changed before daemon handover",
            ));
        }
        let owner_pid = std::process::id();
        let nonce = super::atomic::random_hex16();
        replace_owner_record(&self.path, &format!("{owner_pid} {nonce}"))?;
        if let Some(claim) = self._claim.take() {
            let _ = std::fs::remove_file(claim_token(&self.path, &claim.nonce));
            claim.forget();
        }
        let claim = LockGuard {
            path: std::mem::take(&mut self.path),
            owner_pid,
            nonce,
            delegated: std::sync::atomic::AtomicBool::new(false),
            armed: true,
        };
        drop(self);
        Ok(claim)
    }
}

impl Drop for PublicationLock {
    fn drop(&mut self) {
        // Release may have returned without waiting for this pin. While still
        // holding it, finish cleanup only if this is the withdrawn original.
        if !claim_active(&self.path, &self.owner.nonce)
            && let (Ok(opened), Ok(current)) =
                (self._inode.metadata(), std::fs::symlink_metadata(&self.path))
            && same_inode(&opened, &current)
        {
            if std::fs::remove_file(&self.path).is_ok() {
                let _ = std::fs::remove_file(claim_token(&self.path, &self.owner.nonce));
            }
        }
    }
}

pub fn pin_publication(name: &str, delegate: Option<&CreationClaimDelegate>) -> std::io::Result<PublicationLock> {
    let path = lock_path(name);
    match delegate {
        Some(delegate) => pin_claim(&path, delegate, None, true),
        None => {
            let claim = try_acquire_file_lock(&path)?.ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::WouldBlock, "publication creation lock is busy")
            })?;
            let delegate = claim.delegate()?;
            pin_claim(&path, &delegate, Some(claim), false)
        }
    }
}

fn pin_claim(
    path: &Path,
    delegate: &CreationClaimDelegate,
    claim: Option<LockGuard>,
    delegated: bool,
) -> std::io::Result<PublicationLock> {
    use std::os::unix::fs::OpenOptionsExt;
    let inode = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)?;
    if !try_lock_exclusive(&inode)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock, "publication creation lock is pinned",
        ));
    }
    let mut cleanup = WithdrawnClaimCleanup {
        inode: &inode, path, legacy_nonce: Some((delegate.owner_pid, &delegate.nonce)), armed: true,
    };
    let opened = inode.metadata()?;
    let current = std::fs::symlink_metadata(&path)?;
    let mut owner = String::new();
    (&inode).read_to_string(&mut owner)?;
    if !opened.is_file() || !same_inode(&opened, &current)
        || parse_leading_int(&owner) != Some(delegate.owner_pid)
        || record_nonce(&owner) != Some(delegate.nonce.as_str())
        || !pid_alive(delegate.owner_pid)
        || !claim_active(path, &delegate.nonce)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied, "publication creation ownership changed",
        ));
    }
    cleanup.armed = false;
    drop(cleanup);
    Ok(PublicationLock {
        _inode: inode,
        _claim: claim,
        path: path.to_path_buf(),
        delegate: delegated.then(|| delegate.clone()),
        owner: delegate.clone(),
    })
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
    let _cleanup = WithdrawnClaimCleanup {
        inode: &stale_inode, path: lock_path, legacy_nonce: None, armed: true,
    };

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

    if let Ok(record) = std::fs::read_to_string(lock_path)
        && let Some(nonce) = record_nonce(&record)
    {
        let _ = std::fs::remove_file(claim_token(lock_path, nonce));
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
    let guard = |path: &Path| -> std::io::Result<LockGuard> {
        let record = std::fs::read_to_string(path)?;
        let nonce = record_nonce(&record).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "new creation claim has no nonce")
        })?.to_owned();
        Ok(LockGuard {
            path: path.to_path_buf(),
            owner_pid: std::process::id(),
            nonce,
            delegated: std::sync::atomic::AtomicBool::new(false),
            armed: true,
        })
    };
    if try_create(lock_path)? {
        return guard(lock_path).map(Some);
    }

    let stale_inode = match std::fs::OpenOptions::new().read(true).open(lock_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return try_create(lock_path)?.then(|| guard(lock_path)).transpose();
        }
        Err(error) => return Err(error),
    };
    let acquired = steal_opened_lock(lock_path, stale_inode)?;
    acquired.then(|| guard(lock_path)).transpose()
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
            std::fs::read_to_string(&path).unwrap().split_whitespace().next().unwrap(),
            std::process::id().to_string()
        );

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn successful_legacy_hand_back_removes_its_unreferenced_token() {
        let dir = std::env::temp_dir().join(format!(
            "pty-legacy-return-{}-{}", std::process::id(),
            super::super::atomic::random_hex16()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("legacy.lock");
        let delegate = CreationClaimDelegate {
            owner_pid: std::process::id() as i32,
            nonce: format!("legacy:{}", super::super::atomic::random_hex16()),
        };
        std::fs::write(&path, format!("{} {}", delegate.owner_pid, delegate.nonce)).unwrap();
        activate_claim(&path, &delegate.nonce).unwrap();
        let token = claim_token(&path, &delegate.nonce);
        let publication = pin_claim(&path, &delegate, None, true).unwrap();
        publication.into_daemon_claim().unwrap().hand_back(&delegate).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), delegate.owner_pid.to_string());
        assert!(!token.exists(), "PID-only return must not strand its old capability");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn returned_claim_rejected_contenders_finish_nonblocking_release() {
        let dir = std::env::temp_dir().join(format!(
            "pty-return-contender-{}-{}", std::process::id(),
            super::super::atomic::random_hex16()
        ));
        std::fs::create_dir(&dir).unwrap();
        for stale_checker in [false, true] {
            let path = dir.join(if stale_checker { "stale.lock" } else { "pin.lock" });
            assert!(try_create(&path).unwrap());
            let record = std::fs::read_to_string(&path).unwrap();
            let coordinator = LockGuard {
                path: path.clone(), owner_pid: std::process::id(),
                nonce: record_nonce(&record).unwrap().to_owned(),
                delegated: std::sync::atomic::AtomicBool::new(false), armed: true,
            };
            let delegate = coordinator.delegate().unwrap();
            let publisher = pin_claim(&path, &delegate, None, true).unwrap();
            publisher.into_daemon_claim().unwrap().hand_back(&delegate).unwrap();

            // A contender pins the restored inode just before cancellation.
            // Release must complete while that contender still has flock.
            let contender = std::fs::File::open(&path).unwrap();
            assert!(try_lock_exclusive(&contender).unwrap());
            coordinator.release();
            assert!(path.exists(), "nonblocking release defers to the pin holder");
            if stale_checker {
                assert!(!steal_opened_lock(&path, contender).unwrap());
            } else {
                drop(contender);
                // Validation rejects the withdrawn token. Its error path,
                // not a successful PublicationLock, must finish cleanup.
                assert_eq!(pin_claim(&path, &delegate, None, true).err().unwrap().kind(),
                    std::io::ErrorKind::PermissionDenied);
            }
            assert!(!path.exists(), "rejected contender must not strand the returned live-PID claim");
        }
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
    let _ = release_file_lock_checked(lock_path, None);
}

fn release_file_lock_checked(lock_path: &Path, expected_pid: Option<u32>) -> bool {
    release_file_lock_checked_nonce(lock_path, expected_pid, None)
}

fn release_file_lock_checked_nonce(lock_path: &Path, expected_pid: Option<u32>, expected_nonce: Option<&str>) -> bool {
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(mut inode) = std::fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(lock_path) else {
        return false;
    };
    let mut owner = String::new();
    if inode.read_to_string(&mut owner).is_err()
        || expected_pid.is_some_and(|pid| parse_leading_int(owner.trim()) != Some(pid as i32))
        || expected_nonce.is_some_and(|nonce| record_nonce(&owner) != Some(nonce))
    {
        return false;
    }
    if expected_nonce.is_none()
        && let Some(nonce) = record_nonce(&owner)
    {
        withdraw_claim(lock_path, nonce);
    }
    if !matches!(try_lock_exclusive(&inode), Ok(true)) {
        return false;
    }
    let _cleanup = WithdrawnClaimCleanup {
        inode: &inode, path: lock_path,
        legacy_nonce: expected_pid.zip(expected_nonce).map(|(pid, nonce)| (pid as i32, nonce)), armed: true,
    };
    use std::io::{Seek, SeekFrom};
    if (&inode).seek(SeekFrom::Start(0)).is_err() {
        return false;
    }
    owner.clear();
    if (&inode).read_to_string(&mut owner).is_err()
        || expected_pid.is_some_and(|pid| parse_leading_int(owner.trim()) != Some(pid as i32))
        || expected_nonce.is_some_and(|nonce| record_nonce(&owner) != Some(nonce))
    {
        return false;
    }
    if expected_nonce.is_none()
        && let Some(nonce) = record_nonce(&owner)
    {
        withdraw_claim(lock_path, nonce);
    }
    if let (Ok(opened), Ok(current)) = (inode.metadata(), std::fs::symlink_metadata(lock_path))
        && same_inode(&opened, &current)
    {
        if std::fs::remove_file(lock_path).is_ok() {
            if !lock_path.file_name().is_some_and(|name| name.as_encoded_bytes().ends_with(b".events.lock"))
                && let Some(nonce) = record_nonce(&owner)
            {
                let _ = std::fs::remove_file(claim_token(lock_path, nonce));
            }
            return true;
        }
    }
    false
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
            Err(LockRefusal::Busy) => crate::operation::barrier::barrier("event-lock-busy"),
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
/// Operation-mode child-fork admission shares this bounded budget so brief
/// concurrent metadata mutations do not retire a healthy launch.
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
            Err(LockRefusal::Busy) => crate::operation::barrier::barrier("metadata-lock-busy"),
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
