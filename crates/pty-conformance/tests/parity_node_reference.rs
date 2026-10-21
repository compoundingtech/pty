//! Port of tests/parity-node-reference.test.ts: the reference behaviors
//! pinned for the Rust port — post-exit peek under preserve and reap modes,
//! the plain-peek trailing cursor cell, and plain `run --force` creating a
//! nested session.
//!
//! Left out: the `resolveSeqDelayMs` rounding cases (lines 238, 243) — a
//! pty-core unit test; the end-to-end pacing is in seq_delay.rs.

use pty_conformance::*;
use std::time::Duration;

fn preserve() -> DaemonOpts {
    DaemonOpts::no_display_name().invoke_env("PTY_REAP_ON_EXIT", "false")
}

/// node: tests/parity-node-reference.test.ts:132
#[test]
fn preserve_mode_peek_returns_the_exact_final_viewport_idempotently() {
    let rig = Rig::new();
    let id = "par1";
    rig.daemon(id, &["sh", "-c", "printf \"LINE_A\\nLINE_B\\nDONE\"; exit 7"], preserve());
    rig.wait_for_exit(id);
    std::thread::sleep(Duration::from_millis(600));
    let first = rig.pty(&["peek", "--plain", id]);
    expect_status(&first, 0);
    assert_eq!(first.stdout().trim_end_matches('\n'), "LINE_A\nLINE_B\nDONE");
    let second = rig.pty(&["peek", "--plain", id]);
    expect_status(&second, 0);
    assert_eq!(second.stdout, first.stdout);
    let found = rig.list_entry(id).expect("listed");
    assert_eq!(found["status"], "exited");
    assert_eq!(found["exitCode"], 7);
}

/// node: tests/parity-node-reference.test.ts:159
#[test]
fn preserve_mode_ansi_peek_keeps_the_content() {
    let rig = Rig::new();
    let id = "par2";
    rig.daemon(id, &["sh", "-c", "printf \"ALPHA\\nBETA\"; exit 0"], preserve());
    rig.wait_for_exit(id);
    std::thread::sleep(Duration::from_millis(600));
    let ansi = rig.pty(&["peek", id]);
    expect_status(&ansi, 0);
    expect_contains(&ansi.stdout(), "ALPHA");
    expect_contains(&ansi.stdout(), "BETA");
}

/// node: tests/parity-node-reference.test.ts:171
#[test]
fn reap_mode_session_is_gone_after_exit() {
    let rig = Rig::new();
    let id = "par3";
    rig.daemon(id, &["sh", "-c", "printf \"GONE\"; exit 0"], DaemonOpts::no_display_name());
    rig.wait_for_gone(id);
    std::thread::sleep(Duration::from_millis(300));
    let peek = rig.pty(&["peek", "--plain", id]);
    expect_failure(&peek);
    assert!(rig.list_entry(id).is_none());
}

/// node: tests/parity-node-reference.test.ts:198
#[test]
fn plain_peek_keeps_the_trailing_cursor_cell_blank() {
    let rig = Rig::new();
    let id = "par4";
    rig.daemon(id, &["sh", "-c", "printf 'READY> '; exec cat"], DaemonOpts::no_display_name());
    std::thread::sleep(Duration::from_millis(700));
    let out = rig.pty(&["peek", "--plain", id]);
    expect_status(&out, 0);
    let line = out.stdout().trim_end_matches('\n').to_string();
    assert_eq!(line, "READY> ");
    assert_ne!(line, "READY>");
    assert_eq!(line.len(), 7);
}

/// node: tests/parity-node-reference.test.ts:275
#[test]
fn plain_run_force_creates_a_nested_session() {
    let rig = Rig::new();
    let id = "par5";
    let mut cmd = rig.command(&["run", "--force", "--id", id, "--", "cat"]);
    cmd.env("PTY_SESSION", "outer-session")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().unwrap();
    let mut found = None;
    // The socket is bound before the metadata is published, so for a moment
    // the row reads running with an unknown identity and no pid. Wait for the
    // published row; one that never gains its pid still fails below.
    let _ = poll_for(Duration::from_secs(8), || {
        found = rig.list_entry(id);
        found
            .as_ref()
            .is_some_and(|f| f["status"] == "running" && f["pid"].is_number())
    });
    let found = found.expect("a nested session was created");
    assert_eq!(found["status"], "running");
    assert!(found["pid"].is_number(), "{found}");
    let _ = child.kill();
    let _ = child.wait();
    let _ = rig.pty(&["kill", id]);
}
