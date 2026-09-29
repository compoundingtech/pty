//! `pty kill` prints "killed" only about a tree it has checked.
//!
//! The old contract asked for the daemon and reported on the session. A
//! session is a daemon plus a child plus whatever the child started, so the
//! word "killed" was a claim about processes the command never looked at.
//! These tests hold both binaries to the narrower claim.

use std::time::Duration;

use pty_conformance::*;

/// Every descendant pid of `root`, from `ps`.
fn descendants(root: i32) -> Vec<i32> {
    let out = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid="])
        .output()
        .expect("ps");
    let listing = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut children: std::collections::HashMap<i32, Vec<i32>> = Default::default();
    for line in listing.lines() {
        let mut it = line.split_whitespace();
        if let (Some(p), Some(pp)) = (it.next(), it.next())
            && let (Ok(p), Ok(pp)) = (p.parse::<i32>(), pp.parse::<i32>())
        {
            children.entry(pp).or_default().push(p);
        }
    }
    let mut out = Vec::new();
    let mut queue: std::collections::VecDeque<i32> =
        children.get(&root).cloned().unwrap_or_default().into();
    while let Some(pid) = queue.pop_front() {
        if out.contains(&pid) {
            continue;
        }
        out.push(pid);
        for &c in children.get(&pid).map(|v| v.as_slice()).unwrap_or(&[]) {
            queue.push_back(c);
        }
    }
    out
}

/// The success line is now a statement about the whole tree, so the tree has
/// to be gone for it to appear.
///
/// The existing kill test asserts only that stdout contains "killed". That
/// passes whether or not a child outlived the daemon, which is the exact
/// false pass this command shipped with.
#[test]
fn killed_means_the_descendants_are_gone_too() {
    let rig = Rig::new();
    let d = rig.daemon(
        "kr1",
        &["sh", "-c", "sleep 120"],
        DaemonOpts::no_display_name(),
    );
    let daemon_pid = d.pid();
    assert!(
        poll_for(Duration::from_secs(5), || !descendants(daemon_pid)
            .is_empty()),
        "the session never started a child process to test against"
    );
    let tree = descendants(daemon_pid);

    let out = rig.pty(&["kill", "kr1"]);
    expect_status(&out, 0);
    expect_contains(&out.stdout(), "killed");

    assert!(process_exited(daemon_pid), "daemon {daemon_pid} outlived kill");
    let alive: Vec<i32> = tree.into_iter().filter(|&p| !process_exited(p)).collect();
    assert!(
        alive.is_empty(),
        "kill said \"killed\" while these processes were still running: {alive:?}"
    );
}

/// The status has to agree with the words. A caller that reads only the exit
/// code must reach the same conclusion as one that reads the output, or the
/// honest line is decoration.
#[test]
fn a_verified_kill_exits_zero_and_an_unverified_one_does_not() {
    let rig = Rig::new();
    let d = rig.daemon("kr4", &["cat"], DaemonOpts::no_display_name());
    let daemon_pid = d.pid();
    let out = rig.pty(&["kill", "kr4"]);
    let stdout = out.stdout();

    // This session is ordinary, so the tree should be empty and the status 0.
    let tree_empty = descendants(daemon_pid).into_iter().all(process_exited);
    assert!(tree_empty, "precondition: the tree should be gone");
    expect_status(&out, 0);
    assert!(
        stdout.contains("killed"),
        "a verified kill says so: {stdout:?}"
    );
}

/// The honest report goes somewhere a person reads. A clean kill must not
/// invent a warning, or the real one stops meaning anything.
#[test]
fn a_clean_kill_says_nothing_on_stderr() {
    let rig = Rig::new();
    rig.daemon("kr2", &["cat"], DaemonOpts::no_display_name());
    let out = rig.pty(&["kill", "kr2"]);
    expect_status(&out, 0);
    let err = out.stderr();
    assert!(
        !err.contains("survived") && !err.contains("may still be running"),
        "a clean kill reported survivors: {err:?}"
    );
}

/// `killed` and the survivor report are alternatives, never both. A reader
/// who greps for the success line must not find it next to a warning that
/// contradicts it.
#[test]
fn the_success_line_and_a_survivor_report_never_appear_together() {
    let rig = Rig::new();
    rig.daemon("kr3", &["cat"], DaemonOpts::no_display_name());
    let out = rig.pty(&["kill", "kr3"]);
    let (stdout, stderr) = (out.stdout(), out.stderr());
    let claimed_killed = stdout.contains("killed");
    let reported_survivors =
        stderr.contains("survived") || stderr.contains("may still be running");
    assert!(
        !(claimed_killed && reported_survivors),
        "kill claimed success and reported survivors at once:\nstdout: {stdout:?}\nstderr: {stderr:?}"
    );
}
