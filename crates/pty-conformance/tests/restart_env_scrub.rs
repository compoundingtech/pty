//! Port of tests/restart-env-scrub.test.ts: `restart` does not leak the
//! restarter's legacy or Fractal identity or `AGENT_TRACE_FILE` into the
//! re-launched child, while a fresh `run` still inherits the creator's.

use pty_conformance::*;
use std::path::Path;
use std::time::Duration;

const OUTER: (&str, &str) = ("PTY_SESSION", "outer");

/// A command that records the identity and trace path it was launched with, then
/// stays alive (`UNSET` when absent).
fn recorder(out: &Path) -> String {
    format!(
        "printf '%s|%s|%s|%s|%s' \"${{ST_AGENT-UNSET}}\" \"${{ST_ROOT-UNSET}}\" \"${{FRACTAL_AGENT-UNSET}}\" \"${{FRACTAL_ROOT-UNSET}}\" \"${{AGENT_TRACE_FILE-UNSET}}\" > '{}'; exec sleep 300",
        out.display()
    )
}

fn wait_for_content(file: &Path) -> String {
    let _ = poll_for(Duration::from_secs(4), || {
        std::fs::read_to_string(file).map(|s| !s.is_empty()).unwrap_or(false)
    });
    std::fs::read_to_string(file).unwrap_or_default()
}

/// node: tests/restart-env-scrub.test.ts:60
#[test]
fn restart_does_not_leak_the_restarters_identity() {
    let rig = Rig::new();
    let out = rig.root().join("child.env");
    let script = recorder(&out);
    // The rig scrubs both identity namespaces; supply the original trace path.
    let created = rig.pty_env(
        &[OUTER, ("AGENT_TRACE_FILE", "/creator/session.trace")],
        &["run", "-d", "--id", "s", "--", "sh", "-c", &script],
    );
    expect_status(&created, 0);
    assert_eq!(wait_for_content(&out), "UNSET|UNSET|UNSET|UNSET|/creator/session.trace");

    let _ = std::fs::remove_file(&out);
    let r = rig.pty_env(
        &[
            OUTER,
            ("ST_AGENT", "smalltalk-claude"),
            ("ST_ROOT", "/leaked/convoy"),
            ("FRACTAL_AGENT", "fractal-claude"),
            ("FRACTAL_ROOT", "/leaked/fractal"),
            ("AGENT_TRACE_FILE", "/leaked/session.trace"),
        ],
        &["restart", "-y", "s"],
    );
    expect_status(&r, 0);
    expect_contains(&r.stdout(), "restarted");
    let recorded = wait_for_content(&out);
    assert_eq!(recorded, "UNSET|UNSET|UNSET|UNSET|UNSET");
}

/// node: tests/restart-env-scrub.test.ts:83
#[test]
fn fresh_run_inherits_the_creators_identity() {
    let rig = Rig::new();
    let out = rig.root().join("child.env");
    let script = recorder(&out);
    let created = rig.pty_env(
        &[
            OUTER,
            ("ST_AGENT", "creator-abc"),
            ("ST_ROOT", "/creator/convoy"),
            ("FRACTAL_AGENT", "fractal-creator"),
            ("FRACTAL_ROOT", "/creator/fractal"),
            ("AGENT_TRACE_FILE", "/creator/session.trace"),
        ],
        &["run", "-d", "--id", "fresh", "--", "sh", "-c", &script],
    );
    expect_status(&created, 0);
    assert_eq!(
        wait_for_content(&out),
        "creator-abc|/creator/convoy|fractal-creator|/creator/fractal|/creator/session.trace"
    );
}

#[test]
fn restart_preserves_explicit_identity_in_both_namespaces() {
    let rig = Rig::new();
    let out = rig.root().join("child.env");
    let script = recorder(&out);
    let created = rig.pty_env(
        &[OUTER],
        &[
            "run", "-d", "--id", "explicit", "--env", "ST_AGENT=managed-legacy", "--env",
            "ST_ROOT=/managed/legacy", "--env", "FRACTAL_AGENT=managed-fractal", "--env",
            "FRACTAL_ROOT=/managed/fractal", "--env", "AGENT_TRACE_FILE=/managed/session.trace",
            "--", "sh", "-c", &script,
        ],
    );
    expect_status(&created, 0);
    let expected =
        "managed-legacy|/managed/legacy|managed-fractal|/managed/fractal|/managed/session.trace";
    assert_eq!(wait_for_content(&out), expected);

    let _ = std::fs::remove_file(&out);
    let restarted = rig.pty_env(
        &[
            OUTER,
            ("ST_AGENT", "ambient-legacy"),
            ("ST_ROOT", "/ambient/legacy"),
            ("FRACTAL_AGENT", "ambient-fractal"),
            ("FRACTAL_ROOT", "/ambient/fractal"),
            ("AGENT_TRACE_FILE", "/ambient/session.trace"),
        ],
        &["restart", "-y", "explicit"],
    );
    expect_status(&restarted, 0);
    assert_eq!(wait_for_content(&out), expected);
}
