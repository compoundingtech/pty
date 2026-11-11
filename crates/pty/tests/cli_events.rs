//! `pty events`: `--recent`, `--wait` with and without a timeout, the
//! usage errors, and the text format.
//!
//! node: tests/peek-wait.test.ts:205-226, tests/metadata-events.test.ts:632-692,
//! src/cli.ts:1219-1248, 3965-4051

mod cli_common;

use std::io::{BufRead, Write};
use std::process::Stdio;
use std::time::{Duration, Instant};

use cli_common::Rig;
use serde_json::json;

fn append_event(rig: &Rig, name: &str, line: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(rig.path(&format!("{name}.events.jsonl")))
        .unwrap();
    writeln!(f, "{line}").unwrap();
}

fn parse_json_lines(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn recent_type_filter_returns_every_retained_exact_match_in_order_without_writes() {
    const RECENT_BOUND: usize = 50;

    let rig = Rig::new();
    rig.write_meta("query", json!({"createdAt": "2026-01-01T00:00:00.000Z"}));
    let mut matching = vec![
        json!({
            "session": "query",
            "type": "user.note",
            "ts": "2026-01-02T03:04:05.000Z",
            "text": "first",
            "data": {"sequence": 1}
        }),
        json!({
            "session": "query",
            "type": "user.note",
            "ts": "2026-01-02T03:04:08.000Z",
            "text": "second",
            "data": {"sequence": 2}
        }),
    ];
    let notice = json!({
        "session": "query",
        "type": "user.notice",
        "ts": "2026-01-02T03:04:06.000Z",
        "text": "prefix must not match"
    });
    let mut events = vec![matching[0].clone(), notice.clone(), matching[1].clone()];
    for sequence in 0..=RECENT_BOUND {
        events.push(json!({
            "session": "query",
            "type": "bell",
            "ts": format!("2026-01-02T04:00:{sequence:02}.000Z"),
            "data": {"sequence": sequence}
        }));
    }
    for sequence in 3..=RECENT_BOUND + 3 {
        let event = json!({
            "session": "query",
            "type": "user.note",
            "ts": format!("2026-01-02T05:00:{sequence:02}.000Z"),
            "data": {"sequence": sequence}
        });
        matching.push(event.clone());
        events.push(event);
    }
    let path = rig.path("query.events.jsonl");
    let raw = events.iter().map(serde_json::Value::to_string).collect::<Vec<_>>().join("\n") + "\n";
    std::fs::write(&path, raw).unwrap();
    let before = std::fs::read(&path).unwrap();

    let unfiltered = rig.ok(&["events", "--recent", "--json", "query"]);
    assert_eq!(
        parse_json_lines(&unfiltered.stdout),
        events[events.len() - RECENT_BOUND..]
    );
    assert_eq!(
        parse_json_lines(
            &rig.ok(&["events", "--recent", "--json", "--type", "user.note", "query"])
                .stdout
        ),
        matching
    );
    assert_eq!(
        rig.ok(&["events", "--recent", "--json", "--type", "user.notice", "query"])
            .json(),
        notice
    );
    assert_eq!(
        rig.ok(&["events", "--recent", "--json", "--type", "user", "query"])
            .stdout,
        ""
    );
    assert_eq!(
        rig.ok(&["events", "--recent", "--json", "--type", "User.Note", "query"])
            .stdout,
        ""
    );
    assert_eq!(
        rig.ok(&["events", "--recent", "--json", "--type", "missing", "query"])
            .stdout,
        ""
    );
    assert_eq!(std::fs::read(&path).unwrap(), before, "retained event file changed");
}

/// node: src/cli.ts:1226-1236, 3969-3982
#[test]
fn recent_and_usage() {
    let rig = Rig::new();
    rig.write_meta("s", json!({
        "displayName": "Friendly", "createdAt": "2026-01-01T00:00:00.000Z"
    }));
    assert_eq!(rig.ok(&["events", "--recent", "s"]).stdout, "No recent events for \"s\".\n");
    append_event(&rig, "s", r#"{"session":"s","type":"session_start","ts":"2026-01-02T03:04:05.000Z","tags":{"role":"web"}}"#);
    append_event(&rig, "s", r#"{"session":"s","type":"user.note","ts":"2026-01-02T03:04:06.000Z","text":"hi"}"#);
    append_event(&rig, "s", r#"{"session":"s","type":"display_name_change","ts":"2026-01-02T03:04:07.000Z","previous":"old","value":"new"}"#);
    let out = rig.ok(&["events", "--recent", "--json", "Friendly"]);
    assert_eq!(out.stdout.lines().count(), 3);
    assert_eq!(
        out.stdout.lines().next().unwrap(),
        r#"{"session":"s","type":"session_start","ts":"2026-01-02T03:04:05.000Z","tags":{"role":"web"}}"#
    );
    let text = rig.ok(&["events", "--recent", "s"]).stdout;
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].ends_with("] s: started role=web"), "{:?}", lines[0]);
    assert!(lines[1].ends_with("] s: user.note \"hi\""), "{:?}", lines[1]);
    assert!(lines[2].ends_with("] s: display_name -> \"new\" (was \"old\")"), "{:?}", lines[2]);
    assert!(lines[0].starts_with('['));
    assert_eq!(lines[0].as_bytes()[9], b']');

    let out = rig.run(&["events"]);
    assert_eq!(out.code, 1);
    assert_eq!(
        out.stderr,
        "Usage: pty events [--all] [--recent] [--json] [--type <type>] [--wait <type>] [-t <seconds>] [<name>]\n"
    );
    let out = rig.run(&["events", "--recent", "--all"]);
    assert_eq!(out.code, 1);
    assert_eq!(out.stderr, "--recent requires a session name.\n");
    let out = rig.run(&["events", "--wait", "bell", "--all"]);
    assert_eq!(out.code, 1);
    assert_eq!(out.stderr, "--wait requires a session name.\n");
    let out = rig.run(&["events", "--type", "bell", "s"]);
    assert_eq!(out.code, 1);
    assert_eq!(out.stderr, "--type requires --recent.\n");
    let out = rig.run(&["events", "--recent", "--type"]);
    assert_eq!(out.code, 1);
    assert_eq!(out.stderr, "--type requires an event type.\n");
    let out = rig.run(&["events", "--recent", "ghost"]);
    assert_eq!(out.code, 1);
    assert_eq!(out.stderr, "Session \"ghost\" not found.\n");
    // An unknown dash token ends the flag loop and becomes the ref.
    let out = rig.run(&["events", "--bogus"]);
    assert_eq!(out.stderr, "Session \"--bogus\" not found.\n");
}

/// node: tests/peek-wait.test.ts:205-226
#[test]
fn wait_prints_the_first_matching_event_or_times_out() {
    let rig = Rig::new();
    rig.spawn_cat("w", &[]);
    let mut metadata = rig.read_meta("w").unwrap();
    metadata["createdAt"] = json!("2026-01-01T00:00:00.000Z");
    rig.write_meta("w", metadata);
    let out = rig.run(&["events", "--wait", "bell", "-t", "1", "w"]);
    assert_eq!(out.code, 1);
    assert_eq!(out.stderr, "Timed out after 1s waiting for \"bell\" event.\n");
    let out = rig.run(&["events", "--wait", "bell", "--timeout", "0.5", "w"]);
    assert_eq!(out.stderr, "Timed out after 0.5s waiting for \"bell\" event.\n");

    let mut child = rig
        .cmd(&["events", "--wait", "bell", "-t", "10", "--json", "w"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(400));
    append_event(&rig, "w", r#"{"session":"w","type":"user.other","ts":"2026-01-02T03:04:05.000Z"}"#);
    append_event(&rig, "w", r#"{"session":"w","type":"bell","ts":"2026-01-02T03:04:06.000Z"}"#);
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "{\"session\":\"w\",\"type\":\"bell\",\"ts\":\"2026-01-02T03:04:06.000Z\"}\n"
    );
}

/// node: src/cli.ts:4028-4031, 4044-4047 — SIGINT ends a follow with exit 0.
#[test]
fn follow_exits_zero_on_sigint() {
    let rig = Rig::new();
    rig.write_meta("f", json!({}));
    let mut child = rig
        .cmd(&["events", "f"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(400));
    append_event(&rig, "f", r#"{"session":"f","type":"bell","ts":"2026-01-02T03:04:06.000Z"}"#);
    std::thread::sleep(Duration::from_millis(600));
    let _ = pty_core::kill::process(child.id() as i32, libc::SIGINT);
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).ends_with("] f: bell\n"));
}

fn finish_within(mut child: std::process::Child, limit: Duration) -> std::process::Output {
    let deadline = Instant::now() + limit;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let out = child.wait_with_output().unwrap();
            panic!("events did not exit within {limit:?}: {out:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn timeout_applies_after_the_reference_for_wait_and_follow() {
    let rig = Rig::new();
    rig.spawn_cat("deadline", &[]);
    for args in [
        vec!["events", "deadline", "--wait", "user.never", "--timeout", "1", "--json"],
        vec!["events", "deadline", "--wait", "user.never", "-t", "1", "--json"],
        vec!["events", "deadline", "--timeout", "1", "--json"],
        vec!["events", "--all", "-t", "1", "--json"],
    ] {
        let started = Instant::now();
        let child = rig.cmd(&args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let out = finish_within(child, Duration::from_secs(2));
        assert_eq!(out.status.code(), Some(1), "{args:?}: {out:?}");
        assert!(started.elapsed() >= Duration::from_secs(1));
        assert!(String::from_utf8_lossy(&out.stderr).contains("Timed out after 1s"));
    }
}

fn ready_follower(rig: &Rig, name: &str) -> (std::process::Child, std::thread::JoinHandle<String>) {
    let mut child = rig.cmd(&["events", name, "--json"])
        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut output = String::new();
        for line in std::io::BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if serde_json::from_str::<serde_json::Value>(&line).unwrap()["type"] == "user.ready" {
                let _ = tx.send(());
            }
            output.push_str(&line);
            output.push('\n');
        }
        output
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        // A sentinel written before the follower's EOF snapshot can be history;
        // keep publishing until stdout proves this follower consumed one.
        append_event(rig, name, &json!({
            "session": name, "type": "user.ready", "ts": "2026-01-02T03:04:05.000Z"
        }).to_string());
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(()) => return (child, reader),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
            result => {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("follower never acknowledged readiness: {result:?}; {}", reader.join().unwrap());
            }
        }
    }
}

#[test]
fn named_followers_stop_on_exit_or_registry_removal_after_readiness() {
    let rig = Rig::new();
    for (name, remove) in [("exit-follow", false), ("remove-follow", true)] {
        rig.write_meta(name, json!({}));
        let (child, reader) = ready_follower(&rig, name);
        if remove {
            std::fs::remove_file(rig.path(&format!("{name}.json"))).unwrap();
        } else {
            append_event(&rig, name, &json!({
                "session": name, "type": "session_exit",
                "ts": "2026-01-02T03:04:06.000Z", "exitCode": 0
            }).to_string());
        }
        let out = finish_within(child, Duration::from_secs(2));
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        let stdout = reader.join().unwrap();
        if !remove {
            assert!(stdout.contains("session_exit"), "{stdout}");
        }
    }
}

#[test]
fn wait_fails_without_match_for_retained_exited_session() {
    let rig = Rig::new();
    // --wait deliberately emits no readiness or nonmatching events. Publish
    // retained terminal metadata before startup instead of timing readiness;
    // core channel tests separately prove live removal and exit ordering.
    rig.write_meta("exited-wait", json!({
        "exitCode": 0, "exitedAt": "2026-01-02T03:04:06.000Z"
    }));
    append_event(&rig, "exited-wait", &json!({
        "session": "exited-wait", "type": "session_exit",
        "ts": "2026-01-02T03:04:06.000Z", "exitCode": 0
    }).to_string());
    let child = rig.cmd(&["events", "exited-wait", "--wait", "user.never", "--json"])
        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let out = finish_within(child, Duration::from_secs(2));
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
}

#[test]
fn follower_exits_after_a_real_session_ends() {
    let rig = Rig::new();
    rig.ok(&["run", "-d", "--id", "finite", "--", "sleep", "1"]);
    let child = rig.cmd(&["events", "finite", "--json"])
        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let out = finish_within(child, Duration::from_secs(3));
    assert_eq!(out.status.code(), Some(0), "{out:?}");
}

#[test]
fn an_unexpected_argument_is_refused_rather_than_dropping_the_options_after_it() {
    let rig = Rig::new();
    rig.write_meta("strict", json!({}));
    for (args, unexpected) in [
        (vec!["events", "strict", "--wait", "user.never", "--timeout=1"], "--timeout=1"),
        (vec!["events", "strict", "--wait", "user.never", "-t=1"], "-t=1"),
        (vec!["events", "strict", "--wait=user.never", "-t", "1"], "--wait=user.never"),
        (vec!["events", "strict", "--wait", "user.never", "--bogus", "-t", "1"], "--bogus"),
        (vec!["events", "--wait", "user.never", "-t", "1", "strict", "other"], "other"),
        (vec!["events", "strict", "--json", "--wait"], "--wait"),
        (vec!["events", "--timeout=1", "strict"], "--timeout=1"),
    ] {
        let child = rig.cmd(&args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let out = finish_within(child, Duration::from_secs(2));
        assert_eq!(out.status.code(), Some(1), "{args:?}: {out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            format!("Unexpected argument: {unexpected}\n"),
            "{args:?}"
        );
    }
}

/// A daemon that recorded itself and died without recording an exit can
/// append nothing more, so waits and named follows on it end, whether they
/// started before it died or after. Timeout tests use a live daemon so they
/// exercise deadlines rather than this distinct vanished outcome.
#[test]
fn waits_and_follows_end_when_their_daemon_vanishes() {
    let rig = Rig::new();
    rig.spawn_cat("doomed", &[]);
    let pid: i32 = std::fs::read_to_string(rig.path("doomed.pid"))
        .unwrap().trim().parse().unwrap();
    let spawn = |args: &[&str]| {
        rig.cmd(args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
    };
    let user_wait = ["events", "doomed", "--wait", "user.never", "-t", "3600"];
    let exit_wait = ["events", "doomed", "--wait", "session_exit", "-t", "3600"];
    let follow = ["events", "doomed", "--json"];
    let before = [spawn(&user_wait), spawn(&exit_wait), spawn(&follow)];
    pty_core::kill::process(pid, libc::SIGKILL).unwrap();
    let after = [spawn(&user_wait), spawn(&exit_wait), spawn(&follow)];
    for (i, child) in before.into_iter().chain(after).enumerate() {
        let out = finish_within(child, Duration::from_secs(3));
        let stderr = String::from_utf8_lossy(&out.stderr);
        let (code, message) = match i % 3 {
            0 | 1 => (2, "Session \"doomed\" vanished without a recorded exit.\n"),
            _ => (0, ""),
        };
        assert_eq!(out.status.code(), Some(code), "{i}: {out:?}");
        assert_eq!(stderr, message, "{i}");
    }
}

/// Regression from #201: lose a daemon only after the lifecycle waiter has
/// subscribed, without removing its registry entry or publishing an exit.
#[test]
fn lifecycle_wait_returns_promptly_when_a_live_daemon_vanishes() {
    let rig = Rig::new();
    rig.spawn_cat("vanishing", &[]);
    let pid: i32 = std::fs::read_to_string(rig.path("vanishing.pid"))
        .unwrap().trim().parse().unwrap();
    let mut child = rig.cmd(&[
        "events", "vanishing", "--wait", "session_exit", "--timeout", "3600",
    ]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    // The Linux watcher is installed after the follower's subscription-ready
    // handshake. Observe that installation rather than relying on a sleep.
    #[cfg(target_os = "linux")]
    {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            assert!(child.try_wait().unwrap().is_none(), "wait ended before daemon loss");
            let subscribed = std::fs::read_dir(format!("/proc/{}/fdinfo", child.id()))
                .unwrap()
                .filter_map(Result::ok)
                .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
                .any(|info| info.lines().any(|line| line.starts_with("inotify wd:")));
            if subscribed {
                break;
            }
            assert!(Instant::now() < deadline, "wait did not install its registry watch");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    // Match the original regression's bounded startup allowance on platforms
    // without Linux's observable inotify descriptor.
    #[cfg(not(target_os = "linux"))]
    std::thread::sleep(Duration::from_millis(400));
    assert!(child.try_wait().unwrap().is_none());
    pty_core::kill::process(pid, libc::SIGKILL).unwrap();
    let started = Instant::now();
    let out = finish_within(child, Duration::from_secs(2));
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Session \"vanishing\" vanished without a recorded exit.\n",
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    let metadata = rig.read_meta("vanishing").expect("vanished metadata must remain");
    assert!(metadata.get("exitCode").is_none(), "{metadata}");
    assert!(metadata.get("exitedAt").is_none(), "{metadata}");
    let events = std::fs::read_to_string(rig.path("vanishing.events.jsonl")).unwrap();
    assert!(parse_json_lines(&events).iter().all(|event| event["type"] != "session_exit"));
}

fn spawn_wait(rig: &Rig, name: &str, event_type: &str, json: bool) -> std::process::Child {
    let mut args = vec!["events", name, "--wait", event_type, "--timeout", "10"];
    if json {
        args.push("--json");
    }
    rig.cmd(&args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
}

#[test]
fn wait_replays_a_recorded_exit_before_subscription_in_json_and_text() {
    let rig = Rig::new();
    let event = json!({
        "session": "already-exited", "type": "session_exit",
        "ts": "2026-01-02T03:04:06.000Z", "exitCode": 17
    });
    rig.write_meta("already-exited", json!({
        "createdAt": "2026-01-01T00:00:00.000Z",
        "exitedAt": "2026-01-02T03:04:06.000Z", "exitCode": 17
    }));
    append_event(&rig, "already-exited", &event.to_string());
    for json in [true, false] {
        let out = finish_within(
            spawn_wait(&rig, "already-exited", "session_exit", json),
            Duration::from_secs(2),
        );
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        assert!(out.stderr.is_empty(), "{out:?}");
        let stdout = String::from_utf8(out.stdout).unwrap();
        if json {
            assert_eq!(parse_json_lines(&stdout), vec![event.clone()]);
        } else {
            assert_eq!(stdout.lines().count(), 1, "{stdout}");
            assert!(stdout.ends_with("] already-exited: exited (code 17)\n"), "{stdout}");
        }
    }
}

#[test]
fn wait_observes_exit_published_after_the_wait_process_starts() {
    let rig = Rig::new();
    rig.write_meta("exit-during-wait", json!({"createdAt": "2026-01-01T00:00:00.000Z"}));
    let child = spawn_wait(&rig, "exit-during-wait", "session_exit", true);
    // No readiness event is printed by --wait. Publishing after spawn covers
    // both a live notification and the subscribe/snapshot race without relying
    // on an arbitrary sleep to decide when the subscriber is ready.
    let event = json!({
        "session": "exit-during-wait", "type": "session_exit",
        "ts": "2026-01-02T03:04:06.000Z", "exitCode": 23
    });
    append_event(&rig, "exit-during-wait", &event.to_string());
    rig.write_meta("exit-during-wait", json!({
        "createdAt": "2026-01-01T00:00:00.000Z",
        "exitedAt": "2026-01-02T03:04:06.000Z", "exitCode": 23
    }));
    let out = finish_within(child, Duration::from_secs(2));
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(out.stderr.is_empty(), "{out:?}");
    assert_eq!(parse_json_lines(&String::from_utf8(out.stdout).unwrap()), vec![event]);
}

#[test]
fn wait_replays_arbitrary_exact_types_beyond_the_recent_tail() {
    let rig = Rig::new();
    for event_type in ["bell", "user.note", "display_name_change"] {
        rig.write_meta("retained", json!({"createdAt": "2026-01-01T00:00:00.000Z"}));
        std::fs::write(rig.path("retained.events.jsonl"), "").unwrap();
        append_event(&rig, "retained", &json!({
            "session": "retained", "type": format!("{event_type}.suffix"),
            "ts": "2026-01-02T03:04:05.000Z"
        }).to_string());
        let first = json!({
            "session": "retained", "type": event_type,
            "ts": "2026-01-02T03:04:06.000Z", "data": {"sequence": 1}
        });
        append_event(&rig, "retained", &first.to_string());
        let latest = json!({
            "session": "retained", "type": event_type,
            "ts": "2026-01-02T03:04:06.500Z", "data": {"sequence": 2}
        });
        append_event(&rig, "retained", &latest.to_string());
        for sequence in 0..60 {
            append_event(&rig, "retained", &json!({
                "session": "retained", "type": "user.distractor",
                "ts": "2026-01-02T03:04:07.000Z", "data": {"sequence": sequence}
            }).to_string());
        }
        let before = std::fs::read(rig.path("retained.events.jsonl")).unwrap();
        let out = finish_within(
            spawn_wait(&rig, "retained", event_type, true),
            Duration::from_secs(2),
        );
        assert_eq!(out.status.code(), Some(0), "{event_type}: {out:?}");
        assert!(out.stderr.is_empty(), "{out:?}");
        assert_eq!(parse_json_lines(&String::from_utf8(out.stdout).unwrap()), vec![latest]);
        assert_eq!(std::fs::read(rig.path("retained.events.jsonl")).unwrap(), before);
    }
}

#[test]
fn wait_ignores_previous_generation_events_and_accepts_the_creation_boundary() {
    for event_type in ["user.note", "session_exit"] {
        let rig = Rig::new();
        rig.spawn_cat("reused", &[]);
        let mut metadata = rig.read_meta("reused").unwrap();
        metadata["createdAt"] = json!("2026-01-02T03:04:06.000Z");
        rig.write_meta("reused", metadata);
        std::fs::write(rig.path("reused.events.jsonl"), "").unwrap();
        append_event(&rig, "reused", &json!({
            "session": "reused", "type": event_type,
            "ts": "2026-01-02T03:04:05.999Z", "exitCode": 99
        }).to_string());
        let out = finish_within(
            rig.cmd(&["events", "reused", "--wait", event_type, "-t", "0.2", "--json"])
                .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap(),
            Duration::from_secs(2),
        );
        assert_eq!(out.status.code(), Some(1), "{event_type}: {out:?}");
        assert!(out.stdout.is_empty(), "{out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            format!("Timed out after 0.2s waiting for \"{event_type}\" event.\n"),
        );
        let current = json!({
            "session": "reused", "type": event_type,
            "ts": "2026-01-02T03:04:06.000Z", "exitCode": 0
        });
        append_event(&rig, "reused", &current.to_string());
        let out = finish_within(
            spawn_wait(&rig, "reused", event_type, true),
            Duration::from_secs(2),
        );
        assert_eq!(out.status.code(), Some(0), "{event_type}: {out:?}");
        assert!(out.stderr.is_empty(), "{out:?}");
        assert_eq!(parse_json_lines(&String::from_utf8(out.stdout).unwrap()), vec![current]);
    }
}

#[test]
fn wait_reports_registry_removal_as_vanished_not_timeout() {
    for event_type in ["user.never", "session_exit"] {
        let rig = Rig::new();
        rig.spawn_cat("removed-wait", &[]);
        let mut child = spawn_wait(&rig, "removed-wait", event_type, true);
        // Unlike followers, waits have no stdout readiness marker. Keep the
        // process alive through several follower polling intervals before
        // removing the registry entry; all completion waits remain bounded.
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            assert!(child.try_wait().unwrap().is_none(), "wait ended before removal");
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::remove_file(rig.path("removed-wait.json")).unwrap();
        let out = finish_within(child, Duration::from_secs(2));
        assert_eq!(out.status.code(), Some(2), "{event_type}: {out:?}");
        assert!(out.stdout.is_empty(), "{out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            "Session \"removed-wait\" vanished without a recorded exit.\n",
        );
    }
}

#[test]
fn idle_follow_exits_zero_when_its_stdout_reader_closes_in_json_and_text() {
    let rig = Rig::new();
    for json in [true, false] {
        rig.write_meta("idle-pipe", json!({}));
        let mut args = vec!["events", "idle-pipe"];
        if json {
            args.push("--json");
        }
        let mut child = rig.cmd(&args)
            .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            tx.send((reader, line)).ok();
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        let (pipe, line) = loop {
            append_event(&rig, "idle-pipe", &json!({
                "session": "idle-pipe", "type": "user.ready",
                "ts": "2026-01-02T03:04:05.000Z"
            }).to_string());
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(value) => break value,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
                result => {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    reader.join().unwrap();
                    panic!("idle follower never became ready: {result:?}");
                }
            }
        };
        reader.join().unwrap();
        if json {
            assert_eq!(parse_json_lines(&line)[0]["type"], "user.ready");
        } else {
            assert!(line.ends_with("] idle-pipe: user.ready\n"), "{line}");
        }
        // No event is appended after readiness: a broken-pipe-on-write-only
        // implementation would hang here instead of observing reader closure.
        drop(pipe);
        let out = finish_within(child, Duration::from_secs(2));
        assert_eq!(out.status.code(), Some(0), "json={json}: {out:?}");
        assert!(out.stderr.is_empty(), "{out:?}");
    }
}
