//! Port of tests/security-fixes.test.ts through the binary. BUG-1 (socket
//! path length) is pinned by `pty run -d --id <name>` on names whose socket
//! path would overflow the 104-byte `sun_path` limit; BUG-2 (creation lock
//! semantics) by planting `<id>.lock` files: a live holder blocks `pty run`,
//! a dead or garbage holder is stolen, and two concurrent stealers cannot
//! both win.
//!
//! Left out: "release is idempotent" (:73 — `releaseLock` on a name that was
//! never locked has no CLI-observable counterpart).

use pty_conformance::*;
use std::process::Stdio;

fn root_entries(rig: &Rig, prefix: &str) -> Vec<String> {
    std::fs::read_dir(rig.root())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|f| f.starts_with(prefix))
        .collect()
}

/// node: tests/security-fixes.test.ts:24
#[test]
fn accepts_ordinary_names() {
    let rig = Rig::new();
    let d = rig.daemon("myserver", &["cat"], DaemonOpts::no_display_name());
    expect_contains(&d.launch.stdout(), "Session \"myserver\" created.");
    assert!(rig.socket_path("myserver").exists());
}

/// node: tests/security-fixes.test.ts:28
#[test]
fn rejects_names_that_overflow_the_socket_path_limit() {
    let rig = Rig::new();
    let long = "a".repeat(100);
    let out = rig.pty(&["run", "-d", "--id", &long, "--", "cat"]);
    expect_failure(&out);
    expect_regex(&out.stderr(), "socket path.*exceeds");
    expect_regex(&out.stderr(), "10[34]-byte kernel limit");
    assert!(root_entries(&rig, "aaaa").is_empty());
}

/// node: tests/security-fixes.test.ts:34
#[test]
fn rejects_names_whose_path_hits_the_limit_plus_one() {
    let rig = Rig::new();
    let overhead = rig.root().join(".sock").to_string_lossy().len();
    let overshoot = "a".repeat((104 - overhead + 1).max(1));
    let out = rig.pty(&["run", "-d", "--id", &overshoot, "--", "cat"]);
    expect_failure(&out);
    expect_regex(&out.stderr(), "socket path");
    assert!(root_entries(&rig, "aaaa").is_empty());
}

/// node: tests/security-fixes.test.ts:41
#[test]
fn rejects_bad_characters_before_checking_length() {
    let rig = Rig::new();
    let out = rig.pty(&["run", "-d", "--id", "has/slash", "--", "cat"]);
    expect_failure(&out);
    expect_contains(&out.stderr(), "Invalid session name \"has/slash\"");
    expect_contains(&out.stderr(), "letters, numbers, dots, hyphens, and underscores");
    let out = rig.pty(&["run", "-d", "--id", "..", "--", "cat"]);
    expect_failure(&out);
    expect_contains(&out.stderr(), "Invalid session name \"..\"");
    let too_long = "a".repeat(256);
    let out = rig.pty(&["run", "-d", "--id", &too_long, "--", "cat"]);
    expect_failure(&out);
    expect_contains(&out.stderr(), "Session name too long (max 255 characters).");
    assert!(rig.list_json().is_empty());
}

/// node: tests/security-fixes.test.ts:47
#[test]
fn live_lock_holder_blocks_a_second_creator() {
    let rig = Rig::new();
    let lock = rig.root().join("race1.lock");
    let me = std::process::id().to_string();
    std::fs::write(&lock, &me).unwrap();
    let out = rig.pty(&["run", "-d", "--id", "race1", "--no-display-name", "--", "cat"]);
    expect_status(&out, 1);
    expect_contains(&out.stderr(), "Session \"race1\" is being created by another process. Try again.");
    assert_eq!(root_entries(&rig, "race1"), vec!["race1.lock".to_string()]);
    assert_eq!(std::fs::read_to_string(&lock).unwrap(), me, "lock was stolen from a live holder");
    std::fs::remove_file(&lock).unwrap();
    let out = rig.pty(&["run", "-d", "--id", "race1", "--no-display-name", "--", "cat"]);
    expect_status(&out, 0);
}

/// node: tests/security-fixes.test.ts:53
#[test]
fn steals_a_stale_lock_whose_holder_is_dead() {
    let rig = Rig::new();
    let lock = rig.root().join("race2.lock");
    std::fs::write(&lock, "2147483646").unwrap();
    let d = rig.daemon("race2", &["cat"], DaemonOpts::no_display_name());
    expect_contains(&d.launch.stdout(), "Session \"race2\" created.");
    assert!(!lock.exists(), "stolen creation lock was not released");
    // A metadata mutation steals a stale lock the same way.
    std::fs::write(&lock, "2147483646").unwrap();
    let out = rig.pty(&["tag", "race2", "role=web"]);
    expect_status(&out, 0);
    assert_eq!(rig.meta("race2").unwrap()["tags"]["role"], "web");
    assert!(!lock.exists(), "stolen metadata lock was not released");
}

/// node: tests/security-fixes.test.ts:67
#[test]
fn garbage_lock_content_is_treated_as_stale() {
    let rig = Rig::new();
    let lock = rig.root().join("race3.lock");
    std::fs::write(&lock, "not a pid").unwrap();
    let d = rig.daemon("race3", &["cat"], DaemonOpts::no_display_name());
    expect_contains(&d.launch.stdout(), "Session \"race3\" created.");
    assert!(!lock.exists());
}

/// node: tests/security-fixes.test.ts:77
///
/// Rust's atomic complete-owner publication and inode-validated stale steal
/// make exactly one claimant win. This process-level test covers the public
/// CLI; `pty_core::registry::lock` has a deterministic delayed-stealer
/// regression for the damaging unlink interleaving.
#[test]
fn concurrent_stealers_cannot_both_win() {
    let rig = Rig::new();
    let lock = rig.root().join("race4.lock");
    std::fs::write(&lock, "2147483646").unwrap();
    let mut children = Vec::new();
    for _ in 0..2 {
        let mut cmd = rig.command(&["run", "-d", "--id", "race4", "--no-display-name", "--", "cat"]);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        children.push(cmd.spawn().unwrap());
    }
    let outs: Vec<std::process::Output> = children.into_iter().map(|c| c.wait_with_output().unwrap()).collect();
    let winners = outs.iter().filter(|o| o.status.success()).count();
    assert_eq!(winners, 1, "{:?}", outs.iter().map(|o| String::from_utf8_lossy(&o.stderr).into_owned()).collect::<Vec<_>>());
    let loser = outs.iter().find(|o| !o.status.success()).unwrap();
    // "already in use" is the loser that started after the winner had
    // published: `pty run`'s pre-check refuses it before it reaches the lock.
    // Two launches spawned together still run one after the other on a
    // loaded machine; that failed this test once in 24 whole-workspace runs.
    expect_regex(
        &String::from_utf8_lossy(&loser.stderr),
        "is being created by another process|is already running|event log is busy|is already in use",
    );
    let list = rig.list_json();
    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(list[0]["name"], "race4");
    assert!(!lock.exists());
}
