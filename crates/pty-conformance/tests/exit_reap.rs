//! Port of tests/exit-reap.test.ts, the reap-policy half (lines 668-933): the
//! shipped default reaps a finished session, `PTY_REAP_ON_EXIT=false`
//! preserves it, `keep=true` / `strategy=permanent` / `pty kill` exempt,
//! `--ephemeral` forces a reap, and what gc still has to sweep. The
//! exact-generation evidence half (`pty evidence`, lines 203-666) is deferred.
//!
//! Sessions are created with `pty run -d`; `PTY_REAP_ON_EXIT` is passed in
//! the `run` invocation's environment, which the daemon inherits.

use pty_conformance::*;
use std::time::Duration;

fn session_files(rig: &Rig, name: &str) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(rig.root())
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|f| f.starts_with(name))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn has_json(rig: &Rig, name: &str) -> bool {
    session_files(rig, name).iter().any(|f| f.ends_with(".json"))
}

/// Wait until `name` has no files left, or the budget runs out; returns the
/// survivors so the assertion reports what stayed.
fn wait_for_gone(rig: &Rig, name: &str) -> Vec<String> {
    let _ = poll_for(Duration::from_secs(6), || session_files(rig, name).is_empty());
    session_files(rig, name)
}

/// `pty run -d` for a command that may already have exited by the time the
/// launcher returns; returns the daemon pid if it was still recorded.
fn launch(rig: &Rig, name: &str, cmd: &[&str], opts: DaemonOpts) -> Option<i32> {
    let d = rig.daemon_try(name, cmd, opts);
    assert_eq!(d.launch.status, 0, "run -d failed: {}", d.launch.summary());
    rig.pid(name)
}

fn wait_for_daemon_exit(pid: Option<i32>) {
    if let Some(pid) = pid {
        let _ = poll_for(Duration::from_secs(6), || !pid_alive(pid));
    }
}

/// Wait for the daemon (if still recorded) to leave, then a settle.
fn wait_for_exit_settle(rig: &Rig, name: &str, pid: Option<i32>) {
    wait_for_daemon_exit(pid);
    wait_until(&format!("{name} exit record"), || {
        rig.meta(name).map(|m| m.get("exitCode").is_some()).unwrap_or(false)
    });
    std::thread::sleep(Duration::from_millis(1000));
}

const PRESERVE: &[(&str, &str)] = &[("PTY_REAP_ON_EXIT", "false")];

fn preserve() -> DaemonOpts {
    let mut o = DaemonOpts::no_display_name();
    o.invoke_env = PRESERVE.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    o
}

// ── exit-time reap: sessions that clean themselves up ──

/// node: tests/exit-reap.test.ts:669
#[test]
fn removes_a_session_that_exits_cleanly() {
    let rig = Rig::new();
    let name = unique_id("xr");
    let pid = launch(&rig, &name, &["true"], DaemonOpts::no_display_name());
    wait_for_daemon_exit(pid);
    assert_eq!(wait_for_gone(&rig, &name), Vec::<String>::new());
}

/// node: tests/exit-reap.test.ts:678
#[test]
fn removes_a_session_that_exits_nonzero() {
    let rig = Rig::new();
    let name = unique_id("xr");
    let pid = launch(&rig, &name, &["sh", "-c", "exit 3"], DaemonOpts::no_display_name());
    wait_for_daemon_exit(pid);
    assert_eq!(wait_for_gone(&rig, &name), Vec::<String>::new());
}

/// node: tests/exit-reap.test.ts:690
#[test]
fn removes_the_events_file_with_the_metadata() {
    let rig = Rig::new();
    let name = unique_id("xr");
    let pid = launch(&rig, &name, &["true"], DaemonOpts::no_display_name());
    wait_for_daemon_exit(pid);
    let files = wait_for_gone(&rig, &name);
    let events: Vec<&String> = files.iter().filter(|f| f.ends_with(".events.jsonl")).collect();
    assert!(events.is_empty(), "{files:?}");
}

/// node: tests/exit-reap.test.ts:702
#[test]
fn leaves_nothing_for_gc() {
    let rig = Rig::new();
    let name = unique_id("xr");
    let pid = launch(&rig, &name, &["true"], DaemonOpts::no_display_name());
    wait_for_daemon_exit(pid);
    wait_for_gone(&rig, &name);
    assert_valid_retained_exits(&rig);
    let receipt_path = rig.root().join(pty_core::registry::retained_exit::RETAINED_EXIT_FILE_NAME);
    let before = std::fs::read(&receipt_path).ok();
    let gc = rig.pty(&["gc"]);
    let s = gc.stdout();
    expect_not_contains(&s, &name);
    expect_contains(&s, "Nothing to clean up.");
    assert_eq!(std::fs::read(receipt_path).ok(), before, "GC must preserve durable exit receipts");
    assert_valid_retained_exits(&rig);
}

/// The cross-implementation matrix supplies the pinned pre-receipt Node
/// consumer. Exercise its actual GC against a receipt from the new Rust daemon.
#[test]
fn legacy_node_gc_preserves_new_producer_exit_receipts() {
    if !is_rust() {
        return;
    }
    let Some(node_checkout) = std::env::var_os("PTY_NODE_CHECKOUT") else {
        eprintln!("cross-version GC regression runs in the dual-binary conformance matrix");
        return;
    };
    let node = std::path::PathBuf::from(node_checkout).join("bin/pty");
    assert!(node.is_file(), "pinned legacy Node consumer must exist");
    let rig = Rig::new();
    let name = unique_id("legacy-receipt");
    let pid = launch(
        &rig, &name, &["sh", "-c", "printf 'fatal: fixture failure\\n'; exit 23"],
        DaemonOpts::no_display_name(),
    );
    wait_for_daemon_exit(pid);
    assert!(wait_for_gone(&rig, &name).is_empty());
    let path = rig.root().join(pty_core::registry::retained_exit::RETAINED_EXIT_FILE_NAME);
    let before = std::fs::read(&path).expect("new Rust producer must retain its exit");
    assert_valid_retained_exits(&rig);
    let rows: Vec<pty_core::registry::RetainedSessionExit> = serde_json::from_slice(&before).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, name);
    assert_eq!(rows[0].exit_code, Some(23));
    assert!(rows[0].output_tail.contains("fatal: fixture failure"));
    let output = std::process::Command::new(node).arg("gc")
        .env_clear().envs(rig.base_env()).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(std::fs::read(path).unwrap(), before, "legacy GC must not discover the receipt");
    assert_valid_retained_exits(&rig);
}

// ── config default = preserve (PTY_REAP_ON_EXIT=false) ──

/// node: tests/exit-reap.test.ts:719
#[test]
fn preserve_default_keeps_a_finished_session() {
    let rig = Rig::new();
    let name = unique_id("xr");
    let pid = launch(&rig, &name, &["true"], preserve());
    wait_for_exit_settle(&rig, &name, pid);
    assert!(has_json(&rig, &name), "{:?}", session_files(&rig, &name));
}

/// node: tests/exit-reap.test.ts:731
#[test]
fn preserve_default_still_lets_gc_sweep() {
    let rig = Rig::new();
    let name = unique_id("xr");
    let pid = launch(&rig, &name, &["true"], preserve());
    wait_for_exit_settle(&rig, &name, pid);
    assert!(has_json(&rig, &name));
    let gc = rig.pty(&["gc"]);
    expect_contains(&gc.stdout(), &format!("Removed: {name}"));
    assert_eq!(session_files(&rig, &name), Vec::<String>::new());
}

/// node: tests/exit-reap.test.ts:743
#[test]
fn ephemeral_forces_a_reap_under_the_preserve_default() {
    let rig = Rig::new();
    let name = unique_id("xr");
    let pid = launch(&rig, &name, &["true"], preserve().ephemeral());
    wait_for_daemon_exit(pid);
    assert_eq!(wait_for_gone(&rig, &name), Vec::<String>::new());
}

// ── exit-time reap: exemptions ──

/// node: tests/exit-reap.test.ts:757
#[test]
fn retains_a_session_tagged_keep() {
    let rig = Rig::new();
    let name = unique_id("xr");
    let pid = launch(&rig, &name, &["true"], DaemonOpts::keep());
    wait_for_exit_settle(&rig, &name, pid);
    assert!(has_json(&rig, &name));
}

/// node: tests/exit-reap.test.ts:769
#[test]
fn honours_a_keep_tag_applied_while_running() {
    let rig = Rig::new();
    let name = unique_id("xr");
    let d = rig.daemon(&name, &["sh", "-c", "sleep 3"], DaemonOpts::no_display_name());
    let pid = d.pid();
    let tagged = rig.pty(&["tag", &name, "keep=true"]);
    expect_status(&tagged, 0);
    let _ = poll_for(Duration::from_secs(15), || !pid_alive(pid));
    std::thread::sleep(Duration::from_millis(1000));
    assert!(has_json(&rig, &name), "{:?}", session_files(&rig, &name));
}

/// node: tests/exit-reap.test.ts:786
#[test]
fn keep_false_is_no_exemption() {
    let rig = Rig::new();
    let name = unique_id("xr");
    let pid = launch(&rig, &name, &["true"], DaemonOpts::no_display_name().tag("keep", "false"));
    wait_for_daemon_exit(pid);
    assert_eq!(wait_for_gone(&rig, &name), Vec::<String>::new());
}

/// node: tests/exit-reap.test.ts:797
#[test]
fn retains_a_permanent_session() {
    let rig = Rig::new();
    let name = unique_id("xr");
    let pid = launch(
        &rig,
        &name,
        &["true"],
        DaemonOpts::no_display_name().tag("strategy", "permanent"),
    );
    wait_for_exit_settle(&rig, &name, pid);
    assert!(has_json(&rig, &name));
}

/// node: tests/exit-reap.test.ts:809
#[test]
fn retains_metadata_after_pty_kill() {
    let rig = Rig::new();
    let name = unique_id("xr");
    let d = rig.daemon(&name, &["cat"], DaemonOpts::no_display_name());
    let pid = d.pid();
    expect_contains(&rig.pty(&["kill", &name]).stdout(), "killed");
    let _ = poll_for(Duration::from_secs(6), || !pid_alive(pid));
    std::thread::sleep(Duration::from_millis(500));
    let files = session_files(&rig, &name);
    assert!(files.iter().any(|f| f.ends_with(".json")), "{files:?}");
    assert!(!files.iter().any(|f| f.ends_with(".sock")), "{files:?}");
}

/// node: tests/exit-reap.test.ts:826
#[test]
fn ephemeral_still_reaps_a_permanent_session() {
    let rig = Rig::new();
    let name = unique_id("xr");
    let pid = launch(
        &rig,
        &name,
        &["true"],
        DaemonOpts::no_display_name().tag("strategy", "permanent").ephemeral(),
    );
    wait_for_daemon_exit(pid);
    assert_eq!(wait_for_gone(&rig, &name), Vec::<String>::new());
}

/// node: tests/exit-reap.test.ts:841
#[test]
fn keep_wins_over_ephemeral() {
    let rig = Rig::new();
    let name = unique_id("xr");
    let pid = launch(&rig, &name, &["true"], DaemonOpts::keep().ephemeral());
    wait_for_exit_settle(&rig, &name, pid);
    assert!(has_json(&rig, &name));
}

/// node: tests/exit-reap.test.ts:855
#[test]
fn rm_removes_a_kept_session() {
    let rig = Rig::new();
    let name = unique_id("xr");
    let pid = launch(&rig, &name, &["true"], DaemonOpts::keep());
    wait_for_exit_settle(&rig, &name, pid);
    expect_contains(&rig.pty(&["rm", &name]).stdout(), "removed");
    assert_eq!(session_files(&rig, &name), Vec::<String>::new());
}

// ── what the exit-time reap structurally cannot cover ──

/// node: tests/exit-reap.test.ts:871
#[test]
fn a_vanished_session_is_left_for_gc() {
    let rig = Rig::new();
    let name = unique_id("xr");
    let d = rig.daemon(&name, &["sh", "-c", "sleep 60"], DaemonOpts::no_display_name());
    let pid = d.pid();
    kill_pid(pid, libc::SIGKILL);
    let _ = poll_for(Duration::from_secs(6), || !pid_alive(pid));
    std::thread::sleep(Duration::from_millis(500));
    assert!(has_json(&rig, &name), "no exit record can have been written");
    let gc = rig.pty(&["gc"]);
    expect_contains(&gc.stdout(), &format!("Removed: {name}"));
    assert_eq!(session_files(&rig, &name), Vec::<String>::new());
}

/// node: tests/exit-reap.test.ts:893
#[test]
fn gc_still_sweeps_a_killed_session() {
    let rig = Rig::new();
    let name = unique_id("xr");
    let d = rig.daemon(&name, &["cat"], DaemonOpts::no_display_name());
    let pid = d.pid();
    expect_contains(&rig.pty(&["kill", &name]).stdout(), "killed");
    let _ = poll_for(Duration::from_secs(6), || !pid_alive(pid));
    std::thread::sleep(Duration::from_millis(500));
    assert!(has_json(&rig, &name));
    let gc = rig.pty(&["gc"]);
    expect_contains(&gc.stdout(), &format!("Removed: {name}"));
    assert_eq!(session_files(&rig, &name), Vec::<String>::new());
}

/// node: tests/exit-reap.test.ts:915
#[test]
fn gc_reports_kept_sessions() {
    let rig = Rig::new();
    let name = unique_id("xr");
    let pid = launch(&rig, &name, &["true"], DaemonOpts::keep());
    wait_for_exit_settle(&rig, &name, pid);
    let gc = rig.pty(&["gc"]);
    expect_contains(&gc.stdout(), &format!("Kept (keep tag): {name}"));
    assert!(has_json(&rig, &name));
}
