//! The shared parity fixtures `tests/fixtures/parity/screens.json`, run
//! against a `TerminalHandle` that spawned the fixture's child: plain-screen bytes
//! asserted exactly, with viewport semantics (Node's `getPlainScreen`).
//!
//! node: tests/parity-node-reference.test.ts:132-215

use std::path::PathBuf;
use std::time::Duration;

use pty::{SpawnOptions, TerminalHandle};
use pty_terminal::Range;

fn fixtures() -> serde_json::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/parity/screens.json");
    let text = std::fs::read_to_string(&path).expect("read screens.json");
    serde_json::from_str(&text).expect("parse screens.json")
}

fn fixture(id: &str) -> serde_json::Value {
    fixtures()["fixtures"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == id)
        .unwrap_or_else(|| panic!("fixture {id}"))
        .clone()
}

fn spawn_fixture(f: &serde_json::Value) -> TerminalHandle {
    let spawn = &f["spawn"];
    let args: Vec<&str> = spawn["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap()).collect();
    TerminalHandle::spawn(
        spawn["command"].as_str().unwrap(),
        &args,
        SpawnOptions {
            rows: spawn["rows"].as_u64().unwrap() as u16,
            cols: spawn["cols"].as_u64().unwrap() as u16,
            scrollback: 10_000,
            ..Default::default()
        },
    )
    .expect("spawn")
}

fn settle(f: &serde_json::Value) -> Duration {
    Duration::from_millis(f["settleMs"].as_u64().unwrap_or(500))
}

/// A written trailing space (the cursor cell) survives; never-written cells
/// are trimmed; nothing is padded.
#[test]
fn idle_prompt_plain() {
    let f = fixture("idle-prompt-plain");
    let h = spawn_fixture(&f);
    std::thread::sleep(settle(&f));
    let plain = h.plain(Range::Viewport);
    assert_eq!(plain, f["expect"]["plainScreen"].as_str().unwrap());
    assert_eq!(plain.len() as u64, f["expect"]["plainScreenLength"].as_u64().unwrap());
    h.kill();
}

/// The final viewport survives the child's exit verbatim and reading it is
/// idempotent; the exit code is the real one.
#[test]
fn post_exit_final_screen() {
    let f = fixture("post-exit-final-screen");
    let h = spawn_fixture(&f);
    std::thread::sleep(settle(&f));
    assert!(h.exited(), "child should have exited");
    assert_eq!(h.exit_code(), Some(f["expect"]["exitCode"].as_i64().unwrap() as i32));
    let expected = f["expect"]["plainScreen"].as_str().unwrap();
    assert_eq!(h.plain(Range::Viewport), expected);
    assert_eq!(h.plain(Range::Viewport), expected, "peeking must not consume the screen");
    assert_eq!(h.plain(Range::Full), expected);
    h.kill();
}

/// The terminal side of the reap fixture: the child ran, printed, and exited
/// cleanly. Removing the registry entry is the daemon's job, not the
/// terminal's.
#[test]
fn post_exit_reaped_child_ran() {
    let f = fixture("post-exit-reaped");
    let h = spawn_fixture(&f);
    std::thread::sleep(settle(&f));
    assert_eq!(h.exit_code(), Some(0));
    assert_eq!(h.plain(Range::Viewport), "GONE");
    h.kill();
}
