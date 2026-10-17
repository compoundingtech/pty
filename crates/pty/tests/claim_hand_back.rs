//! A failed coordinator return must not kill an already spawned session.
#![cfg(debug_assertions)]

mod cli_common;

use cli_common::{Rig, wait_until};
use std::process::{Child, Stdio};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn failed_claim_hand_back_keeps_the_successfully_spawned_session_serving() {
    let rig = Rig::new();
    rig.ok(&["run", "-d", "--id", "return-failure", "--", "/bin/sh", "-c",
        "printf serving; exec cat"]);
    let barriers = rig.scratch.join("barriers");
    std::fs::create_dir(&barriers).unwrap();
    std::fs::write(barriers.join("before-claim-hand-back.hold"), "").unwrap();
    let mut restart = ChildGuard(rig.cmd(&["restart", "-y", "return-failure"])
        .env("PTY_TEST_OPERATION_BARRIERS", &barriers)
        .env("PTY_SESSION", "test-coordinator")
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    wait_until("new daemon reaches hand-back after child spawn", || {
        barriers.join("before-claim-hand-back.reached").exists()
    });
    let metadata = rig.read_meta("return-failure").unwrap();
    let daemon_pid = metadata["daemonPid"].as_i64().unwrap() as i32;
    let lock = rig.path("return-failure.lock");
    let displaced = rig.scratch.join("displaced-daemon-claim");
    std::fs::rename(&lock, &displaced).unwrap();
    std::fs::create_dir(&lock).unwrap();
    // Returning the claim now encounters a real directory where its daemon
    // owner record must be, failing validation on both Linux and macOS.
    std::fs::write(barriers.join("before-claim-hand-back.release"), "").unwrap();
    wait_until("restart reports successful launch despite return failure", || {
        restart.0.try_wait().unwrap().is_some()
    });
    assert!(restart.0.wait().unwrap().success());
    std::fs::remove_dir(&lock).unwrap();
    assert!(std::os::unix::net::UnixStream::connect(rig.path("return-failure.sock")).is_ok());
    // SAFETY: signal zero only probes the exact daemon from the new metadata.
    assert_eq!(unsafe { libc::kill(daemon_pid, 0) }, 0);
    wait_until("spawned child output remains available", || {
        let peek = rig.run(&["peek", "return-failure"]);
        peek.code == 0 && peek.stdout.contains("serving")
    });
}
