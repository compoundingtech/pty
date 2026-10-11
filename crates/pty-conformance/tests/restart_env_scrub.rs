//! Port of tests/restart-env-scrub.test.ts: `restart` does not leak the
//! restarter's `ST_AGENT`/`ST_ROOT`/`AGENT_TRACE_FILE` into the re-launched child, while a
//! fresh `run` still inherits the creator's.

use pty_conformance::*;
use std::path::Path;
use std::time::Duration;

const OUTER: (&str, &str) = ("PTY_SESSION", "outer");

/// A command that records the identity and trace path it was launched with, then
/// stays alive (`UNSET` when absent).
fn recorder(out: &Path) -> String {
    format!(
        "printf '%s|%s|%s' \"${{ST_AGENT-UNSET}}\" \"${{ST_ROOT-UNSET}}\" \"${{AGENT_TRACE_FILE-UNSET}}\" > '{}'; exec sleep 300",
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
    // The rig scrubs ST_AGENT/ST_ROOT; supply a trace path for the original child.
    let created = rig.pty_env(
        &[OUTER, ("AGENT_TRACE_FILE", "/creator/session.trace")],
        &["run", "-d", "--id", "s", "--", "sh", "-c", &script],
    );
    expect_status(&created, 0);
    assert_eq!(wait_for_content(&out), "UNSET|UNSET|/creator/session.trace");

    let _ = std::fs::remove_file(&out);
    let r = rig.pty_env(
        &[
            OUTER,
            ("ST_AGENT", "smalltalk-claude"),
            ("ST_ROOT", "/leaked/convoy"),
            ("AGENT_TRACE_FILE", "/leaked/session.trace"),
        ],
        &["restart", "-y", "s"],
    );
    expect_status(&r, 0);
    expect_contains(&r.stdout(), "restarted");
    let recorded = wait_for_content(&out);
    assert_eq!(recorded, "UNSET|UNSET|UNSET");
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
            ("AGENT_TRACE_FILE", "/creator/session.trace"),
        ],
        &["run", "-d", "--id", "fresh", "--", "sh", "-c", &script],
    );
    expect_status(&created, 0);
    assert_eq!(wait_for_content(&out), "creator-abc|/creator/convoy|/creator/session.trace");
}
