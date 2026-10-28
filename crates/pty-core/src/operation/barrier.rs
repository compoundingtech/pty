//! Acceptance barriers: named points where a test can hold the launcher or
//! the daemon still, deliver input or kill a participant, and then let it go.
//!
//! **Debug builds only.** A release build compiles every barrier to nothing
//! and never reads the variable.
//!
//! With `PTY_TEST_OPERATION_BARRIERS=<dir>` set, reaching a barrier creates
//! `<dir>/<name>.reached`. If the test armed it by creating
//! `<dir>/<name>.hold`, the holder then blocks until `<dir>/<name>.release`
//! appears. An unarmed barrier only records that it was reached.

/// The variable that names the barrier directory.
pub const BARRIERS_ENV: &str = "PTY_TEST_OPERATION_BARRIERS";

/// Named launch, revocation installation and cleanup acceptance points.
pub const BARRIERS: [&str; 22] = [
    "launcher-before-claim",
    "launcher-before-peek",
    "launcher-before-fork",
    "daemon-gated",
    "before-decision",
    "publish-issued-before-commit",
    "publish-committed-before-effect",
    "publication-before-lock",
    "publication-before-effects",
    "published-before-child-fork",
    "before-claim-hand-back",
    "metadata-lock-busy",
    "event-lock-busy",
    "child-forked-before-record",
    "child-prepared-before-ack-read",
    "before-ready",
    "transfer-before-commit",
    "transfer-committed-before-release",
    "released-before-ack",
    "revocation-install-pinned",
    "gc-cleanup-before-effects",
    "settle-writer-lock-refused",
];

/// Record that `name` was reached, and say whether a test armed it. A
/// caller that must keep working while held (the daemon's actor) holds
/// without blocking: it parks the step and waits for [`wait_released`] on
/// another thread.
pub fn armed(name: &str) -> bool {
    #[cfg(debug_assertions)]
    {
        debug_assert!(BARRIERS.contains(&name), "unknown barrier {name}");
        let Some(dir) = std::env::var_os(BARRIERS_ENV) else {
            return false;
        };
        let dir = std::path::PathBuf::from(dir);
        record_reached(&dir, name);
        dir.join(format!("{name}.hold")).exists()
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = name;
        false
    }
}

/// Publish `<dir>/<name>.reached` holding this process's id. It is written
/// aside and renamed into place, because tests read the pid as soon as the
/// file exists, and `fs::write` shows an empty file between creating it and
/// writing to it.
#[cfg(debug_assertions)]
fn record_reached(dir: &std::path::Path, name: &str) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static STAGED: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let staged = dir.join(format!(
        "{name}.reached.{pid}.{}",
        STAGED.fetch_add(1, Ordering::Relaxed)
    ));
    if std::fs::write(&staged, pid.to_string()).is_ok() {
        let _ = std::fs::rename(&staged, dir.join(format!("{name}.reached")));
    }
}

/// Block until the test releases `name`.
pub fn wait_released(name: &str) {
    #[cfg(debug_assertions)]
    {
        let Some(dir) = std::env::var_os(BARRIERS_ENV) else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        while !dir.join(format!("{name}.release")).exists() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    #[cfg(not(debug_assertions))]
    let _ = name;
}

/// Pass the barrier `name`, holding here if a test armed it.
pub fn barrier(name: &str) {
    #[cfg(debug_assertions)]
    {
        debug_assert!(BARRIERS.contains(&name), "unknown barrier {name}");
        let Some(dir) = std::env::var_os(BARRIERS_ENV) else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        record_reached(&dir, name);
        if !dir.join(format!("{name}.hold")).exists() {
            return;
        }
        while !dir.join(format!("{name}.release")).exists() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = name;
    }
}
