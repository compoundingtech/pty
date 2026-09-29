//! Port of tests/rm-immediate-reuse.test.ts, the CLI half: `pty rm` waits
//! out the old generation's daemon so an immediate same-name `run -d` is
//! never unlinked by the old daemon's deferred cleanup. The
//! `cleanupOwnedSocket`/`cleanupOwnedAll` library case stays in Node.

use pty_conformance::*;
use std::time::Duration;

fn wait_for_exit_metadata(rig: &Rig, name: &str) -> serde_json::Value {
    wait_until_for(&format!("{name} exit metadata"), Duration::from_secs(5), &mut || {
        rig.meta(name).map(|m| m["exitedAt"].is_string()).unwrap_or(false)
    });
    rig.meta(name).unwrap()
}

/// node: tests/rm-immediate-reuse.test.ts:99
#[test]
fn rm_waits_out_the_old_generation_before_permitting_replacement() {
    let rig = Rig::new();
    let name = "reuse";
    // Repeat the formerly 500 ms race enough times that success is a
    // lifecycle contract, not scheduling luck.
    for iteration in 0..5 {
        let first = rig.pty(&[
            "run", "-d", "--id", name, "--tag", "keep=true", "--", "sh", "-c", "sleep 0.05; exit 0",
        ]);
        expect_status(&first, 0);

        let old = wait_for_exit_metadata(&rig, name);
        let old_pid = old["daemonPid"].as_i64().unwrap_or(0) as i32;
        let old_generation = old["generation"].as_str().unwrap_or("").to_string();
        assert!(old_pid > 0, "[{iteration}] daemonPid missing: {old}");
        assert!(!old_generation.is_empty(), "[{iteration}] generation missing: {old}");
        assert!(pid_alive(old_pid), "[{iteration}] old daemon already gone");

        let removed = rig.pty(&["rm", name]);
        expect_status(&removed, 0);
        expect_contains(&removed.stdout(), "removed");
        assert!(process_exited(old_pid), "[{iteration}] rm returned with the old daemon alive");

        let replacement = rig.pty(&["run", "-d", "--id", name, "--", "cat"]);
        expect_status(&replacement, 0);
        let rmeta = rig.meta(name).expect("replacement metadata");
        let rpid = rmeta["daemonPid"].as_i64().unwrap() as i32;
        assert_ne!(rmeta["generation"], old_generation, "[{iteration}]");

        // Cross the old daemon's former deferred-cleanup window.
        std::thread::sleep(Duration::from_millis(650));
        assert!(rig.socket_path(name).exists(), "[{iteration}] replacement socket unlinked");
        assert_eq!(rig.pid(name), Some(rpid), "[{iteration}] replacement pid file clobbered");
        assert_eq!(rig.meta(name).unwrap()["generation"], rmeta["generation"], "[{iteration}]");
        assert!(pid_alive(rpid), "[{iteration}] replacement daemon died");

        expect_status(&rig.pty(&["kill", name]), 0);
        expect_status(&rig.pty(&["rm", name]), 0);
    }
}
