//! Node-compatible file-lock contract: no-replace claim, complete holder pid,
//! one stale steal, release by unlink; plus event-lock waiting and the
//! event-then-metadata order of `with_both_locks` / `cleanup_all`.

mod registry_support;

use std::time::Duration;

use pty_core::registry::{self, LockBusy};
use registry_support::{DEAD_PID, root, unique_name};

/// node: tests/security-fixes.test.ts:47-52
#[test]
fn first_caller_wins_second_refused_while_holder_alive() {
    let _ = root();
    let name = unique_name("race");
    let first = registry::acquire_lock(&name).expect("first acquire");
    assert!(
        registry::acquire_lock(&name).is_none(),
        "second acquire must refuse"
    );
    first.release();
    assert!(
        registry::acquire_lock(&name).is_some(),
        "acquire after release"
    );
    registry::release_lock(&name);
}

/// node: tests/security-fixes.test.ts:54-65
#[test]
fn steals_a_stale_lock_whose_holder_is_dead() {
    let root = root();
    let name = unique_name("stale");
    let lock_path = root.join(format!("{name}.lock"));
    std::fs::write(&lock_path, "1").unwrap();
    std::fs::write(&lock_path, DEAD_PID.to_string()).unwrap();
    let guard = registry::acquire_lock(&name).expect("stale lock must be stolen");
    let holder: u32 = std::fs::read_to_string(&lock_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(
        holder,
        std::process::id(),
        "lock file must now hold our pid"
    );
    assert_eq!(guard.path(), lock_path.as_path());
    drop(guard);
    assert!(!lock_path.exists(), "drop releases (unlinks) the lock");
}

/// A stale owner record only needs to be readable; replacement must not
/// require write access to the stale inode.
#[test]
fn steals_a_read_only_stale_lock() {
    use std::os::unix::fs::PermissionsExt;

    let root = root();
    let name = unique_name("readonly-stale");
    let lock_path = root.join(format!("{name}.lock"));
    std::fs::write(&lock_path, DEAD_PID.to_string()).unwrap();
    std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o400)).unwrap();

    let guard = registry::acquire_lock(&name).expect("read-only stale lock must be stolen");
    assert_eq!(
        std::fs::read_to_string(&lock_path).unwrap(),
        std::process::id().to_string()
    );
    drop(guard);
    assert!(!lock_path.exists());
}

/// node: tests/security-fixes.test.ts:67-71
#[test]
fn garbage_lock_content_is_stale() {
    let root = root();
    let name = unique_name("garbage");
    std::fs::write(root.join(format!("{name}.lock")), "not a pid").unwrap();
    assert!(registry::acquire_lock(&name).is_some());
    registry::release_lock(&name);
}

/// node: tests/security-fixes.test.ts:73-75
#[test]
fn release_of_a_missing_lock_is_fine() {
    let _ = root();
    registry::release_lock("never-locked");
    registry::release_event_lock("never-locked");
}

/// node: tests/security-fixes.test.ts:77-87
#[test]
fn only_one_of_two_sequential_steals_wins() {
    let root = root();
    let name = unique_name("race4");
    std::fs::write(root.join(format!("{name}.lock")), DEAD_PID.to_string()).unwrap();
    let a = registry::acquire_lock(&name);
    let b = registry::acquire_lock(&name);
    assert_eq!(a.is_some() as u8 + b.is_some() as u8, 1);
}

/// The atomically published file is `0600` and holds only the decimal pid.
///
/// node: src/sessions.ts:2298-2310
#[test]
fn lock_file_mode_and_content() {
    use std::os::unix::fs::PermissionsExt;
    let root = root();
    let name = unique_name("mode");
    let _guard = registry::acquire_lock(&name).unwrap();
    let path = root.join(format!("{name}.lock"));
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        std::process::id().to_string()
    );
}

/// node: src/sessions.ts:2273-2281; tests/gc-generation-guard.test.ts:181-207
#[test]
fn is_lock_owned_by_pid_checks_holder_and_liveness() {
    let root = root();
    let name = unique_name("owned");
    let me = std::process::id() as i32;
    assert!(!registry::is_lock_owned_by_pid(&name, me), "no lock file");
    let _guard = registry::acquire_lock(&name).unwrap();
    assert!(registry::is_lock_owned_by_pid(&name, me));
    assert!(!registry::is_lock_owned_by_pid(&name, me + 1));
    assert!(!registry::is_lock_owned_by_pid(&name, 0));
    assert!(!registry::is_lock_owned_by_pid(&name, -1));
    std::fs::write(root.join(format!("{name}.lock")), DEAD_PID.to_string()).unwrap();
    assert!(
        !registry::is_lock_owned_by_pid(&name, DEAD_PID),
        "dead holder is not an owner"
    );
}

/// node: tests/atomic-writes.test.ts:209-233
#[test]
fn cleanup_all_cannot_clean_through_a_live_metadata_lock_holder() {
    let root = root();
    let name = unique_name("busy");
    let metadata_path = root.join(format!("{name}.json"));
    let events_path = root.join(format!("{name}.events.jsonl"));
    std::fs::write(&metadata_path, format!("{{\"name\":\"{name}\"}}")).unwrap();
    std::fs::write(&events_path, "{\"type\":\"user.keep\"}\n").unwrap();
    let metadata_before = std::fs::read(&metadata_path).unwrap();
    let events_before = std::fs::read(&events_path).unwrap();

    let held = registry::acquire_lock(&name).expect("acquire");
    let err = registry::cleanup_all(&name).expect_err("must refuse");
    assert_eq!(err, LockBusy::Metadata);
    assert_eq!(
        err.message(&name),
        format!("Session id \"{name}\" metadata is busy. Retry the operation.")
    );
    assert_eq!(std::fs::read(&metadata_path).unwrap(), metadata_before);
    assert_eq!(std::fs::read(&events_path).unwrap(), events_before);
    assert!(registry::acquire_lock(&name).is_none());
    assert!(
        !root.join(format!("{name}.events.lock")).exists(),
        "the refused cleanup released the event lock"
    );
    held.release();

    registry::cleanup_all(&name).expect("cleanup after release");
    assert!(!metadata_path.exists());
    assert!(!events_path.exists());
}

/// node: tests/atomic-writes.test.ts:235-245
#[test]
fn reclaims_a_stale_event_lock_during_full_cleanup() {
    let root = root();
    let name = unique_name("staleev");
    std::fs::write(root.join(format!("{name}.events.jsonl")), "").unwrap();
    std::fs::write(root.join(format!("{name}.events.lock")), "2147483647").unwrap();
    registry::cleanup_all(&name).expect("stale event lock is reclaimed");
    assert!(!root.join(format!("{name}.events.jsonl")).exists());
    assert!(!root.join(format!("{name}.events.lock")).exists());
}

/// A live event-lock holder refuses `cleanup_all` before the metadata lock
/// is even tried.
///
/// node: src/sessions.ts:2188-2202
#[test]
fn cleanup_all_refuses_a_live_event_lock_holder_first() {
    let root = root();
    let name = unique_name("evbusy");
    let _held = registry::acquire_event_lock(&name).unwrap();
    let err = registry::cleanup_all(&name).expect_err("must refuse");
    assert_eq!(err, LockBusy::Events);
    assert_eq!(
        err.message(&name),
        format!("Session id \"{name}\" event log is busy. Retry the operation.")
    );
    assert!(
        !root.join(format!("{name}.lock")).exists(),
        "the metadata lock was never taken"
    );
}

/// node: tests/atomic-writes.test.ts:268-281
#[test]
fn wait_for_event_lock_with_zero_budget_reports_busy() {
    let _ = root();
    let name = unique_name("waitev");
    let _held = registry::acquire_event_lock(&name).unwrap();
    let err = registry::wait_for_event_lock(&name, Duration::ZERO).expect_err("busy");
    assert_eq!(
        err,
        format!("Session id \"{name}\" event log is busy. Retry the operation.")
    );
}

/// node: src/events.ts:237-249
#[test]
fn wait_for_event_lock_acquires_once_the_holder_releases() {
    let _ = root();
    let name = unique_name("waitok");
    let held = registry::acquire_event_lock(&name).unwrap();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(120));
        held.release();
    });
    let start = std::time::Instant::now();
    let guard = registry::wait_for_event_lock(&name, Duration::from_secs(5))
        .expect("acquired after release");
    assert!(
        start.elapsed() >= Duration::from_millis(100),
        "must have waited for the holder"
    );
    drop(guard);
    releaser.join().unwrap();
}

/// `with_both_locks` takes the event lock, then the creation lock, and
/// releases both.
#[test]
fn with_both_locks_takes_events_then_metadata_and_releases() {
    let root = root();
    let name = unique_name("both");
    let event_lock = root.join(format!("{name}.events.lock"));
    let meta_lock = root.join(format!("{name}.lock"));
    let out = registry::with_both_locks(&name, || {
        assert!(event_lock.exists());
        assert!(meta_lock.exists());
        42
    })
    .unwrap();
    assert_eq!(out, 42);
    assert!(!event_lock.exists());
    assert!(!meta_lock.exists());
}

#[test]
fn publication_pin_releases_its_own_acquired_claim() {
    let root = root();
    let name = unique_name("pub-own");
    let path = root.join(format!("{name}.lock"));
    let pin = registry::lock::pin_publication(&name, None).expect("pin acquires claim");
    assert!(path.exists());
    assert!(registry::acquire_lock(&name).is_none());
    drop(pin);
    assert!(!path.exists(), "dropping the pin releases its acquired claim");
    let _next = registry::acquire_lock(&name).expect("next contender enters");
}

#[test]
fn publication_pin_preserves_an_existing_owned_claim() {
    let _ = root();
    let name = unique_name("pub-held");
    let claim = registry::acquire_lock(&name).expect("launcher claim");
    let pin = registry::lock::pin_publication(&name, None).expect("pin existing claim");
    drop(pin);
    assert!(claim.path().exists(), "the caller still owns its claim");
    assert!(registry::acquire_lock(&name).is_none());
    claim.release();
    let _next = registry::acquire_lock(&name).expect("claim released");
}

#[test]
fn revocation_and_publication_use_the_same_name_serialization_in_both_orders() {
    use pty_core::registry::revocation::{self, PublicationBinding};

    let _ = root();
    let root_id = revocation::root_id().expect("enroll isolated root");
    let binding = PublicationBinding {
        operation: "publication-lock-order".to_string(),
        digest: "a".repeat(64),
    };
    let intent = "b".repeat(64);
    let name = unique_name("pub-first");
    let pin = registry::lock::pin_publication(&name, None).expect("publisher wins");
    revocation::check(&name, Some(&binding), Some(&binding.operation)).expect("not revoked");
    assert_eq!(
        revocation::install(&name, &binding, &intent, &root_id).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "a revoker cannot enter the publisher's critical section"
    );
    assert!(revocation::read(&name, &binding.operation).unwrap().is_none());
    drop(pin);
    revocation::install(&name, &binding, &intent, &root_id).expect("revoker enters after publication");

    let name = unique_name("revoke-first");
    let receipt = revocation::install(&name, &binding, &intent, &root_id).expect("revoker wins");
    let pin = registry::lock::pin_publication(&name, None).expect("late publisher pins");
    let rejection = revocation::check(&name, Some(&binding), Some(&binding.operation)).unwrap_err();
    assert_eq!(rejection.to_string(), "operation-revoked");
    assert_eq!(
        rejection.kind(),
        std::io::ErrorKind::PermissionDenied,
        "the final check rejects the late publisher while it holds serialization"
    );
    assert_eq!(revocation::read(&name, &binding.operation).unwrap(), Some(receipt.record));
    drop(pin);
}

// The owner runs in a separate process so killing and reaping it really makes
// the PID record stale, unlike a synthetic dead-PID rewrite of a live claim.
struct PublicationClaimOwner {
    child: std::process::Child,
    control: Option<std::os::unix::net::UnixStream>,
    socket: std::path::PathBuf,
}

impl PublicationClaimOwner {
    fn spawn(name: &str) -> Self {
        use std::io::Read;
        use std::os::unix::net::UnixListener;

        let socket = root().join(unique_name("pubctl"));
        let listener = UnixListener::bind(&socket).expect("bind owner control");
        listener.set_nonblocking(true).unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "publication_claim_owner_subprocess", "--nocapture"])
            .env("PTY_PUBLICATION_TEST_NAME", name)
            .env("PTY_PUBLICATION_TEST_CONTROL", &socket)
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn claim owner");
        // Construct the cleanup guard before any bounded handshake can fail.
        let mut owner = Self { child, control: None, socket };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let control = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(owner.child.try_wait().unwrap().is_none(), "owner exited before connecting");
                    assert!(std::time::Instant::now() < deadline, "owner connection timed out");
                    // Polling only; readiness is established by the control message.
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept owner control: {error}"),
            }
        };
        control.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        control.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
        owner.control = Some(control);
        let mut ready = [0];
        owner.control().read_exact(&mut ready).expect("owner acquired claim");
        assert_eq!(ready, [b'L']);
        owner
    }

    fn control(&mut self) -> &mut std::os::unix::net::UnixStream {
        self.control.as_mut().expect("connected owner control")
    }
}

impl Drop for PublicationClaimOwner {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

#[test]
#[ignore = "subprocess helper, invoked explicitly by publication lock tests"]
fn publication_claim_owner_subprocess() {
    use std::io::{Read, Write};

    // Deliberately do not call root(): this subprocess uses the isolated root
    // inherited from its test parent rather than creating another registry.
    let name = std::env::var("PTY_PUBLICATION_TEST_NAME").expect("owner name");
    let mut control = std::os::unix::net::UnixStream::connect(
        std::env::var_os("PTY_PUBLICATION_TEST_CONTROL").expect("owner control"),
    ).unwrap();
    control.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    control.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    let claim = registry::acquire_lock(&name).expect("subprocess acquires claim");
    control.write_all(b"L").unwrap();
    let mut command = [0];
    control.read_exact(&mut command).expect("release command");
    assert_eq!(command, [b'R']);
    control.write_all(b"A").unwrap();
    claim.release();
    control.write_all(b"D").unwrap();
}

#[test]
fn delegated_publication_pin_prevents_stale_steal_after_owner_exit() {
    use std::os::unix::fs::MetadataExt;
    use pty_core::registry::revocation::{self, PublicationBinding};

    let root = root();
    let root_id = revocation::root_id().expect("enroll isolated root");
    let name = unique_name("pub-death");
    let path = root.join(format!("{name}.lock"));
    let mut owner = PublicationClaimOwner::spawn(&name);
    let pin = registry::lock::pin_publication(&name, Some(owner.child.id() as i32))
        .expect("pin live delegated owner's inode");
    let before = std::fs::metadata(&path).unwrap();
    owner.child.kill().unwrap();
    owner.child.wait().expect("reap owner before stale recovery");
    assert!(registry::acquire_lock(&name).is_none(), "stale stealing must respect the pin");
    let binding = PublicationBinding {
        operation: "owner-exit-publication".to_string(),
        digest: "c".repeat(64),
    };
    assert_eq!(
        revocation::install(&name, &binding, &"d".repeat(64), &root_id).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "revocation cannot enter after owner death while publication is pinned"
    );
    let after = std::fs::metadata(&path).unwrap();
    assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), owner.child.id().to_string());
    drop(pin);
    let next = registry::acquire_lock(&name).expect("stale claim is stealable after publication");
    next.release();
    revocation::install(&name, &binding, &"d".repeat(64), &root_id).expect("revocation enters after publication");
}

// Linux exposes blocked flock requests, so this proves that release reached
// the pinned inode rather than inferring blocking from an arbitrary sleep.
#[cfg(target_os = "linux")]
#[test]
fn delegated_owner_release_waits_for_publication_pin() {
    use std::io::{Read, Write};
    use std::os::unix::fs::MetadataExt;

    let root = root();
    let name = unique_name("pub-release");
    let path = root.join(format!("{name}.lock"));
    let mut owner = PublicationClaimOwner::spawn(&name);
    let pin = registry::lock::pin_publication(&name, Some(owner.child.id() as i32))
        .expect("pin delegated claim");
    let inode = std::fs::metadata(&path).unwrap().ino().to_string();
    owner.control().write_all(b"R").unwrap();
    let mut message = [0];
    owner.control().read_exact(&mut message).expect("owner starts release");
    assert_eq!(message, [b'A']);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let owner_pid = owner.child.id().to_string();
    loop {
        let locks = std::fs::read_to_string("/proc/locks").expect("read kernel lock waiters");
        let waiting = locks.lines().any(|line| {
            let mut fields = line.split_whitespace();
            fields.nth(1) == Some("->")
                && fields.next() == Some("FLOCK")
                && fields.nth(2) == Some(owner_pid.as_str())
                && fields.next().and_then(|field| field.rsplit(':').next()) == Some(inode.as_str())
        });
        if waiting {
            break;
        }
        assert!(owner.child.try_wait().unwrap().is_none(), "release returned before publication completed");
        assert!(std::time::Instant::now() < deadline, "release never blocked on publication inode");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(path.exists(), "blocked release has not unlinked the pinned claim");
    assert!(registry::acquire_lock(&name).is_none());
    drop(pin);
    owner.control().read_exact(&mut message).expect("release completes after publication");
    assert_eq!(message, [b'D']);
    assert!(!path.exists());
    let _next = registry::acquire_lock(&name).expect("next owner enters after release");
}
