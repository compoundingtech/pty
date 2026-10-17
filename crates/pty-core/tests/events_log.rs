//! The events log: envelope, the daemon writer, one-shot appends, retention
//! for both writer kinds, `clear_events`, `read_recent_events`, and the
//! `user.*` type rules.

mod registry_support;

use std::io::Read;
use std::time::Duration;

use pty_core::events::{self, Event, EventWriter, event_type};
use pty_core::registry;
use registry_support::{read_events, root, unique_name};
use serde_json::{Value, json};

fn line_count(name: &str) -> usize {
    std::fs::read_to_string(registry::events_path(name))
        .unwrap()
        .trim_end()
        .split('\n')
        .filter(|l| !l.is_empty())
        .count()
}

/// node: src/events.ts:32-40; src/server.ts:1204-1211
#[test]
fn envelope_is_session_type_ts_then_payload() {
    let e = Event::session_exit("s", 137, Some(9)).with_ts("2026-04-05T00:00:00.000Z");
    assert_eq!(
        e.to_json(),
        r#"{"session":"s","type":"session_exit","ts":"2026-04-05T00:00:00.000Z","exitCode":137,"signal":9}"#
    );
    let e = Event::session_exit("s", 0, None);
    assert!(!e.payload.contains_key("signal"));
    assert!(e.ts.ends_with('Z') && e.ts.len() == 24, "{}", e.ts);

    let parsed: Event = serde_json::from_str(
        r#"{"session":"s","type":"user.x","ts":"t","data":{"a":1},"text":"hi"}"#,
    )
    .unwrap();
    assert_eq!(parsed.session, "s");
    assert_eq!(parsed.r#type, "user.x");
    assert_eq!(parsed.ts, "t");
    assert_eq!(parsed.get("data"), Some(&json!({"a": 1})));
    assert_eq!(parsed.get_str("text"), Some("hi"));
    assert!(parsed.is_user_event());
    assert!(!Event::bell("s").is_user_event());
    // A numeric `ts` from a foreign writer is accepted and normalised.
    let numeric: Event = serde_json::from_str(r#"{"session":"s","type":"bell","ts":0}"#).unwrap();
    assert_eq!(numeric.ts, "1970-01-01T00:00:00.000Z");
}

/// Typed payload builders produce Node's exact shapes.
///
/// node: src/events.ts:42-191; src/server.ts:410-451, 581-584, 674-676
#[test]
fn payload_builders() {
    let t = registry::TagMap::from_iter([("role".to_string(), "web".to_string())]);
    let payload = |e: Event| Value::Object(e.payload);
    assert_eq!(payload(Event::bell("s")), json!({}));
    assert_eq!(
        payload(Event::title_change("s", "Building...")),
        json!({"value": "Building..."})
    );
    assert_eq!(
        payload(Event::notification(
            "s",
            Some("Done"),
            Some("Build ok"),
            Some(events::NotificationSource::Osc777)
        )),
        json!({"title": "Done", "body": "Build ok", "source": "osc777"})
    );
    assert_eq!(
        payload(Event::notification(
            "s",
            None,
            Some("data"),
            Some(events::NotificationSource::Osc9)
        )),
        json!({"body": "data", "source": "osc9"})
    );
    assert_eq!(payload(Event::focus_request("s")), json!({}));
    assert_eq!(payload(Event::cursor_visible("s")), json!({}));
    assert_eq!(payload(Event::session_start("s", None)), json!({}));
    assert_eq!(
        payload(Event::session_start("s", Some(&registry::TagMap::new()))),
        json!({})
    );
    assert_eq!(
        payload(Event::session_start("s", Some(&t))),
        json!({"tags": {"role": "web"}})
    );
    assert_eq!(
        payload(Event::session_exec("s", "cat", "sh")),
        json!({"previousCommand": "cat", "command": "sh"})
    );
    assert_eq!(payload(Event::session_respawn("s")), json!({}));
    assert_eq!(
        payload(Event::session_abandoned(
            "s",
            events::AbandonReason::CwdGone,
            None
        )),
        json!({"reason": "cwd-gone"})
    );
    assert_eq!(
        payload(Event::session_abandoned(
            "s",
            events::AbandonReason::Idle,
            Some(3)
        )),
        json!({"reason": "idle", "idleDays": 3})
    );
    assert_eq!(
        payload(Event::session_flapping("s", 3, 3, 60)),
        json!({"counter": 3, "limit": 3, "window": 60})
    );
    assert_eq!(
        payload(Event::user("s", "user.x", Some(json!({"pct": 40})), None)),
        json!({"data": {"pct": 40}})
    );
    assert_eq!(
        payload(Event::user("s", "user.x", None, Some("hi"))),
        json!({"text": "hi"})
    );
    assert_eq!(
        payload(Event::display_name_change("s", None, Some("new".into()))),
        json!({"previous": null, "value": "new"})
    );
    assert_eq!(
        payload(Event::tags_change("s", registry::TagMap::new(), t.clone())),
        json!({"previous": {}, "value": {"role": "web"}})
    );
    let snap = registry::MetadataChangeSnapshot {
        display_name: Some(None),
        tags: None,
    };
    assert_eq!(
        payload(Event::metadata_change("s", snap.clone(), snap)),
        json!({"previous": {"displayName": null}, "value": {"displayName": null}})
    );
    for t in event_type::ALL {
        assert!(!t.is_empty());
    }
    assert_eq!(Event::new("s", event_type::BELL).r#type, "bell");
}

/// node: tests/events.test.ts:107-129
#[test]
fn event_writer_appends_jsonl_lines() {
    let _ = root();
    let name = unique_name("ew");
    events::clear_events(&name).unwrap();
    let writer = EventWriter::new(&name);
    writer.append(Event::bell(&name).with_ts("2026-04-05T00:00:00.000Z"));
    writer.append(Event::bell(&name).with_ts("2026-04-05T00:00:01.000Z"));
    writer.flush();
    let evs = events::read_recent_events(&name, events::DEFAULT_RECENT_EVENTS);
    assert_eq!(evs.len(), 2);
    assert_eq!(evs[0].r#type, "bell");
    assert_eq!(evs[1].ts, "2026-04-05T00:00:01.000Z");
    writer.close();
}

/// node: tests/events.test.ts:131-160
#[test]
fn event_writer_truncates_when_exceeding_max_lines() {
    let _ = root();
    let name = unique_name("ewtrunc");
    events::clear_events(&name).unwrap();
    let writer = EventWriter::new(&name);
    for i in 0..1050 {
        writer.append(Event::bell(&name).with_ts(&format!("2026-04-05T00:00:{i:04}Z")));
    }
    writer.flush();
    let count = line_count(&name);
    assert!(count <= 650, "{count}");
    assert!(count > 0);
    let last = read_events(&name).pop().unwrap();
    assert_eq!(last["ts"], "2026-04-05T00:00:1049Z");
}

/// A raw reader that opened the old log before a concurrent append keeps one
/// complete snapshot while the current path advances to the retained suffix.
/// Opening and pausing after the first bytes makes the overlap deterministic:
/// an in-place append changes the reader's inode and fails this assertion,
/// whereas sibling-temp publication leaves it untouched.
#[test]
fn append_and_retention_publish_one_complete_snapshot_to_raw_readers() {
    let _ = root();
    let name = unique_name("snapshot");
    let path = registry::events_path(&name);
    let original = (0..1200)
        .map(|i| {
            json!({
                "session": name,
                "type": "user.prime",
                "ts": "2026-04-05T00:00:00.000Z",
                "data": {"i": i}
            })
            .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&path, &original).unwrap();

    let (opened_tx, opened_rx) = std::sync::mpsc::channel();
    let (published_tx, published_rx) = std::sync::mpsc::channel();
    let reader_path = path.clone();
    let reader = std::thread::spawn(move || {
        let mut file = std::fs::File::open(reader_path).unwrap();
        let mut snapshot = vec![0; 64];
        file.read_exact(&mut snapshot).unwrap();
        opened_tx.send(()).unwrap();
        published_rx.recv().unwrap();
        file.read_to_end(&mut snapshot).unwrap();
        snapshot
    });

    opened_rx.recv().unwrap();
    events::append_event_sync(
        &name,
        &Event::user(
            &name,
            "user.published",
            Some(json!({"marker": "last"})),
            None,
        )
        .with_ts("2026-04-05T00:00:01.000Z"),
    )
    .unwrap();
    published_tx.send(()).unwrap();

    assert_eq!(reader.join().unwrap(), original.as_bytes());
    let current = std::fs::read(&path).unwrap();
    assert_eq!(current.last(), Some(&b'\n'));
    let records: Vec<Value> = current
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).expect("complete JSONL record"))
        .collect();
    assert_eq!(records.len(), events::KEEP_LINES);
    assert!(
        current.len() < original.len(),
        "one retained publication should shrink the over-cap snapshot"
    );
    assert_eq!(records.last().unwrap()["type"], "user.published");
    assert_eq!(records.last().unwrap()["data"]["marker"], "last");
}

/// node: tests/events.test.ts:163-190
#[test]
fn read_recent_events_returns_last_n_and_empty_for_missing() {
    let _ = root();
    let name = unique_name("recent");
    events::clear_events(&name).unwrap();
    let lines: Vec<String> = (0..10)
        .map(|i| {
            json!({"session": name, "type": "bell", "ts": format!("2026-04-05T00:00:0{i}Z")})
                .to_string()
        })
        .collect();
    std::fs::write(registry::events_path(&name), lines.join("\n") + "\n").unwrap();
    let evs = events::read_recent_events(&name, 3);
    assert_eq!(evs.len(), 3);
    assert_eq!(evs[0].ts, "2026-04-05T00:00:07Z");
    assert_eq!(evs[2].ts, "2026-04-05T00:00:09Z");
    assert!(events::read_recent_events("nonexistent-session-xyz", 50).is_empty());
    // A malformed selected line empties the result, as Node's reader does;
    // `read_all_events` skips it instead.
    std::fs::write(
        registry::events_path(&name),
        lines.join("\n") + "\n{not json\n",
    )
    .unwrap();
    assert!(events::read_recent_events(&name, 3).is_empty());
    assert_eq!(events::read_all_events(&name).len(), 10);
}

/// node: tests/events.test.ts:192-198
#[test]
fn clear_events_creates_an_empty_file() {
    let _ = root();
    let name = unique_name("clear");
    std::fs::write(registry::events_path(&name), "x\n").unwrap();
    events::clear_events(&name).unwrap();
    assert_eq!(
        std::fs::read_to_string(registry::events_path(&name)).unwrap(),
        ""
    );
    let _held = registry::acquire_event_lock(&name).unwrap();
    assert_eq!(
        events::clear_events(&name),
        Err(format!(
            "Session id \"{name}\" event log is busy. Retry the operation."
        ))
    );
}

#[test]
fn publication_clear_waits_for_transient_event_ownership_with_a_deadline() {
    let _ = root();
    let name = unique_name("clear-wait");
    let path = registry::events_path(&name);
    std::fs::write(&path, "predecessor event\n").unwrap();
    let held = registry::acquire_event_lock(&name).unwrap();
    let started = std::time::Instant::now();
    let budget = Duration::from_millis(50);
    let refused = events::clear_events_with_wait(&name, budget);
    assert!(started.elapsed() >= budget, "publication clear failed before its lock deadline");
    assert_eq!(
        refused,
        Err(format!("Session id \"{name}\" event log is busy. Retry the operation."))
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "predecessor event\n");

    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let publisher = std::thread::spawn(move || {
        ready_tx.send(()).unwrap();
        done_tx.send(events::clear_events_with_wait(&name, Duration::from_secs(2))).unwrap();
    });
    ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(
        done_rx.recv_timeout(Duration::from_millis(100)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout),
        "publication must wait rather than reject the transient holder"
    );
    held.release();
    done_rx.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
    publisher.join().unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), "");
}

/// node: tests/events.test.ts:200-212
#[test]
fn cleanup_all_removes_the_events_file() {
    let _ = root();
    let name = unique_name("rmev");
    events::clear_events(&name).unwrap();
    std::fs::write(
        registry::events_path(&name),
        "{\"session\":\"x\",\"type\":\"bell\",\"ts\":\"t\"}\n",
    )
    .unwrap();
    assert!(registry::events_path(&name).exists());
    registry::cleanup_all(&name).unwrap();
    assert!(!registry::events_path(&name).exists());
    events::clear_events(&name).unwrap();
    events::remove_events(&name).unwrap();
    assert!(!registry::events_path(&name).exists());
}

/// node: tests/events-emit.test.ts:92-110
#[test]
fn validate_user_event_type_messages() {
    assert_eq!(events::validate_user_event_type("user.build-done"), None);
    assert_eq!(events::validate_user_event_type("user.a"), None);
    for bad in ["build-done", "session_start", "state.set"] {
        let msg = events::validate_user_event_type(bad).unwrap();
        assert_eq!(
            msg,
            format!("custom events must start with \"user.\" (got \"{bad}\")")
        );
        assert!(msg.contains("must start with"));
    }
    assert_eq!(
        events::validate_user_event_type("user.").unwrap(),
        "event type \"user.\" needs a suffix (e.g. \"user.build-done\")"
    );
    assert_eq!(
        events::validate_user_event_type("").unwrap(),
        "event type must be a non-empty string"
    );
    for bad in [
        "user.has space",
        "user.tab\tfoo",
        "user.nl\n",
        "user.ctl\u{1}",
        "user.nbsp\u{a0}x",
    ] {
        assert_eq!(
            events::validate_user_event_type(bad).unwrap(),
            "event type may not contain whitespace or control characters",
            "{bad:?}"
        );
    }
}

/// node: tests/events-emit.test.ts:112-135
#[test]
fn emit_user_event_round_trips_and_rejects_bad_types() {
    let _ = root();
    let name = unique_name("emit");
    events::emit_user_event(&name, "user.build-done", Some(json!({"pct": 100})), None).unwrap();
    events::emit_user_event(&name, "user.note", None, Some("hello")).unwrap();
    let evs = events::read_recent_events(&name, 50);
    assert_eq!(evs.len(), 2);
    assert_eq!(evs[0].r#type, "user.build-done");
    assert_eq!(evs[0].get("data"), Some(&json!({"pct": 100})));
    assert!(evs[0].get("text").is_none());
    assert_eq!(evs[1].r#type, "user.note");
    assert_eq!(evs[1].get_str("text"), Some("hello"));
    assert!(evs[1].get("data").is_none());
    assert!(evs[0].is_user_event());

    let err = events::emit_user_event(&name, "bogus-type", None, None).unwrap_err();
    assert!(err.contains("must start with"));
    assert_eq!(
        events::read_recent_events(&name, 50).len(),
        2,
        "nothing written for a bad type"
    );
}

/// node: tests/events-emit.test.ts:237-262
#[test]
fn append_event_retention_caps_the_log_and_keeps_the_tail() {
    let _ = root();
    let name = unique_name("loop");
    for i in 0..1200 {
        events::emit_user_event(&name, "user.loop", Some(json!({"i": i})), None).unwrap();
    }
    let content = std::fs::read_to_string(registry::events_path(&name)).unwrap();
    assert!(content.trim_end().split('\n').count() <= 1000);
    assert!(content.contains("\"i\":1199"));
    // The rewrite leaves no temporaries behind.
    let leftovers: Vec<String> = std::fs::read_dir(root())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(&name) && n.contains(".tmp."))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

/// node: tests/atomic-writes.test.ts:247-266
#[test]
fn append_event_queues_until_the_event_lock_is_released() {
    let _ = root();
    let name = unique_name("queued");
    let held = registry::acquire_event_lock(&name).unwrap();
    let pending_name = name.clone();
    let pending = std::thread::spawn(move || {
        events::append_event(
            &pending_name,
            &Event::user(
                &pending_name,
                "user.queued",
                Some(json!({"complete": true})),
                None,
            ),
        )
    });
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        events::read_recent_events(&name, 50).is_empty(),
        "the append must wait"
    );
    held.release();
    pending.join().unwrap().unwrap();
    let evs = events::read_recent_events(&name, 50);
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0].r#type, "user.queued");
    assert_eq!(evs[0].get("data"), Some(&json!({"complete": true})));
}

/// node: src/events.ts:286-295
#[test]
fn append_event_sync_fails_fast_when_locked() {
    let _ = root();
    let name = unique_name("syncbusy");
    let _held = registry::acquire_event_lock(&name).unwrap();
    let err = events::append_event_sync(&name, &Event::bell(&name)).unwrap_err();
    assert_eq!(
        err,
        format!("Session id \"{name}\" event log is busy. Retry the operation.")
    );
    assert!(!registry::events_path(&name).exists());
}

/// node: tests/atomic-writes.test.ts:344-409
#[test]
fn reader_never_sees_a_half_written_file_during_truncation() {
    let _ = root();
    let name = unique_name("trunc");
    for i in 0..1200 {
        events::append_event_sync(
            &name,
            &Event::user(&name, "user.prime", Some(json!({"i": i})), None),
        )
        .unwrap();
    }
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader_done = done.clone();
    let reader_name = name.clone();
    let reader = std::thread::spawn(move || {
        let mut errors = Vec::new();
        while !reader_done.load(std::sync::atomic::Ordering::Relaxed) {
            let content =
                std::fs::read_to_string(registry::events_path(&reader_name)).unwrap_or_default();
            if !content.is_empty() && !content.ends_with('\n') {
                errors.push("file without trailing newline".to_string());
            }
            for line in content.lines().filter(|l| !l.is_empty()) {
                if serde_json::from_str::<Value>(line).is_err() {
                    errors.push(format!("bad line: {line}"));
                }
            }
            std::thread::yield_now();
        }
        errors
    });
    for i in 0..500 {
        events::append_event_sync(
            &name,
            &Event::user(&name, "user.more", Some(json!({"i": i})), None),
        )
        .unwrap();
    }
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let errors = reader.join().unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    assert!(line_count(&name) < 1200);
    let content = std::fs::read_to_string(registry::events_path(&name)).unwrap();
    assert!(content.contains("\"i\":499"));
}

/// Event lines over 4096 bytes are fine.
///
/// node: tests/atomic-writes.test.ts:283-342
#[test]
fn oversized_event_lines_survive() {
    let _ = root();
    let name = unique_name("big");
    let big = "😀".repeat(1200);
    events::append_event(
        &name,
        &Event::user(&name, "user.big", Some(json!({"d": big})), None),
    )
    .unwrap();
    let evs = events::read_recent_events(&name, 50);
    assert_eq!(evs.len(), 1);
    assert!(evs[0].to_json().len() > 4096);
    assert_eq!(
        evs[0].get("data").unwrap()["d"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        1200
    );
}
