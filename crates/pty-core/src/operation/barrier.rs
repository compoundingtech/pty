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

/// Every barrier name, in the order a successful launch reaches them.
pub const BARRIERS: [&str; 14] = [
    "launcher-before-claim",
    "launcher-before-peek",
    "launcher-before-fork",
    "daemon-gated",
    "before-decision",
    "publish-issued-before-commit",
    "publish-committed-before-effect",
    "published-before-child-fork",
    "child-forked-before-record",
    "child-prepared-before-ack-read",
    "before-ready",
    "transfer-before-commit",
    "transfer-committed-before-release",
    "released-before-ack",
];

/// Pass the barrier `name`, holding here if a test armed it.
pub fn barrier(name: &str) {
    #[cfg(debug_assertions)]
    {
        debug_assert!(BARRIERS.contains(&name), "unknown barrier {name}");
        let Some(dir) = std::env::var_os(BARRIERS_ENV) else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        let _ = std::fs::write(
            dir.join(format!("{name}.reached")),
            std::process::id().to_string(),
        );
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
