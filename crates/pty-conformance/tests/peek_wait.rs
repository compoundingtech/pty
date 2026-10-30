//! Port of tests/peek-wait.test.ts: `peek --full`, `peek --wait` (live,
//! already-on-screen, multiple patterns, exited sessions), and
//! `events --wait`.

use pty_conformance::*;
use serde_json::Value;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

/// node: tests/peek-wait.test.ts:81
#[test]
fn peek_full_shows_the_scrollback() {
    let rig = Rig::new();
    let name = unique_id("pw");
    rig.daemon(
        &name,
        &["sh", "-c", "for i in $(seq 1 100); do echo line$i; done; exec cat"],
        DaemonOpts::no_display_name(),
    );
    std::thread::sleep(Duration::from_millis(500));
    let normal = rig.pty(&["peek", "--plain", &name]);
    let full = rig.pty(&["peek", "--plain", "--full", &name]);
    expect_status(&normal, 0);
    expect_status(&full, 0);
    let normal_lines = normal.stdout().trim().lines().count();
    let full_lines = full.stdout().trim().lines().count();
    assert!(full_lines > normal_lines, "full={full_lines} normal={normal_lines}");
    assert!(full_lines >= 100, "full={full_lines}");
    let s = full.stdout();
    expect_contains(&s, "line1");
    expect_contains(&s, "line100");
}

/// node: tests/peek-wait.test.ts:101
#[test]
fn wait_until_text_appears() {
    let rig = Rig::new();
    let name = unique_id("pw");
    rig.daemon(&name, &["sh", "-c", "sleep 0.5; echo READY; exec cat"], DaemonOpts::no_display_name());
    let r = rig.pty(&["peek", "--wait", "READY", "-t", "5", "--plain", &name]);
    expect_status(&r, 0);
    expect_contains(&r.stdout(), "READY");
}

/// node: tests/peek-wait.test.ts:112
#[test]
fn wait_times_out_when_text_never_appears() {
    let rig = Rig::new();
    let name = unique_id("pw");
    rig.daemon(&name, &["cat"], DaemonOpts::no_display_name());
    let r = rig.pty(&["peek", "--wait", "NEVER", "-t", "1", "--plain", &name]);
    expect_status(&r, 1);
    let err = r.stderr();
    expect_contains(&err, "Timed out");
    expect_contains(&err, "NEVER");
}

/// node: tests/peek-wait.test.ts:123
#[test]
fn wait_returns_immediately_when_text_is_already_on_screen() {
    let rig = Rig::new();
    let name = unique_id("pw");
    rig.daemon(&name, &["sh", "-c", "echo ALREADY; exec cat"], DaemonOpts::no_display_name());
    std::thread::sleep(Duration::from_millis(300));
    let start = Instant::now();
    let r = rig.pty(&["peek", "--wait", "ALREADY", "-t", "5", "--plain", &name]);
    let elapsed = start.elapsed();
    expect_status(&r, 0);
    expect_contains(&r.stdout(), "ALREADY");
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
}

/// node: tests/peek-wait.test.ts:138
#[test]
fn multiple_wait_patterns_match_any() {
    let rig = Rig::new();
    let name = unique_id("pw");
    rig.daemon(&name, &["sh", "-c", "echo SECOND; exec cat"], DaemonOpts::no_display_name());
    std::thread::sleep(Duration::from_millis(300));
    let r = rig.pty(&["peek", "--wait", "FIRST", "--wait", "SECOND", "-t", "5", "--plain", &name]);
    expect_status(&r, 0);
    expect_contains(&r.stdout(), "SECOND");
}

/// node: tests/peek-wait.test.ts:149
#[test]
fn wait_reads_saved_output_from_an_exited_session() {
    let rig = Rig::new();
    let name = unique_id("pw");
    rig.daemon(&name, &["sh", "-c", "echo TEST_PASSED; exit 0"], DaemonOpts::keep());
    rig.wait_for_exit(&name);
    std::thread::sleep(Duration::from_millis(500));
    let r = rig.pty(&["peek", "--wait", "TEST_PASSED", "-t", "5", "--plain", &name]);
    expect_status(&r, 0);
    expect_contains(&r.stdout(), "TEST_PASSED");
}

/// node: tests/peek-wait.test.ts:162
#[test]
fn wait_on_an_exited_session_errors_when_the_pattern_is_missing() {
    let rig = Rig::new();
    let name = unique_id("pw");
    rig.daemon(&name, &["sh", "-c", "echo nope; exit 0"], DaemonOpts::keep());
    rig.wait_for_exit(&name);
    std::thread::sleep(Duration::from_millis(500));
    let r = rig.pty(&["peek", "--wait", "MISSING", "-t", "5", "--plain", &name]);
    expect_status(&r, 1);
    let err = r.stderr();
    expect_contains(&err, "exited");
    expect_contains(&err, "MISSING");
}

/// node: tests/peek-wait.test.ts:175
#[test]
fn peek_plain_on_an_exited_session_shows_saved_output() {
    let rig = Rig::new();
    let name = unique_id("pw");
    rig.daemon(&name, &["sh", "-c", "echo SAVED_OUTPUT; exit 0"], DaemonOpts::keep());
    rig.wait_for_exit(&name);
    std::thread::sleep(Duration::from_millis(500));
    let r = rig.pty(&["peek", "--plain", &name]);
    expect_status(&r, 0);
    expect_contains(&r.stdout(), "SAVED_OUTPUT");
}

// ── pty events --wait ──

/// node: tests/peek-wait.test.ts:189
#[test]
fn events_wait_for_a_specific_event_type() {
    let rig = Rig::new();
    let name = unique_id("pw");
    rig.daemon(
        &name,
        &["sh", "-c", "sleep 2; printf '\\007'; exec cat"],
        DaemonOpts::no_display_name(),
    );
    let r = rig.pty(&["events", "--wait", "bell", "-t", "10", &name]);
    expect_status(&r, 0);
    expect_contains(&r.stdout(), "bell");
}

/// node: tests/peek-wait.test.ts:199
#[test]
fn events_wait_times_out_when_the_event_never_occurs() {
    let rig = Rig::new();
    let name = unique_id("pw");
    rig.daemon(&name, &["cat"], DaemonOpts::no_display_name());
    let start = Instant::now();
    let r = rig.run_with_timeout(
        rig.command(&["events", &name, "--wait", "session_exit", "--timeout", "1"]),
        None,
        Duration::from_secs(5),
    );
    let elapsed = start.elapsed();
    expect_status(&r, 1);
    assert!(!r.timed_out, "events ignored its timeout: {}", r.summary());
    expect_contains(&r.stderr(), "Timed out");
    expect_contains(&r.stderr(), "session_exit");
    assert!(elapsed >= Duration::from_millis(900), "returned before timeout: {elapsed:?}");
    assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
}

fn wait_for_events_command(mut child: Child) -> Out {
    let finished = poll_for(Duration::from_secs(5), || {
        child.try_wait().expect("poll events --wait").is_some()
    });
    if !finished {
        let _ = child.kill();
    }
    let output = child.wait_with_output().expect("collect events --wait");
    Out {
        status: output.status.code().unwrap_or(-1),
        stdout: output.stdout,
        stderr: output.stderr,
        timed_out: !finished,
    }
}

fn exit_event(out: &Out, name: &str, code: i32) -> Value {
    expect_status(out, 0);
    assert!(!out.timed_out, "events --wait hung: {}", out.summary());
    let event: Value = serde_json::from_str(out.stdout().trim()).expect("one session_exit JSON event");
    assert_eq!(event["type"], "session_exit");
    assert_eq!(event["session"], name);
    assert_eq!(event["exitCode"], code);
    assert!(event["ts"].as_str().is_some_and(|ts| !ts.is_empty()), "{event}");
    event
}

#[test]
fn events_wait_returns_success_when_a_live_session_exits() {
    let rig = Rig::new();
    let name = unique_id("pw");
    rig.daemon(
        &name,
        &["sh", "-c", "printf 'EXIT_GATE_READY\\n'; read -r gate; exit 23"],
        DaemonOpts::keep(),
    );
    rig.wait_for_screen(&name, "EXIT_GATE_READY");
    let mut input = rig.connect(&name);
    input.attach(24, 80);
    let child = rig
        .command(&["events", &name, "--wait", "session_exit", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn events --wait before releasing the live child");
    // Release only after the waiter has been launched. Exit may race follower
    // setup: both the subscription path and its metadata recheck must work.
    input.data(b"go\n");
    exit_event(&wait_for_events_command(child), &name, 23);
}

#[test]
fn events_wait_returns_the_retained_exit_event_for_an_already_exited_session() {
    let rig = Rig::new();
    let name = unique_id("pw");
    let daemon = rig.daemon(&name, &["sh", "-c", "exit 37"], DaemonOpts::keep());
    rig.wait_for_exit(&name);
    let metadata = daemon.meta();
    wait_for_process_gone(metadata["daemonPid"].as_i64().expect("daemonPid") as i32);
    let log = std::fs::read_to_string(rig.root().join(format!("{name}.events.jsonl")))
        .expect("retained event log");
    let retained = log
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("event JSON"))
        .find(|event| event["type"] == "session_exit")
        .expect("retained session_exit");
    let out = rig.run_with_timeout(
        rig.command(&["events", &name, "--wait", "session_exit", "--json"]),
        None,
        Duration::from_secs(5),
    );
    assert_eq!(exit_event(&out, &name, 37), retained);
}

#[test]
fn events_wait_reconstructs_an_already_exited_session_event_when_the_log_is_missing() {
    let rig = Rig::new();
    let name = unique_id("pw");
    let daemon = rig.daemon(&name, &["sh", "-c", "exit 41"], DaemonOpts::keep());
    rig.wait_for_exit(&name);
    let metadata = daemon.meta();
    wait_for_process_gone(metadata["daemonPid"].as_i64().expect("daemonPid") as i32);
    std::fs::remove_file(rig.root().join(format!("{name}.events.jsonl")))
        .expect("remove retained event log after the daemon has stopped");
    let out = rig.run_with_timeout(
        rig.command(&["events", &name, "--wait", "session_exit", "--json"]),
        None,
        Duration::from_secs(5),
    );
    let event = exit_event(&out, &name, 41);
    assert_eq!(event["ts"], metadata["exitedAt"]);
}

#[test]
fn events_wait_on_a_removed_session_returns_not_found_without_waiting() {
    let rig = Rig::new();
    let name = unique_id("pw");
    rig.daemon(&name, &["cat"], DaemonOpts::no_display_name());
    expect_status(&rig.pty(&["rm", &name]), 0);
    rig.wait_for_gone(&name);
    let out = rig.run_with_timeout(
        rig.command(&["events", &name, "--wait", "session_exit"]),
        None,
        Duration::from_secs(5),
    );
    expect_status(&out, 1);
    assert!(!out.timed_out, "removed session wait hung: {}", out.summary());
    expect_contains(&out.stderr(), &name);
    expect_contains(&out.stderr(), "not found");
}

#[test]
fn events_wait_on_a_never_started_session_returns_not_found_without_waiting() {
    let rig = Rig::new();
    let name = unique_id("pw");
    let out = rig.run_with_timeout(
        rig.command(&["events", &name, "--wait", "session_exit"]),
        None,
        Duration::from_secs(5),
    );
    expect_status(&out, 1);
    assert!(!out.timed_out, "nonexistent session wait hung: {}", out.summary());
    expect_contains(&out.stderr(), &name);
    expect_contains(&out.stderr(), "not found");
}

#[test]
fn events_wait_on_metadata_without_a_started_daemon_returns_vanished_without_waiting() {
    let rig = Rig::new();
    let name = unique_id("pw");
    write_fake_metadata(rig.root(), &name, FakeMeta::created(0));
    let out = rig.run_with_timeout(
        rig.command(&["events", &name, "--wait", "session_exit"]),
        None,
        Duration::from_secs(5),
    );
    expect_status(&out, 1);
    assert!(!out.timed_out, "vanished session wait hung: {}", out.summary());
    expect_contains(&out.stderr(), &name);
    expect_contains(&out.stderr(), "vanished");
}

/// `--timeout inf` used to end the command with a Rust panic. It is an
/// ordinary thing to type and `f64::from_str` accepts it, so it reached
/// `Duration::from_secs_f64`, which panics on a value that is not finite.
/// `--timeout NaN` quietly turned the bounded wait into an unbounded one.
///
/// Both now wait, which is what the Node tool does with the same input.
#[test]
fn a_timeout_that_is_not_a_finite_number_waits_instead_of_crashing() {
    let rig = Rig::new();
    rig.daemon("pw-inf", &["cat"], DaemonOpts::no_display_name());
    for value in ["inf", "Infinity", "NaN", "-1"] {
        let mut child = rig
            .command(&["peek", "--wait", "NEVERMATCH", "--timeout", value, "pw-inf"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn pty peek --wait");
        // Long enough that a crash would already have happened, short
        // enough to keep the suite quick.
        std::thread::sleep(Duration::from_millis(600));
        let finished = child.try_wait().expect("try_wait");
        let _ = child.kill();
        let out = child.wait_with_output().expect("wait");
        let said = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            finished.is_none(),
            "--timeout {value} ended early with {finished:?}: {said}"
        );
        assert!(
            !said.contains("panicked"),
            "--timeout {value} panicked: {said}"
        );
    }
}
