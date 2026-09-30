//! The session manager, driven through a real terminal.
//!
//! `pty` with no arguments opens the picker. These run it inside the
//! libghostty harness, so what is asserted is what a person would see.
//!
//! node: tests/tui.test.ts

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

use pty_testkit::{Session, SpawnOptions};

fn pty_bin() -> &'static str {
    env!("CARGO_BIN_EXE_pty")
}

/// These each run a real picker over a real registry; one at a time.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn unique_root() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("pty-tui-{}-{}", std::process::id(), n));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_pty(root: &PathBuf, args: &[&str]) {
    let out = Command::new(pty_bin())
        .args(args)
        .env("PTY_ROOT", root)
        .env_remove("PTY_SESSION")
        .output()
        .expect("spawn pty");
    assert!(
        out.status.success(),
        "pty {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The picker, with `--force` because the suite itself may run inside a
/// session and the nesting guard is doing its job.
fn open_picker(root: &PathBuf) -> Session {
    Session::spawn(
        pty_bin(),
        &["--force"],
        SpawnOptions {
            rows: Some(24),
            cols: Some(100),
            env: vec![
                ("PTY_ROOT".to_string(), root.to_string_lossy().into_owned()),
                // Keep the picker off the theme a previous test saved.
                ("HOME".to_string(), root.to_string_lossy().into_owned()),
            ],
            ..Default::default()
        },
    )
    .expect("spawn the picker")
}

#[test]
fn it_lists_a_session_and_offers_to_create_one() {
    let _serial = serial();
    let root = unique_root();
    run_pty(&root, &["run", "-d", "--id", "alpha", "--name", "Alpha", "--", "cat"]);

    let mut s = open_picker(&root);
    s.wait_for_text("Alpha (alpha)", 8000).expect("the session is listed");
    s.wait_for_text("+ Create new session...", 8000)
        .expect("the create row");
    s.wait_for_text("Filter:", 8000).expect("the filter line");
    s.wait_for_text("q quit", 8000).expect("the footer");

    s.type_str("q");
    s.close();
    run_pty(&root, &["kill", "alpha"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn typing_filters_the_list_and_escape_clears_it() {
    let _serial = serial();
    let root = unique_root();
    run_pty(&root, &["run", "-d", "--id", "alpha", "--no-display-name", "--", "cat"]);
    run_pty(&root, &["run", "-d", "--id", "bravo", "--no-display-name", "--", "cat"]);

    let mut s = open_picker(&root);
    s.wait_for_text("alpha", 8000).expect("alpha listed");
    s.wait_for_text("bravo", 8000).expect("bravo listed");

    s.type_str("brav");
    s.wait_for_absent("alpha", 8000).expect("alpha filtered out");
    s.wait_for_text("bravo", 8000).expect("bravo still there");

    // Escape clears the filter rather than quitting.
    s.type_str("\x1b");
    s.wait_for_text("alpha", 8000).expect("alpha back after escape");

    s.type_str("q");
    s.close();
    for id in ["alpha", "bravo"] {
        run_pty(&root, &["kill", id]);
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// The filter takes the readline editing keys every other text field does.
///
/// node: `src/tui/interactive.ts` delegates filter keys to `applyTextKey`
#[test]
fn the_filter_takes_readline_editing_keys() {
    let _serial = serial();
    let root = unique_root();
    run_pty(&root, &["run", "-d", "--id", "alpha", "--no-display-name", "--", "cat"]);
    run_pty(&root, &["run", "-d", "--id", "bravo", "--no-display-name", "--", "cat"]);

    let mut s = open_picker(&root);
    s.wait_for_text("bravo", 8000).expect("bravo listed");

    s.type_str("x alp");
    s.wait_for_text("Filter: x alp", 8000).expect("typed filter");
    s.press("ctrl+w").expect("ctrl+w");
    // Only the filter line: session rows can match "alp" on their own.
    s.wait_for(
        |ss| {
            ss.text
                .lines()
                .find(|l| l.contains("Filter:"))
                .is_some_and(|l| l.trim_end_matches([' ', '│']).ends_with("Filter: x"))
        },
        8000,
        "ctrl+w to delete the word behind the cursor",
    )
    .expect("ctrl+w deletes the word behind");

    s.press("ctrl+u").expect("ctrl+u");
    s.wait_for_text("type to filter", 8000).expect("ctrl+u clears to the start");
    s.wait_for_text("alpha", 8000).expect("alpha back once the filter is empty");
    s.wait_for_text("bravo", 8000).expect("bravo back once the filter is empty");

    s.type_str("q");
    s.close();
    for id in ["alpha", "bravo"] {
        run_pty(&root, &["kill", id]);
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn q_quits_only_when_the_filter_is_empty() {
    let _serial = serial();
    let root = unique_root();
    run_pty(&root, &["run", "-d", "--id", "quiet", "--no-display-name", "--", "cat"]);

    let mut s = open_picker(&root);
    s.wait_for_text("quiet", 8000).expect("listed");

    // A filter has to start with something other than `q`, because `q` on an
    // empty filter is the quit key. Once there is a filter, `q` is text.
    s.type_str("u");
    s.wait_for_text("quiet", 8000).expect("u still matches quiet");
    s.type_str("q");
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!s.has_exited(), "the picker quit on a q that should have been text");

    // Clear the filter, and then `q` really does quit. The pause is for the
    // terminal, not the picker: an escape byte followed straight away by a
    // letter is how a terminal spells alt+letter, so a person's escape and a
    // person's `q` have to arrive as two keys.
    s.type_str("\x1b");
    std::thread::sleep(std::time::Duration::from_millis(300));
    s.type_str("q");
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(8) && !s.has_exited() {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(s.has_exited(), "the picker did not quit on q with an empty filter");

    s.close();
    run_pty(&root, &["kill", "quiet"]);
    let _ = std::fs::remove_dir_all(&root);
}

/// The point of the picker: return attaches, and detaching comes back to the
/// list rather than dropping you at a shell.
#[test]
fn return_attaches_and_detach_comes_back_to_the_list() {
    let _serial = serial();
    let root = unique_root();
    run_pty(
        &root,
        &["run", "-d", "--id", "shellish", "--no-display-name", "--", "sh", "-c", "printf 'INSIDE-THE-SESSION\\n'; exec cat"],
    );

    let mut s = open_picker(&root);
    s.wait_for_text("shellish", 8000).expect("listed");

    // Return attaches to the selected session.
    s.type_str("\r");
    s.wait_for_text("INSIDE-THE-SESSION", 8000)
        .expect("attached to the session");

    // Ctrl+\ detaches, and the picker is there again.
    s.type_str("\x1c");
    s.wait_for_text("+ Create new session...", 8000)
        .expect("back at the list after detaching");
    s.wait_for_text("shellish", 8000).expect("the session is still listed");

    s.type_str("q");
    s.close();
    run_pty(&root, &["kill", "shellish"]);
    let _ = std::fs::remove_dir_all(&root);
}
