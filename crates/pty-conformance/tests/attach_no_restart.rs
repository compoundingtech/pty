//! Port of tests/attach-no-restart.test.ts: the attach-only restart policy.
//! `pty attach --no-restart` never prompts, never re-launches a retained or
//! vanished session, and still follows a running session to its exit; the
//! default policy prompts `Restart? [Y/n]` and `--auto-restart` re-launches
//! without asking. The terminal cases run the CLI inside a real pty
//! (`Rig::pty_tty_raw`) with the same 30×100 geometry the Node test uses.

use pty_conformance::*;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

fn invocation_count(marker: &Path) -> usize {
    std::fs::read_to_string(marker)
        .map(|s| s.lines().filter(|l| !l.is_empty()).count())
        .unwrap_or(0)
}

fn event_count(rig: &Rig, id: &str, kind: &str) -> usize {
    std::fs::read_to_string(rig.root().join(format!("{id}.events.jsonl")))
        .map(|s| {
            s.lines()
                .filter(|l| !l.is_empty())
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .filter(|e| e["type"] == kind)
                .count()
        })
        .unwrap_or(0)
}

/// `run -d --id <id> --tag keep=true -- sh -c "printf 'started\n' >> marker; exit 42"`,
/// then wait for the exit record and for the daemon to be fully gone.
fn spawn_retained_once(rig: &Rig, id: &str, marker: &Path) {
    let script = format!("printf 'started\\n' >> '{}'; exit 42", marker.display());
    let d = rig.daemon(id, &["sh", "-c", &script], DaemonOpts::keep());
    wait_until("exit metadata", || {
        rig.meta(id)
            .map(|m| m["exitCode"] == 42 && m["exitedAt"].is_string())
            .unwrap_or(false)
    });
    let pid = d.pid();
    wait_until("retained daemon to exit", || !pid_alive(pid));
    assert_eq!(invocation_count(marker), 1);
}

/// node: tests/attach-no-restart.test.ts:134
#[test]
fn help_advertises_the_attach_only_policy() {
    let rig = Rig::new();
    let out = rig.pty(&["attach", "--help"]);
    expect_status(&out, 0);
    expect_contains(&out.stdout(), "--no-restart");
    expect_regex(&out.stdout(), "never prompt");
}

/// node: tests/attach-no-restart.test.ts:142
#[test]
fn contradictory_restart_policies_are_rejected() {
    let rig = Rig::new();
    let out = rig.pty(&["attach", "--no-restart", "--auto-restart", "missing"]);
    expect_failure(&out);
    expect_regex(&out.stderr(), "mutually exclusive");
}

/// node: tests/attach-no-restart.test.ts:149
#[test]
fn missing_session_fails_without_prompting() {
    let rig = Rig::new();
    let out = rig.pty(&["attach", "--no-restart", "missing"]);
    expect_failure(&out);
    expect_contains(&out.stderr(), "Session \"missing\" not found.");
    expect_not_contains(&out.stdout(), "Restart?");
}

/// node: tests/attach-no-restart.test.ts:157
#[test]
fn exited_session_is_refused_before_delayed_input_can_restart_it() {
    let rig = Rig::new();
    let marker = rig.tmp().join("invocations");
    let id = "exited-target";
    spawn_retained_once(&rig, id, &marker);

    let mut t = rig.pty_tty_raw(&[], &[], &["attach", "--no-restart", id], 30, 100);
    // Wait for the refusal, then type. A sleep here is a guess about how long
    // the command takes to decide, and this binary lost that guess twice on a
    // slower machine — a different test inside it each time, which is what a
    // load-sensitive wait looks like rather than a flaky assertion. The point
    // of the test is that input arriving AFTER the refusal cannot resurrect
    // anything, so waiting for the refusal is the thing it meant to do.
    assert!(
        t.wait_for_text("is not running", deadline()),
        "no refusal before the input was sent: {}",
        t.output_str()
    );
    t.write(b"future-relay-input\r");
    let code = t.wait_exit(Duration::from_secs(10)).expect("attach exits");
    let output = t.output_str();
    assert_ne!(code, 0, "{output}");
    expect_not_contains(&output, "Restart?");
    expect_not_contains(&output, "Command was:");
    assert_eq!(invocation_count(&marker), 1);
    assert_eq!(event_count(&rig, id, "session_start"), 1);
    if let Some(pid) = rig.pid(id) {
        assert!(process_exited(pid), "retained daemon {pid} came back");
    }
}

/// node: tests/attach-no-restart.test.ts:181
#[test]
fn vanished_session_is_refused_without_evaluating_its_command() {
    let rig = Rig::new();
    let marker = rig.tmp().join("must-not-exist");
    let id = "vanished-target";
    let restart = format!("printf 'restarted\\n' >> '{}'", marker.display());
    write_fake_metadata(
        rig.root(),
        id,
        FakeMeta {
            command: Some("sh".into()),
            ..FakeMeta::created(0)
        }
            .tag("keep", "true")
            .extra("args", serde_json::json!(["-c", restart]))
            .extra("displayCommand", Value::String("synthetic stored command".into()))
            .extra("cwd", Value::String(rig.root().to_string_lossy().into_owned())),
    );

    let mut t = rig.pty_tty_raw(&[], &[], &["attach", "--no-restart", id], 30, 100);
    // Wait for the refusal, then type. A sleep here is a guess about how long
    // the command takes to decide, and this binary lost that guess twice on a
    // slower machine — a different test inside it each time, which is what a
    // load-sensitive wait looks like rather than a flaky assertion. The point
    // of the test is that input arriving AFTER the refusal cannot resurrect
    // anything, so waiting for the refusal is the thing it meant to do.
    assert!(
        t.wait_for_text("is not running", deadline()),
        "no refusal before the input was sent: {}",
        t.output_str()
    );
    t.write(b"future-relay-input\r");
    let code = t.wait_exit(Duration::from_secs(10)).expect("attach exits");
    let output = t.output_str();
    assert_ne!(code, 0, "{output}");
    // The status in its own place. The session id here is
    // "vanished-target", so a check for the bare word matched the id and
    // said nothing about the status the command reported.
    expect_contains(&output, &format!("Session \"{id}\" is not running (status: vanished)."));
    expect_not_contains(&output, "Restart?");
    expect_not_contains(&output, "synthetic stored command");
    assert!(!marker.exists(), "the stored command ran");
}

/// node: tests/attach-no-restart.test.ts:208
#[test]
fn running_session_is_followed_to_its_exit_without_a_second_incarnation() {
    let rig = Rig::new();
    let marker = rig.tmp().join("invocations");
    let id = "running-target";
    let script = format!(
        "printf 'started\\n' >> '{}'; printf 'ATTACH_READY\\n'; read line; exit 37",
        marker.display()
    );
    rig.daemon(id, &["sh", "-c", &script], DaemonOpts::keep());
    wait_until("first invocation", || invocation_count(&marker) == 1);

    let mut t = rig.pty_tty_raw(&[], &[], &["attach", "--no-restart", id], 30, 100);
    assert!(t.wait_for_text("ATTACH_READY", Duration::from_secs(8)), "{}", t.output_str());
    t.write(b"finish\r");
    let code = t.wait_exit(Duration::from_secs(8)).expect("attach exits with the session");
    let output = t.output_str();
    assert_eq!(code, 37, "{output}");
    expect_contains(&output, &format!("{id} exited with code 37"));

    t.write(b"future-relay-input\r");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(invocation_count(&marker), 1);
    assert_eq!(event_count(&rig, id, "session_start"), 1);
}

/// node: tests/attach-no-restart.test.ts:257
#[test]
fn default_policy_prompts_and_restarts() {
    let rig = Rig::new();
    let marker = rig.tmp().join("invocations");
    let id = "legacy-target";
    spawn_retained_once(&rig, id, &marker);

    let mut t = rig.pty_tty_raw(&[], &[], &["attach", id], 30, 100);
    std::thread::sleep(Duration::from_millis(250));
    t.write(b"future-relay-input\r");
    let _ = t.wait_exit(Duration::from_secs(10));
    let output = t.output_str();
    expect_contains(&output, "Restart? [Y/n]");
    wait_until("second invocation", || invocation_count(&marker) == 2);
    assert_eq!(invocation_count(&marker), 2);
    // A restart begins a new event log rather than appending to the old one.
    assert_eq!(event_count(&rig, id, "session_start"), 1);
}

/// node: tests/attach-no-restart.test.ts:275
#[test]
fn auto_restart_relaunches_without_prompting() {
    let rig = Rig::new();
    let marker = rig.tmp().join("invocations");
    let id = "automatic-target";
    spawn_retained_once(&rig, id, &marker);

    let mut t = rig.pty_tty_raw(&[], &[], &["attach", "--auto-restart", id], 30, 100);
    let _ = t.wait_exit(Duration::from_secs(10));
    let output = t.output_str();
    expect_not_contains(&output, "Restart?");
    wait_until("second invocation", || invocation_count(&marker) == 2);
    assert_eq!(invocation_count(&marker), 2);
    assert_eq!(event_count(&rig, id, "session_start"), 1);
}
