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

#[test]
fn retention_does_not_replay_events_from_before_subscription() {
    let _ = root();
    let name = unique_name("exact-retention");
    let path = registry::events_path(&name);
    let old = (0..999)
        .map(|i| Event::title_change(&name, &format!("old-{i}")).to_json() + "\n")
        .collect::<String>();
    std::fs::write(&path, old).unwrap();

    let (mut follower, rx) = EventFollower::channel(FollowerOptions::names(vec![name.clone()]));
    std::thread::sleep(Duration::from_millis(100));
    events::append_event_sync(&name, &Event::title_change(&name, "new-0")).unwrap();
    events::append_event_sync(&name, &Event::title_change(&name, "new-1")).unwrap();

    let received = collect(&rx, 2, 3000);
    std::thread::sleep(Duration::from_millis(400));
    let rest: Vec<_> = rx.try_iter().collect();
    follower.stop();
    let titles: Vec<_> = received
        .iter()
        .chain(rest.iter())
        .filter_map(|e| e.get_str("value"))
        .collect();
    assert_eq!(titles, ["new-0", "new-1"]);
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
