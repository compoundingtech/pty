//! `EventFollower`: existing files from EOF, files created while following
//! from offset 0, a shrink restarts at 0, `--all` picks up new logs, and the
//! metadata patches are delivered live.

mod registry_support;

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use pty_core::events::{self, Event, EventFollower, EventWriter, FollowerOptions};
use pty_core::registry::{self, TagMap};
use registry_support::{root, unique_name, wait_for};
use serde_json::json;

fn collect(rx: &Receiver<Event>, want: usize, ms: u64) -> Vec<Event> {
    let deadline = Instant::now() + Duration::from_millis(ms);
    let mut out = Vec::new();
    while out.len() < want && Instant::now() < deadline {
        if let Ok(e) = rx.recv_timeout(Duration::from_millis(50)) {
            out.push(e);
        }
    }
    while let Ok(e) = rx.try_recv() {
        out.push(e);
    }
    out
}

/// node: tests/events.test.ts:298-334
#[test]
fn follows_new_events_appended_to_a_file() {
    let _ = root();
    let name = unique_name("fol");
    events::clear_events(&name).unwrap();
    let (mut follower, rx) = EventFollower::channel(FollowerOptions::names(vec![name.clone()]));
    std::thread::sleep(Duration::from_millis(100));

    let writer = EventWriter::new(&name);
    writer.append(Event::bell(&name).with_ts("2026-04-05T00:00:00Z"));
    writer.append(Event::bell(&name).with_ts("2026-04-05T00:00:01Z"));
    writer.flush();

    let received = collect(&rx, 2, 3000);
    follower.stop();
    assert!(received.len() >= 2, "{received:?}");
    assert_eq!(received[0].r#type, "bell");
    assert_eq!(received[1].ts, "2026-04-05T00:00:01Z");
}

/// An existing file's history is not replayed.
///
/// node: src/events.ts:466-476
#[test]
fn existing_files_start_at_eof() {
    let _ = root();
    let name = unique_name("eof");
    events::append_event_sync(&name, &Event::bell(&name)).unwrap();
    events::append_event_sync(&name, &Event::bell(&name)).unwrap();
    let (mut follower, rx) = EventFollower::channel(FollowerOptions::names(vec![name.clone()]));
    std::thread::sleep(Duration::from_millis(300));
    assert!(rx.try_recv().is_err(), "history must not replay");
    events::append_event_sync(&name, &Event::title_change(&name, "now")).unwrap();
    let received = collect(&rx, 1, 3000);
    follower.stop();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].r#type, "title_change");
}

/// An event appended as soon as `start` returns is new, not history: the
/// follower has its end-of-file baseline by then, as Node's does. When the
/// thread took it later, a busy host turned these events into history and
/// they were never delivered.
#[test]
fn an_event_appended_right_after_start_is_delivered() {
    let _ = root();
    for i in 0..50 {
        let name = unique_name("start");
        events::append_event_sync(&name, &Event::bell(&name)).unwrap();
        let (mut follower, rx) = EventFollower::channel(FollowerOptions {
            poll_interval: Duration::from_millis(10),
            ..FollowerOptions::names(vec![name.clone()])
        });
        events::append_event_sync(&name, &Event::title_change(&name, "now")).unwrap();
        let received = collect(&rx, 1, 10_000);
        follower.stop();
        assert_eq!(received.len(), 1, "iteration {i}: {received:?}");
        assert_eq!(received[0].r#type, "title_change");
    }
}

#[test]
fn follower_sees_retained_new_records_when_a_batch_shrinks_and_regrows() {
    let _ = root();
    let name = unique_name("batch-retention");
    let history: String = (0..900)
        .map(|i| {
            format!(
                "{}\n",
                Event::bell(&name).with_ts(&format!("old-{i}")).to_json()
            )
        })
        .collect();
    std::fs::write(registry::events_path(&name), &history).unwrap();
    let (mut follower, rx) = EventFollower::channel(FollowerOptions::names(vec![name.clone()]));
    std::thread::sleep(Duration::from_millis(100));
    let writer = EventWriter::new(&name);
    let padding = "x".repeat(2000);
    for i in 0..300 {
        writer.append(
            Event::new(&name, "user.batch")
                .with("seq", i)
                .with("padding", padding.as_str()),
        );
    }
    writer.flush();
    assert!(
        std::fs::metadata(registry::events_path(&name))
            .unwrap()
            .len()
            > history.len() as u64
    );
    let mut seen = std::collections::BTreeSet::new();
    assert!(wait_for(3000, || {
        while let Ok(event) = rx.try_recv() {
            if event.r#type == "user.batch" {
                seen.insert(event.get("seq").unwrap().as_u64().unwrap());
            }
        }
        seen.len() == 300
    }));
    follower.stop();
    assert_eq!(seen, (0..300).collect());
}

/// node: tests/events.test.ts:336-364
#[test]
fn directory_watch_replays_new_files_from_offset_zero() {
    let _ = root();
    let name = unique_name("newfile");
    let _ = std::fs::remove_file(registry::events_path(&name));
    let (mut follower, rx) = EventFollower::channel(FollowerOptions::all());
    std::thread::sleep(Duration::from_millis(100));

    let writer = EventWriter::new(&name);
    writer.append(Event::session_start(&name, None).with_ts("2026-04-05T00:00:00Z"));
    writer.flush();

    let received: Vec<Event> = collect(&rx, 1, 3000)
        .into_iter()
        .filter(|e| e.session == name)
        .collect();
    // Give any duplicate a chance to show up before asserting exactly one.
    std::thread::sleep(Duration::from_millis(400));
    let late: Vec<Event> = rx.try_iter().filter(|e| e.session == name).collect();
    follower.stop();
    let starts = received
        .iter()
        .chain(late.iter())
        .filter(|e| e.r#type == "session_start")
        .count();
    assert_eq!(starts, 1);
}

/// node: tests/events.test.ts:366-410
#[test]
fn handles_file_truncation_gracefully() {
    let _ = root();
    let name = unique_name("trunc");
    events::clear_events(&name).unwrap();
    let line = Event::bell(&name).with_ts("2026-04-05T00:00:00Z").to_json() + "\n";
    std::fs::write(registry::events_path(&name), line.repeat(5)).unwrap();

    let (mut follower, rx) = EventFollower::channel(FollowerOptions::names(vec![name.clone()]));
    std::thread::sleep(Duration::from_millis(100));
    std::fs::write(registry::events_path(&name), "").unwrap();
    std::thread::sleep(Duration::from_millis(100));

    let writer = EventWriter::new(&name);
    writer.append(
        Event::notification(
            &name,
            Some("After truncation"),
            None,
            Some(events::NotificationSource::Osc9),
        )
        .with_ts("2026-04-05T00:01:00Z"),
    );
    writer.flush();
    let received = collect(&rx, 1, 3000);
    follower.stop();
    let notifications = received
        .iter()
        .filter(|e| e.r#type == "notification")
        .count();
    assert!(notifications >= 1, "{received:?}");
}

/// A retention rewrite (smaller file, new inode) is followed too.
///
/// node: src/events.ts:494-498
#[test]
fn follows_across_an_atomic_retention_rewrite() {
    let _ = root();
    let name = unique_name("rewrite");
    for i in 0..1100 {
        events::append_event_sync(
            &name,
            &Event::user(&name, "user.prime", Some(json!({"i": i})), None),
        )
        .unwrap();
    }
    let (mut follower, rx) = EventFollower::channel(FollowerOptions::names(vec![name.clone()]));
    std::thread::sleep(Duration::from_millis(100));
    // Force the rewrite: the one-shot writer truncates once the file is
    // over the size threshold and at/over 1000 lines.
    events::append_event_sync(
        &name,
        &Event::user(&name, "user.after", None, Some("post-rewrite")),
    )
    .unwrap();
    let got = collect(&rx, 1, 3000);
    assert!(wait_for(2000, || std::fs::read_to_string(
        registry::events_path(&name)
    )
    .unwrap()
    .lines()
    .count()
        < 1100));
    events::append_event_sync(&name, &Event::user(&name, "user.after2", None, None)).unwrap();
    let mut got2 = collect(&rx, 1, 3000);
    follower.stop();
    let mut all = got;
    all.append(&mut got2);
    assert!(
        all.iter().any(|e| e.r#type == "user.after2"),
        "{:?}",
        all.iter().map(|e| &e.r#type).collect::<Vec<_>>()
    );
}

/// node: tests/events-emit.test.ts:264-303
#[test]
fn delivers_user_events_in_order() {
    let _ = root();
    let name = unique_name("user");
    events::clear_events(&name).unwrap();
    let (mut follower, rx) = EventFollower::channel(FollowerOptions::names(vec![name.clone()]));
    std::thread::sleep(Duration::from_millis(100));
    events::emit_user_event(&name, "user.first", None, Some("one")).unwrap();
    events::emit_user_event(&name, "user.second", Some(json!({"n": 2})), None).unwrap();
    let received = collect(&rx, 2, 3000);
    follower.stop();
    let types: Vec<&str> = received.iter().map(|e| e.r#type.as_str()).collect();
    assert_eq!(types, ["user.first", "user.second"]);
    assert!(received[0].is_user_event());
    assert_eq!(received[0].get_str("text"), Some("one"));
    assert_eq!(received[1].get("data"), Some(&json!({"n": 2})));
}

/// node: tests/metadata-events.test.ts:489-520, 600-628
#[test]
fn delivers_display_name_and_tags_changes_live() {
    let _ = root();
    let name = unique_name("live");
    registry::write_metadata(
        &name,
        &registry::SessionMetadata {
            command: "cat".into(),
            display_command: "cat".into(),
            cwd: "/tmp".into(),
            created_at: registry::now_iso8601(),
            ..Default::default()
        },
    )
    .unwrap();
    events::clear_events(&name).unwrap();
    let (mut follower, rx) = EventFollower::channel(FollowerOptions::names(vec![name.clone()]));
    std::thread::sleep(Duration::from_millis(100));

    registry::set_display_name(&name, Some("live-label")).unwrap();
    let mut live: TagMap = TagMap::new();
    live.insert("live".into(), "yes".into());
    registry::update_tags(&name, &live, &[]).unwrap();

    let received = collect(&rx, 2, 3000);
    follower.stop();
    let dn: Vec<&Event> = received
        .iter()
        .filter(|e| e.r#type == "display_name_change")
        .collect();
    assert_eq!(dn.len(), 1);
    assert_eq!(dn[0].get_str("value"), Some("live-label"));
    let tc: Vec<&Event> = received
        .iter()
        .filter(|e| e.r#type == "tags_change")
        .collect();
    assert_eq!(tc.len(), 1);
    assert_eq!(tc[0].get("value"), Some(&json!({"live": "yes"})));
}

/// The callback form and `stop` on drop.
#[test]
fn callback_form_and_drop() {
    let _ = root();
    let name = unique_name("cb");
    events::clear_events(&name).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let follower = EventFollower::start(FollowerOptions::names(vec![name.clone()]), move |e| {
        let _ = tx.send(e.r#type);
    });
    std::thread::sleep(Duration::from_millis(100));
    events::append_event_sync(&name, &Event::focus_request(&name)).unwrap();
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(3)).unwrap(),
        "focus_request"
    );
    drop(follower);
    events::append_event_sync(&name, &Event::bell(&name)).unwrap();
    assert!(
        rx.recv_timeout(Duration::from_millis(600)).is_err(),
        "stopped followers deliver nothing"
    );
}

#[test]
fn terminal_metadata_drains_an_exit_published_after_the_first_read() {
    let _ = root();
    let name = unique_name("exit-between-scans");
    registry::write_metadata(&name, &registry::SessionMetadata::default()).unwrap();
    events::clear_events(&name).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let (publication_tx, publication_rx) = std::sync::mpsc::channel();
    let callback_name = name.clone();
    let follower = EventFollower::start(
        FollowerOptions::names(vec![name.clone()]),
        move |event| {
            if event.r#type == "bell" {
                // The reader already captured this scan's bytes. Wait until
                // the preceding append has returned and released its event
                // lock before reproducing finalize_exit's log then metadata
                // publication, still before the follower checks metadata.
                publication_rx.recv().unwrap();
                events::append_event_sync(
                    &callback_name,
                    &Event::session_exit(&callback_name, 0, None),
                )
                .unwrap();
                registry::write_metadata(
                    &callback_name,
                    &registry::SessionMetadata {
                        exit_code: Some(0),
                        exited_at: Some(registry::now_iso8601()),
                        ..Default::default()
                    },
                )
                .unwrap();
            }
            tx.send(event.r#type).unwrap();
        },
    );
    events::append_event_sync(&name, &Event::bell(&name)).unwrap();
    publication_tx.send(()).unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), "bell");
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        "session_exit"
    );
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(2)),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
    ));
    drop(follower);
}

#[test]
fn registry_removal_waits_for_a_locked_respawn_to_finish() {
    let _ = root();
    for republish in [true, false] {
        let name = unique_name("locked-respawn");
        registry::write_metadata(&name, &registry::SessionMetadata::default()).unwrap();
        events::clear_events(&name).unwrap();
        let (follower, rx) = EventFollower::channel(FollowerOptions {
            poll_interval: Duration::from_millis(20),
            ..FollowerOptions::names(vec![name.clone()])
        });
        // As in reconcile_permanent, keep the creation claim across removal
        // and replacement publication, including more than one fallback scan.
        let claim = registry::try_acquire_file_lock(&registry::lock_path(&name))
            .unwrap()
            .unwrap();
        std::fs::remove_file(registry::metadata_path(&name)).unwrap();
        std::fs::remove_file(registry::events_path(&name)).unwrap();
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(300)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ), "a locked publication gap must not disconnect the follower");
        if republish {
            registry::write_metadata(&name, &registry::SessionMetadata::default()).unwrap();
        }
        claim.release();
        if republish {
            events::append_event_sync(&name, &Event::session_respawn(&name)).unwrap();
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(2)).unwrap().r#type,
                "session_respawn"
            );
            events::append_event_sync(&name, &Event::session_exit(&name, 0, None)).unwrap();
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(2)).unwrap().r#type,
                "session_exit"
            );
        }
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        ));
        drop(follower);
    }
}
