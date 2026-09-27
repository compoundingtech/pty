//! `queryStats` (`client.ts:344-389`): STATUS request/response, the 2 s
//! budget, and the invalid-JSON text.

mod common;

use std::time::Duration;

use common::*;
use pty_client::{ClientError, query_stats, query_stats_with_timeout, query_status_json};
use pty_core::protocol::{MessageType, encode_status_response};
use pty_core::stats::ConnectionStats;

const T: Duration = Duration::from_secs(5);

const BODY: &str = r#"{"name":"demo","terminal":{"cols":80,"rows":24,"cursorX":1,"cursorY":2,"scrollbackUsed":24,"scrollbackCapacity":10024},"process":{"alive":true,"exitCode":null,"pid":123,"resources":{"rssKb":1024,"cpuPercent":0.5}},"daemon":{"pid":456,"resources":null},"clients":{"total":2,"attached":1,"readOnly":1,"connections":[{"role":"writable","rows":24,"cols":80,"lastRequestSequence":1,"constrains":{"rows":true,"cols":true}},{"role":"readonly","constrains":{"rows":false,"cols":false}}]},"modes":{"sgrMouse":false,"cursorHidden":false,"kittyKeyboard":false,"kittyKeyboardFlags":[]},"uptimeSeconds":10,"createdAt":"2026-07-31T00:00:00.000Z"}"#;

fn stats_daemon(reply: Option<&'static str>) -> FakeDaemon {
    let d = FakeDaemon::bind("stats");
    let listener = d.listener.try_clone().unwrap();
    std::thread::spawn(move || {
        use std::io::Write;
        let (mut s, _) = listener.accept().unwrap();
        let first = packets(&read_chunk(&mut s));
        assert_eq!(first[0].type_, MessageType::Status);
        assert!(first[0].payload.is_empty());
        if let Some(body) = reply {
            s.write_all(&encode_status_response(body)).unwrap();
        }
        let _ = read_packets_until_eof(&mut s, T);
    });
    d
}

/// node: tests/stats-cli.test.ts:127-151 (the JSON is the daemon's, verbatim)
#[test]
fn status_round_trip_verbatim_and_typed() {
    let d = stats_daemon(Some(BODY));
    let raw = query_status_json(&d.name, T).unwrap();
    assert_eq!(raw, BODY);
    let d = stats_daemon(Some(BODY));
    let stats = query_stats(&d.name).unwrap();
    assert_eq!(stats.name, "demo");
    assert_eq!(stats.terminal.scrollback_capacity, 10024);
    assert_eq!(stats.clients.total, 2);
    assert_eq!(stats.uptime_seconds, Some(10));
    assert!(matches!(
        stats.clients.connections.as_deref(),
        Some([
            ConnectionStats::Writable { .. },
            ConnectionStats::Readonly { .. }
        ])
    ));
    assert_eq!(serde_json::to_string(&stats).unwrap(), BODY);
}

/// node: client.ts:349-352
#[test]
fn no_reply_within_the_budget_is_a_timeout() {
    let d = stats_daemon(None);
    let err = query_stats_with_timeout(&d.name, Duration::from_millis(300)).unwrap_err();
    assert_eq!(err, ClientError::StatsTimeout(d.name.clone()));
    assert_eq!(
        err.to_string(),
        format!("Timeout querying stats for \"{}\"", d.name)
    );
}

/// node: client.ts:365-370
#[test]
fn invalid_json_is_reported() {
    let d = stats_daemon(Some("not json"));
    let err = query_stats(&d.name).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("Invalid stats response from \"{}\"", d.name)
    );
}

/// node: client.ts:377-383
#[test]
fn missing_session_is_not_found() {
    test_root();
    let err = query_stats("absent").unwrap_err();
    assert_eq!(
        err.to_string(),
        "Session \"absent\" not found or not running."
    );
}
