//! Port of tests/atomic-writes.test.ts: the halves that are observable
//! through the binary. Concurrent `pty tag` writers never corrupt
//! `<id>.json` and leave no `*.tmp.*` behind; concurrent `pty emit` calls
//! plus a retried `pty metadata patch` all land in the event log, including
//! a `metadata_change` record over 4 KiB; a stale `<id>.events.lock` is
//! reclaimed by `pty rm`; a live `<id>.lock` or `<id>.events.lock` makes
//! `pty rm` refuse and touch nothing; a held event lock makes the one-shot
//! CLI writers fail with `event log is busy`; a reader hammering the event
//! log during CLI-driven truncation never sees a torn line.
//!
//! Left out: the `atomicWriteFileSync` / `atomicWriteFile` loops (:96, :132,
//! :165 — library helpers; the no-leftover-tmp postcondition is asserted in
//! the CLI racer tests instead) and `appendEvent` queueing behind a held lock
//! (:248 — the async library writer; the CLI writers are synchronous and
//! fail fast, which :270 pins).

use pty_conformance::*;
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;

fn entries(rig: &Rig) -> Vec<String> {
    std::fs::read_dir(rig.root())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

fn tmp_leftovers(rig: &Rig) -> Vec<String> {
    entries(rig).into_iter().filter(|e| e.contains(".tmp.")).collect()
}

/// Spawn `n` CLI invocations at once (args built per index) and wait for
/// all of them; returns their exit statuses.
fn race(rig: &Rig, n: usize, args: impl Fn(usize) -> Vec<String>) -> Vec<i32> {
    let mut children = Vec::new();
    for i in 0..n {
        let a = args(i);
        let refs: Vec<&str> = a.iter().map(String::as_str).collect();
        let mut cmd = rig.command(&refs);
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        children.push(cmd.spawn().expect("spawn pty"));
    }
    children
        .iter_mut()
        .map(|c| c.wait().expect("wait").code().unwrap_or(-1))
        .collect()
}

fn read_events(rig: &Rig, id: &str) -> Vec<Value> {
    let raw = std::fs::read_to_string(rig.root().join(format!("{id}.events.jsonl"))).unwrap_or_default();
    raw.lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad event line ({e}): {l}")))
        .collect()
}

/// node: tests/atomic-writes.test.ts:178
#[test]
fn ten_racing_taggers_leave_valid_json_and_no_tmp_files() {
    let rig = Rig::new();
    rig.daemon("at1", &["cat"], DaemonOpts::no_display_name());
    race(&rig, 10, |i| vec!["tag".into(), "at1".into(), format!("k{i}={i}")]);
    let content = std::fs::read_to_string(rig.meta_path("at1")).unwrap();
    let parsed: Value = serde_json::from_str(&content).unwrap_or_else(|e| panic!("metadata is not JSON ({e}): {content}"));
    assert!(parsed.is_object());
    assert!(tmp_leftovers(&rig).is_empty(), "{:?}", tmp_leftovers(&rig));
}

/// node: tests/atomic-writes.test.ts:413
#[test]
fn twenty_racing_taggers_leave_valid_json_with_some_updates() {
    let rig = Rig::new();
    rig.daemon("at2", &["cat"], DaemonOpts::no_display_name());
    race(&rig, 20, |i| vec!["tag".into(), "at2".into(), format!("race{i}={i}")]);
    let content = std::fs::read_to_string(rig.meta_path("at2")).unwrap();
    let parsed: Value = serde_json::from_str(&content).unwrap_or_else(|e| panic!("metadata is not JSON ({e}): {content}"));
    assert!(parsed.is_object());
    let race_keys = parsed["tags"]
        .as_object()
        .map(|t| t.keys().filter(|k| k.starts_with("race")).count())
        .unwrap_or(0);
    assert!(race_keys > 0, "no tag update landed: {content}");
    assert!(tmp_leftovers(&rig).is_empty(), "{:?}", tmp_leftovers(&rig));
}

/// node: tests/atomic-writes.test.ts:209
#[test]
fn rm_cannot_clean_through_a_live_metadata_lock_holder() {
    let rig = Rig::new();
    let daemon = rig.daemon("at3", &["true"], DaemonOpts::keep());
    let daemon_pid = daemon.pid();
    rig.wait_for_exit("at3");
    let meta = rig.meta_path("at3");
    let events = rig.root().join("at3.events.jsonl");
    // **Wait for the daemon, not for its socket.** The socket vanishing is a
    // precursor: the daemon unlinks it and then keeps going, flushing events
    // and touching the very lock file this test is about to write. Under load
    // that gap is wide enough that `pty rm` ran against a lock the daemon had
    // already cleared, removed the session, and the test failed asserting the
    // refusal.
    wait_for_process_gone(daemon_pid);
    let meta_before = std::fs::read(&meta).unwrap();
    let events_before = std::fs::read(&events).unwrap();
    let lock = rig.root().join("at3.lock");
    std::fs::write(&lock, std::process::id().to_string()).unwrap();

    let out = rig.pty(&["rm", "at3"]);
    expect_status(&out, 1);
    expect_contains(&out.stderr(), "not removed");
    assert_eq!(std::fs::read(&meta).unwrap(), meta_before, "metadata was touched");
    assert_eq!(std::fs::read(&events).unwrap(), events_before, "events were touched");
    assert_eq!(std::fs::read_to_string(&lock).unwrap(), std::process::id().to_string(), "lock was stolen");

    std::fs::remove_file(&lock).unwrap();
    let out = rig.pty(&["rm", "at3"]);
    expect_status(&out, 0);
    expect_contains(&out.stdout(), "removed");
    assert!(!meta.exists());
    assert!(!events.exists());
}

/// node: tests/atomic-writes.test.ts:235
#[test]
fn rm_reclaims_a_stale_event_lock_during_full_cleanup() {
    let rig = Rig::new();
    rig.daemon("at4", &["true"], DaemonOpts::keep());
    rig.wait_for_exit("at4");
    wait_until("daemon to release the socket", || !rig.socket_path("at4").exists());
    let events = rig.root().join("at4.events.jsonl");
    let events_lock = rig.root().join("at4.events.lock");
    assert!(events.exists());
    std::fs::write(&events_lock, "2147483647").unwrap();

    let out = rig.pty(&["rm", "at4"]);
    expect_status(&out, 0);
    assert!(!events.exists(), "events file survived cleanup");
    assert!(!events_lock.exists(), "stale event lock survived cleanup");
    assert!(!rig.meta_path("at4").exists());
}

/// node: tests/atomic-writes.test.ts:270
#[test]
fn held_event_lock_makes_cli_writers_fail_fast_with_busy() {
    let rig = Rig::new();
    rig.daemon("at5", &["cat"], DaemonOpts::no_display_name());
    let events_lock = rig.root().join("at5.events.lock");
    std::fs::write(&events_lock, std::process::id().to_string()).unwrap();
    let before = std::fs::read(rig.meta_path("at5")).unwrap();

    let out = rig.pty(&["tag", "at5", "a=1"]);
    expect_status(&out, 1);
    expect_regex(&out.stderr(), "(?i)event log is busy");
    let out = rig.pty(&["emit", "at5", "user.blocked"]);
    expect_status(&out, 1);
    expect_regex(&out.stderr(), "(?i)event log is busy");
    let out = rig.pty_stdin(br#"{"tags":{"z":"1"}}"#, &["metadata", "patch", "--id", "at5"]);
    expect_status(&out, 1);
    expect_regex(&out.stderr(), "(?i)event log is busy");
    let out = rig.pty(&["rm", "at5"]);
    expect_status(&out, 1);
    assert_eq!(std::fs::read(rig.meta_path("at5")).unwrap(), before, "metadata was touched");
    assert!(read_events(&rig, "at5").iter().all(|e| e["type"] != "user.blocked"));

    std::fs::remove_file(&events_lock).unwrap();
    expect_status(&rig.pty(&["tag", "at5", "a=1"]), 0);
    assert_eq!(rig.meta("at5").unwrap()["tags"]["a"], "1");
}

/// node: tests/atomic-writes.test.ts:283
#[test]
fn concurrent_emits_and_an_oversized_metadata_event_survive_retention() {
    let rig = Rig::new();
    rig.daemon("at6", &["cat"], DaemonOpts::no_display_name());
    let previous = "😀".repeat(1000);
    let next = "🫠".repeat(1000);
    expect_status(&rig.pty(&["tag", "at6", &format!("description={previous}")]), 0);

    let events_path = rig.root().join("at6.events.jsonl");
    let ts = iso_timestamp(0);
    let mut prime = String::new();
    for i in 0..999 {
        prime.push_str(&serde_json::json!({"session": "at6", "type": "user.prime", "ts": ts, "data": {"i": i}}).to_string());
        prime.push('\n');
    }
    std::fs::write(&events_path, prime).unwrap();

    let markers: Vec<String> = (0..16).map(|i| format!("marker-{i}")).collect();
    let mut children = Vec::new();
    for m in &markers {
        let json = serde_json::json!({"marker": m}).to_string();
        let mut cmd = rig.command(&["emit", "at6", "user.concurrent", "--json", &json]);
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        children.push(cmd.spawn().expect("spawn emit"));
    }
    let patch = serde_json::json!({"tags": {"description": next}}).to_string();
    let mut patched = false;
    let mut last_patch = None;
    for _ in 0..100 {
        let out = rig.pty_stdin(patch.as_bytes(), &["metadata", "patch", "--id", "at6"]);
        if out.status == 0 {
            patched = true;
            break;
        }
        last_patch = Some(out);
    }
    let results: Vec<_> = markers
        .iter()
        .zip(children)
        .map(|(marker, child)| {
            let out = child.wait_with_output().expect("wait for emit");
            (marker, out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
        })
        .collect();
    assert!(
        results.iter().all(|(_, status, _)| *status == 0),
        "emit results (marker, status, stderr): {results:#?}\nmetadata patched: {patched}; last failed patch: {last_patch:?}"
    );
    assert!(patched, "metadata patch remained busy after concurrent writers completed: {last_patch:?}");

    let events = read_events(&rig, "at6");
    let mut observed: Vec<String> = events
        .iter()
        .filter(|e| e["type"] == "user.concurrent")
        .map(|e| e["data"]["marker"].as_str().unwrap().to_string())
        .collect();
    observed.sort();
    let mut expected = markers.clone();
    expected.sort();
    assert_eq!(observed, expected);
    let meta_events: Vec<&Value> = events.iter().filter(|e| e["type"] == "metadata_change").collect();
    assert_eq!(meta_events.len(), 1, "{meta_events:?}");
    assert_eq!(meta_events[0]["previous"]["tags"]["description"], previous);
    assert_eq!(meta_events[0]["value"]["tags"]["description"], next);
    assert!(meta_events[0].to_string().len() > 4096);
}

/// node: tests/atomic-writes.test.ts:344
#[test]
fn reader_never_sees_a_half_written_event_log_during_truncation() {
    let rig = Rig::new();
    rig.daemon("at7", &["cat"], DaemonOpts::no_display_name());
    let events_path = rig.root().join("at7.events.jsonl");
    // Prime well past the 1000-line retention cap so every CLI append
    // triggers a truncating rewrite.
    let ts = iso_timestamp(0);
    let mut prime = String::new();
    for i in 0..1200 {
        prime.push_str(&serde_json::json!({"session": "at7", "type": "user.prime", "ts": ts, "data": {"i": i}}).to_string());
        prime.push('\n');
    }
    std::fs::write(&events_path, prime).unwrap();

    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader_done = done.clone();
    let reader_path = events_path.clone();
    let reader = std::thread::spawn(move || {
        let mut errors: Vec<String> = Vec::new();
        let mut reads = 0usize;
        while !reader_done.load(std::sync::atomic::Ordering::Relaxed) {
            if let Ok(content) = std::fs::read(&reader_path) {
                reads += 1;
                if !content.is_empty() && content.last() != Some(&b'\n') {
                    errors.push("file does not end in a newline".into());
                }
                for line in content.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
                    match serde_json::from_slice::<Value>(line) {
                        Ok(v) if v["type"].is_string() => {}
                        Ok(_) => errors.push("event without .type".into()),
                        Err(e) => errors.push(format!("unparseable line: {e}")),
                    }
                }
                if errors.len() > 5 {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        (errors, reads)
    });
    for i in 0..60 {
        let json = format!("{{\"i\":{i}}}");
        let out = rig.pty(&["emit", "at7", "user.more", "--json", &json]);
        expect_status(&out, 0);
    }
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let (errors, reads) = reader.join().unwrap();
    assert!(reads > 10, "reader barely ran ({reads} reads)");
    assert!(errors.is_empty(), "{errors:?}");

    let content = std::fs::read_to_string(&events_path).unwrap();
    let lines: Vec<&str> = content.trim_end().lines().filter(|l| !l.is_empty()).collect();
    for l in &lines {
        serde_json::from_str::<Value>(l).unwrap_or_else(|e| panic!("bad final line ({e}): {l}"));
    }
    assert!(lines.len() <= 1000, "retention did not cap the log: {} lines", lines.len());
    assert!(lines.last().unwrap().contains("\"i\":59"));
}

/// A lock that cannot be CREATED is a different answer from a lock somebody
/// holds, and the two used to print the same line.
///
/// `acquire_file_lock` folded every I/O error into "no lock", so a registry
/// this process may not write reported `event log is busy. Retry the
/// operation.` — untrue, and an instruction that can never work. Node throws
/// the underlying error out of `acquireFileLock` instead: it returns false
/// only on `EEXIST`.
///
/// node: src/sessions.ts:2293-2336
#[test]
fn a_registry_that_cannot_be_written_says_so_instead_of_busy() {
    use std::os::unix::fs::PermissionsExt;

    let rig = Rig::new();
    rig.daemon("at6", &["cat"], DaemonOpts::no_display_name());
    let root = rig.root().to_path_buf();
    let before = std::fs::metadata(&root).unwrap().permissions().mode();

    // Take away the write bit on the registry directory. Every lock file
    // lives directly in it, so no lock can be created at all.
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o500)).unwrap();
    let tag = rig.pty(&["tag", "at6", "a=1"]);
    let emit = rig.pty(&["emit", "at6", "user.blocked"]);
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(before)).unwrap();

    for (what, out) in [("tag", &tag), ("emit", &emit)] {
        let said = out.stderr();
        assert_ne!(out.status, 0, "pty {what} succeeded: {}", out.summary());
        assert!(
            !said.to_lowercase().contains("busy"),
            "pty {what} called a permission error busy: {}",
            out.summary()
        );
        assert!(
            said.to_lowercase().contains("permission denied") || said.contains("EACCES"),
            "pty {what} did not name the cause: {}",
            out.summary()
        );
    }
}
