//! `list_sessions` liveness exactly as Node classifies it, from fabricated
//! registry files; reference resolution with the ambiguity text; the
//! process helpers; and a cross-check against the Node `pty list --json`.

mod registry_support;

use std::os::unix::net::UnixListener;
use std::time::Duration;

use pty_core::registry::{self, SessionMetadata, SessionStatus};
use registry_support::{DEAD_PID, node_pty, root, run_node_pty, stale_socket, unique_name};
use serde_json::{Value, json};

fn write_json(name: &str, value: Value) {
    std::fs::write(
        registry::metadata_path(name),
        serde_json::to_string_pretty(&value).unwrap(),
    )
    .unwrap();
}

fn find(name: &str) -> registry::SessionInfo {
    registry::list_sessions()
        .into_iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("session {name} not listed"))
}

fn base(name: &str) -> Value {
    json!({
        "command": "cat", "args": [], "displayCommand": "cat", "cwd": "/tmp",
        "createdAt": "2026-04-05T10:15:03.000Z", "displayName": format!("label-{name}")
    })
}

/// node: tests/list-filters.test.ts:119-131 (vanished: metadata only)
#[test]
fn json_only_with_no_exit_record_is_vanished() {
    let _ = root();
    let name = unique_name("van");
    write_json(&name, base(&name));
    let s = find(&name);
    assert_eq!(s.status, SessionStatus::Vanished);
    assert_eq!(s.pid, None);
    assert!(s.is_gone());
    let m = s.metadata.unwrap();
    assert_eq!(m.exit_code, None);
    assert_eq!(m.exited_at, None);
}

/// node: tests/list-filters.test.ts:147-160 (exited: exit record present)
#[test]
fn json_only_with_exit_record_is_exited() {
    let _ = root();
    let name = unique_name("exd");
    let mut v = base(&name);
    v["exitCode"] = json!(0);
    v["exitedAt"] = json!("2026-04-05T10:16:03.000Z");
    write_json(&name, v);
    let s = find(&name);
    assert_eq!(s.status, SessionStatus::Exited);
    assert_eq!(s.pid, None);
    assert_eq!(s.metadata.unwrap().exit_code, Some(0));
}

/// node: tests/list-filters.test.ts:170-192 (live pid, missing socket -> running)
#[test]
fn json_with_live_pid_and_no_socket_is_running_and_untouched() {
    let _ = root();
    let name = unique_name("livepid");
    std::fs::write(registry::pid_path(&name), std::process::id().to_string()).unwrap();
    let mut v = base(&name);
    v["createdAt"] = json!("2020-01-01T00:00:00.000Z");
    write_json(&name, v);
    let s = find(&name);
    assert_eq!(s.status, SessionStatus::Running);
    assert_eq!(s.pid, Some(std::process::id() as i32));
    assert!(
        registry::metadata_path(&name).exists(),
        "listing never unlinks"
    );
    assert!(registry::pid_path(&name).exists());
}

/// node: tests/list-filters.test.ts:194-209 (dead pid, old metadata -> vanished, kept)
#[test]
fn json_with_dead_pid_is_vanished_and_kept() {
    let _ = root();
    let name = unique_name("deadpid");
    std::fs::write(registry::pid_path(&name), "2147483647").unwrap();
    write_json(&name, base(&name));
    let s = find(&name);
    assert_eq!(s.status, SessionStatus::Vanished);
    assert_eq!(s.pid, None);
    assert!(registry::pid_path(&name).exists());
    assert!(registry::metadata_path(&name).exists());
}

/// A socket whose pid is dead but which answers a connect is alive.
///
/// node: src/sessions.ts:934-947; tests/list-live-session-race.test.ts
#[test]
fn reachable_socket_overrides_a_dead_pid() {
    let _ = root();
    let name = unique_name("sockok");
    write_json(&name, base(&name));
    std::fs::write(registry::pid_path(&name), DEAD_PID.to_string()).unwrap();
    let _listener = UnixListener::bind(registry::socket_path(&name)).unwrap();
    let s = find(&name);
    assert_eq!(s.status, SessionStatus::Running);
    assert_eq!(
        s.pid,
        Some(DEAD_PID),
        "Node reports the pid it read even when the socket rescued it"
    );
    assert!(
        registry::socket_path(&name).exists(),
        "listing never unlinks a socket"
    );
}

/// A stale socket with a dead pid and an exit record is exited; without
/// one, vanished.
///
/// node: src/sessions.ts:948-961
#[test]
fn stale_socket_with_dead_pid_reports_the_retained_record() {
    let _ = root();
    let name = unique_name("stalesock");
    stale_socket(&registry::socket_path(&name));
    std::fs::write(registry::pid_path(&name), DEAD_PID.to_string()).unwrap();
    write_json(&name, base(&name));
    assert_eq!(find(&name).status, SessionStatus::Vanished);
    let mut v = base(&name);
    v["exitCode"] = json!(3);
    v["exitedAt"] = json!("2026-04-05T10:16:03.000Z");
    write_json(&name, v);
    let s = find(&name);
    assert_eq!(s.status, SessionStatus::Exited);
    assert_eq!(s.pid, None);
    assert!(registry::socket_path(&name).exists());
}

/// A stale socket, a dead pid and no metadata is not listed at all.
///
/// node: src/sessions.ts:948-961
#[test]
fn stale_socket_with_dead_pid_and_no_metadata_is_omitted() {
    let _ = root();
    let name = unique_name("nometa");
    stale_socket(&registry::socket_path(&name));
    std::fs::write(registry::pid_path(&name), DEAD_PID.to_string()).unwrap();
    assert!(registry::list_sessions().iter().all(|s| s.name != name));
}

/// A socket with an unreadable pid is reported running defensively, with
/// `metadata: None` when there is no record.
///
/// node: src/sessions.ts:962-971
#[test]
fn socket_with_unreadable_pid_is_running_defensively() {
    let _ = root();
    let name = unique_name("nopid");
    stale_socket(&registry::socket_path(&name));
    let s = find(&name);
    assert_eq!(s.status, SessionStatus::Running);
    assert_eq!(s.pid, None);
    assert!(s.metadata.is_none());
    assert_eq!(s.socket_path, registry::socket_path(&name));

    // With a garbage pid file the same holds.
    std::fs::write(registry::pid_path(&name), "not a pid").unwrap();
    write_json(&name, base(&name));
    let s = find(&name);
    assert_eq!(s.status, SessionStatus::Running);
    assert!(s.metadata.is_some());
}

/// A live socket with `exitedAt` already written is exited (the daemon's
/// cleanup delay).
///
/// node: src/sessions.ts:940-947
#[test]
fn live_socket_with_exit_record_is_exited() {
    let _ = root();
    let name = unique_name("exiting");
    let _listener = UnixListener::bind(registry::socket_path(&name)).unwrap();
    std::fs::write(registry::pid_path(&name), std::process::id().to_string()).unwrap();
    let mut v = base(&name);
    v["exitCode"] = json!(7);
    v["exitedAt"] = json!("2026-04-05T10:16:03.000Z");
    write_json(&name, v);
    let s = find(&name);
    assert_eq!(s.status, SessionStatus::Exited);
    assert_eq!(s.pid, Some(std::process::id() as i32));
}

/// `daemonPid` counts only with a matching process start token.
///
/// node: src/sessions.ts:2086-2095; tests/recovery.test.ts:129-170
#[test]
fn daemon_pid_needs_a_matching_start_token() {
    if registry_support::skip_without_ps("lstart=") {
        return;
    }
    let _ = root();
    let me = std::process::id() as i32;
    let token = registry::read_process_start_token(me).expect("own start token");
    assert!(token.starts_with("linux:") || token.starts_with("darwin:"));
    assert_eq!(registry::read_process_start_token(0), None);
    assert_eq!(registry::read_process_start_token(DEAD_PID), None);

    let name = unique_name("token");
    let mut v = base(&name);
    v["daemonPid"] = json!(me);
    v["recovery"] = json!({"protocol": 1, "processStartToken": token});
    write_json(&name, v.clone());
    assert_eq!(registry::read_pid(&name), Some(me));
    let s = find(&name);
    assert_eq!(s.status, SessionStatus::Running);
    assert_eq!(s.pid, Some(me));

    v["recovery"] = json!({"protocol": 1, "processStartToken": "linux:0"});
    write_json(&name, v.clone());
    assert_eq!(registry::read_pid(&name), None);
    assert_eq!(find(&name).status, SessionStatus::Vanished);

    v.as_object_mut().unwrap().remove("recovery");
    write_json(&name, v);
    assert_eq!(
        registry::read_pid(&name),
        None,
        "daemonPid alone is never trusted"
    );

    // The sidecar pid always wins.
    std::fs::write(registry::pid_path(&name), "12abc").unwrap();
    assert_eq!(registry::read_session_pid(&name), Some(12));
    assert_eq!(registry::read_pid(&name), Some(12));
}

/// A destructive signal target binds to the real OS start token of the live
/// daemon, never to a stale one.
#[test]
fn signal_target_binds_the_live_os_start_token() {
    if registry_support::skip_without_ps("lstart=") {
        return;
    }
    let _ = root();
    let me = std::process::id() as i32;
    let name = unique_name("signal");
    let live = SessionMetadata {
        daemon_pid: Some(me),
        daemon_start_token: registry::read_process_start_token(me),
        ..Default::default()
    };
    assert_eq!(
        registry::read_signal_target_with(&name, Some(&live)),
        Some(me)
    );

    let stale = SessionMetadata {
        daemon_start_token: Some("linux:0".to_string()),
        ..live
    };
    assert_eq!(registry::read_signal_target_with(&name, Some(&stale)), None);
}

/// Temporaries of in-flight atomic writes are never listed.
#[test]
fn tmp_files_are_skipped() {
    let root = root();
    let name = unique_name("tmp");
    std::fs::write(
        root.join(format!("{name}.json.tmp.1.abcdef0123456789")),
        "{}",
    )
    .unwrap();
    assert!(
        registry::list_sessions()
            .iter()
            .all(|s| !s.name.starts_with(&name))
    );
}

/// Sorted by name; `get_session` resolves ids first, unique display names
/// second, and fails closed on ambiguity with the id list.
///
/// node: src/sessions.ts:1351-1363; tests/display-name.test.ts:340-367
#[test]
fn get_session_resolution_and_ambiguity() {
    let _ = root();
    let a = unique_name("amb-a");
    let b = unique_name("amb-b");
    let c = unique_name("amb-c");
    let d = unique_name("amb-d");
    let mut va = base(&a);
    va["displayName"] = json!("shared");
    let mut vb = base(&b);
    vb["displayName"] = json!("shared");
    let mut vc = base(&c);
    vc["displayName"] = json!(a.clone()); // a display name equal to another id
    write_json(&a, va);
    write_json(&b, vb);
    write_json(&c, vc);
    write_json(&d, base(&d));

    let names: Vec<String> = registry::list_sessions()
        .into_iter()
        .map(|s| s.name)
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);

    assert_eq!(
        registry::get_session(&a).unwrap().unwrap().name,
        a,
        "exact id wins over c's display name"
    );
    assert_eq!(
        registry::get_session(&format!("label-{d}"))
            .unwrap()
            .unwrap()
            .name,
        d,
        "a unique display name resolves"
    );
    assert_eq!(registry::get_session("no-such-thing").unwrap(), None);
    let err = registry::get_session("shared").unwrap_err();
    assert_eq!(
        err,
        format!(
            "Session reference \"shared\" is ambiguous. Matching stable session IDs:\n  {a}\n  {b}\nUse a stable session ID instead."
        )
    );
    assert_eq!(registry::resolve_ref("shared"), None);
    assert_eq!(registry::resolve_ref(&a).as_deref(), Some(a.as_str()));
    assert!(registry::all_session_names().contains(&b));
    assert!(
        registry::get_session_by_name(&format!("label-{d}")).is_none(),
        "by-name never uses display names"
    );
    assert!(registry::session_exists(&a));
}

/// node: src/sessions.ts:801-817, 2097-2127
#[test]
fn process_helpers() {
    assert!(registry::pid_alive(std::process::id() as i32));
    assert!(!registry::pid_alive(DEAD_PID));
    assert!(!registry::pid_alive(0));
    assert!(!registry::pid_alive(-1));
    assert!(
        registry::pid_alive(1),
        "pid 1 exists; EPERM counts as alive"
    );
    if !registry_support::skip_without_ps("stat=") {
        assert!(!registry::has_process_exited_for_reap(
            std::process::id() as i32
        ));
    }
    assert!(registry::has_process_exited_for_reap(DEAD_PID));
    let start = std::time::Instant::now();
    assert!(registry::wait_for_process_exit(
        DEAD_PID,
        Duration::from_secs(1)
    ));
    assert!(start.elapsed() < Duration::from_millis(500));
    assert!(!registry::wait_for_process_exit(
        std::process::id() as i32,
        Duration::from_millis(120)
    ));

    // A zombie child counts as exited.
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id() as i32;
    assert!(
        registry::wait_for_process_exit(pid, Duration::from_secs(5)),
        "zombie must read as exited"
    );
    let _ = child.wait();
}

/// The socket probe respects one shared budget.
///
/// node: tests/list-liveness-budget.test.ts:60-78
#[test]
fn socket_probe_budget_is_shared() {
    let root = root();
    let paths: Vec<_> = (0..100)
        .map(|i| root.join(format!("probe-{i}.sock")))
        .collect();
    let start = std::time::Instant::now();
    let results = registry::probe_sockets_within_budget(&paths, Duration::from_millis(25));
    assert!(
        start.elapsed() < Duration::from_millis(250),
        "{:?}",
        start.elapsed()
    );
    assert!(results.values().all(|r| !r));
    assert!(!registry::socket_reachable(&root.join("probe-none.sock")));
}

/// Field-for-field agreement with Node's `pty list --json` on a directory
/// of fabricated running / exited / vanished records.
///
/// node: src/cli.ts:2292-2305
#[test]
fn matches_node_list_json_on_a_mixed_directory() {
    let Some(bin) = node_pty() else {
        eprintln!("skipping: Node pty 0.12 not on PATH");
        return;
    };
    let root = root();
    // Both sides read the same directory; only the `m-` records are compared.
    write_json("m-vanished", base("m-vanished"));
    let mut exited = base("m-exited");
    exited["exitCode"] = json!(3);
    exited["exitedAt"] = json!("2026-04-05T10:16:03.000Z");
    write_json("m-exited", exited);
    std::fs::write(
        registry::pid_path("m-running"),
        std::process::id().to_string(),
    )
    .unwrap();
    write_json("m-running", base("m-running"));
    std::fs::write(registry::pid_path("m-dead"), DEAD_PID.to_string()).unwrap();
    write_json("m-dead", base("m-dead"));
    let _listener = UnixListener::bind(registry::socket_path("m-sockonly")).unwrap();

    let out = run_node_pty(&bin, &root, &["list", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let node: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
    let node: Vec<(String, String, Option<i32>)> = node
        .into_iter()
        .filter(|s| s["name"].as_str().unwrap().starts_with("m-"))
        .map(|s| {
            (
                s["name"].as_str().unwrap().to_string(),
                s["status"].as_str().unwrap().to_string(),
                s["pid"].as_i64().map(|p| p as i32),
            )
        })
        .collect();
    // The CLI orders by `displayName ?? name`; the registry by name. Compare
    // the records, not the order.
    let mut node = node;
    node.sort();
    let rust: Vec<(String, String, Option<i32>)> = registry::list_sessions()
        .into_iter()
        .filter(|s| s.name.starts_with("m-"))
        .map(|s| (s.name, s.status.as_str().to_string(), s.pid))
        .collect();
    assert_eq!(rust, node);
    let statuses: Vec<&str> = rust.iter().map(|(_, s, _)| s.as_str()).collect();
    assert_eq!(
        statuses,
        ["vanished", "exited", "running", "running", "vanished"]
    );
}

#[test]
fn session_metadata_defaults_for_sparse_records() {
    let _ = root();
    let name = unique_name("sparse");
    write_json(&name, json!({"name": name}));
    let m: SessionMetadata = registry::read_metadata(&name).unwrap();
    assert_eq!(m.command, "");
    assert_eq!(m.args, Vec::<String>::new());
    assert_eq!(m.extra.get("name"), Some(&json!(name)));
    assert_eq!(find(&name).status, SessionStatus::Vanished);
}
