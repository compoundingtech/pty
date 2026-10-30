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
    rig.write_meta("query", json!({}));
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
    rig.write_meta("s", json!({"displayName": "Friendly"}));
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
    rig.write_meta("w", json!({}));
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
    rig.write_meta("deadline", json!({}));
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
