//! Identity-bound registry listing, historical records, reference resolution,
//! process helpers, and compatibility with identity-proven Node records.

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

fn bind_live(value: &mut Value) {
    let pid = std::process::id() as i32;
    value["daemonPid"] = json!(pid);
    value["daemonStartToken"] = json!(registry::read_process_start_token(pid).unwrap());
    value["daemonBootId"] = json!(registry::read_boot_id().unwrap());
}

/// Metadata without a named PID cannot prove its owner has gone.
#[test]
fn json_only_with_no_exit_record_is_unknown_not_gone() {
    let _ = root();
    let name = unique_name("van");
    write_json(&name, base(&name));
    let s = find(&name);
    assert_eq!(s.status, SessionStatus::Running);
    assert_eq!(s.pid, None);
    assert!(!s.is_gone());
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
    v["daemonPid"] = json!(DEAD_PID);
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
    bind_live(&mut v);
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

/// A reachable socket cannot rescue an absent daemon identity.
#[test]
fn reachable_socket_never_overrides_a_dead_pid() {
    let _ = root();
    let name = unique_name("sockok");
    write_json(&name, base(&name));
    std::fs::write(registry::pid_path(&name), DEAD_PID.to_string()).unwrap();
    let _listener = UnixListener::bind(registry::socket_path(&name)).unwrap();
    let s = find(&name);
    assert_eq!(s.status, SessionStatus::Vanished);
    assert_eq!(s.pid, None);
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

/// A stale socket with a dead PID is listed as vanished without metadata.
#[test]
fn stale_socket_with_dead_pid_and_no_metadata_is_vanished() {
    let _ = root();
    let name = unique_name("nometa");
    stale_socket(&registry::socket_path(&name));
    std::fs::write(registry::pid_path(&name), DEAD_PID.to_string()).unwrap();
    assert_eq!(find(&name).status, SessionStatus::Vanished);
}

/// Missing PID evidence retains an unknown row without claiming loss.
#[test]
fn socket_with_unreadable_pid_is_unknown_not_gone() {
    let _ = root();
    let name = unique_name("nopid");
    stale_socket(&registry::socket_path(&name));
    let s = find(&name);
    assert_eq!(s.status, SessionStatus::Running);
    assert_eq!(s.identity, registry::DaemonIdentityCheck::Unknown);
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
    bind_live(&mut v);
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
    v["daemonBootId"] = json!(registry::read_boot_id().unwrap());
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

    // The raw sidecar remains inspectable, but never overrides identity.
    std::fs::write(registry::pid_path(&name), "12abc").unwrap();
    assert_eq!(registry::read_session_pid(&name), Some(12));
    assert_eq!(registry::read_pid(&name), None);
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
        daemon_boot_id: registry::read_boot_id(),
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
    std::fs::write(registry::pid_path("m-vanished"), DEAD_PID.to_string()).unwrap();
    let mut exited = base("m-exited");
    exited["exitCode"] = json!(3);
    exited["exitedAt"] = json!("2026-04-05T10:16:03.000Z");
    exited["daemonPid"] = json!(DEAD_PID);
    write_json("m-exited", exited);
    std::fs::write(
        registry::pid_path("m-running"),
        std::process::id().to_string(),
    )
    .unwrap();
    let mut running = base("m-running");
    bind_live(&mut running);
    write_json("m-running", running);
    std::fs::write(registry::pid_path("m-dead"), DEAD_PID.to_string()).unwrap();
    write_json("m-dead", base("m-dead"));
    let _listener = UnixListener::bind(registry::socket_path("m-sockonly")).unwrap();
    let mut sockonly = base("m-sockonly");
    bind_live(&mut sockonly);
    write_json("m-sockonly", sockonly);
    std::fs::write(
        registry::pid_path("m-sockonly"),
        std::process::id().to_string(),
    )
    .unwrap();

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
    assert_eq!(find(&name).status, SessionStatus::Running);
}

fn strict_root() -> std::path::PathBuf {
    let dir = root().join(unique_name("strict"));
    std::fs::create_dir(&dir).unwrap();
    dir
}

#[test]
fn strict_clean_inventory_matches_default_and_preserves_root() {
    let dir = strict_root();
    std::fs::write(
        dir.join("retained.json"),
        format!(r#"{{"exitCode":0,"daemonPid":{DEAD_PID}}}"#),
    )
    .unwrap();
    std::fs::write(dir.join("orphan.pid"), DEAD_PID.to_string()).unwrap();
    std::fs::write(dir.join("ignored.tmp.bad.json"), b"not json").unwrap();
    let socket = dir.join("socket.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let mut socket_record = base("socket");
    bind_live(&mut socket_record);
    std::fs::write(dir.join("socket.json"), socket_record.to_string()).unwrap();
    let options = registry::ListOptions::default();
    let inventory = registry::list_sessions_strict_in(&dir, &options);
    assert!(inventory.complete);
    assert!(inventory.errors.is_empty());
    assert_eq!(inventory.root, dir);
    assert_eq!(inventory.entries, registry::list_sessions_in(&dir, &options));
    assert_eq!(
        inventory.entries.iter().map(|s| (s.name.as_str(), s.status)).collect::<Vec<_>>(),
        vec![("retained", SessionStatus::Exited), ("socket", SessionStatus::Running)]
    );
    drop(listener);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn strict_retains_proven_ended_sockets_alongside_a_healthy_session() {
    let dir = strict_root();
    let live = UnixListener::bind(dir.join("live.sock")).unwrap();
    let mut live_record = base("live");
    bind_live(&mut live_record);
    std::fs::write(dir.join("live.json"), live_record.to_string()).unwrap();
    for name in ["dead-pid", "recorded-exit"] {
        stale_socket(&dir.join(format!("{name}.sock")));
    }
    std::fs::write(dir.join("dead-pid.pid"), DEAD_PID.to_string()).unwrap();
    std::fs::write(
        dir.join("recorded-exit.json"),
        format!(r#"{{"exitCode":7,"daemonPid":{DEAD_PID}}}"#),
    )
    .unwrap();
    let options = registry::ListOptions::default();
    let inventory = registry::list_sessions_strict_in(&dir, &options);
    assert!(inventory.complete);
    assert!(inventory.errors.is_empty());
    assert_eq!(
        inventory
            .entries
            .iter()
            .map(|session| (session.name.as_str(), session.status, session.pid))
            .collect::<Vec<_>>(),
        vec![
            ("dead-pid", SessionStatus::Vanished, None),
            (
                "live",
                SessionStatus::Running,
                Some(std::process::id() as i32)
            ),
            ("recorded-exit", SessionStatus::Exited, None),
        ]
    );
    assert_eq!(
        inventory.entries[2].metadata.as_ref().unwrap().exit_code,
        Some(7)
    );
    // Tolerant and strict listings use the same identity boundary.
    assert_eq!(
        registry::list_sessions_in(&dir, &options),
        inventory.entries
    );
    drop(live);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn strict_refused_socket_without_dead_pid_is_incomplete() {
    let dir = strict_root();
    let socket = dir.join("unknown.sock");
    stale_socket(&socket);
    let options = registry::ListOptions::default();
    let inventory = registry::list_sessions_strict_in(&dir, &options);
    assert!(!inventory.complete);
    assert_eq!(
        inventory.errors[0].kind,
        registry::InventoryErrorKind::IdentityUnknown
    );
    assert_eq!(inventory.errors[0].path, dir.join("unknown.json"));
    assert_eq!(inventory.entries[0].status, SessionStatus::Running);
    // Recorded exit evidence does not prove an unknown process is gone.
    std::fs::write(dir.join("unknown.json"), br#"{"exitCode":0}"#).unwrap();
    assert!(!registry::list_sessions_strict_in(&dir, &options).complete);
    std::fs::write(dir.join("unknown.pid"), DEAD_PID.to_string()).unwrap();
    let inventory = registry::list_sessions_strict_in(&dir, &options);
    assert!(inventory.complete);
    assert!(inventory.errors.is_empty());
    assert_eq!(inventory.entries[0].status, SessionStatus::Exited);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn strict_missing_root_is_incomplete_without_creating_it() {
    let dir = root().join(unique_name("missing"));
    let options = registry::ListOptions::default();
    let inventory = registry::list_sessions_strict_in(&dir, &options);
    assert!(!inventory.complete);
    assert!(inventory.entries.is_empty());
    assert_eq!(
        inventory.errors[0].kind,
        registry::InventoryErrorKind::RootMissing
    );
    assert_eq!(inventory.errors[0].path, dir);
    assert!(!dir.exists());
    assert!(registry::list_sessions_in(&dir, &options).is_empty());
}

#[test]
fn strict_checks_malformed_metadata_and_orphan_pids_without_losing_good_records() {
    use registry::InventoryErrorKind::{MetadataMalformed, PidMalformed};
    let dir = strict_root();
    std::fs::write(
        dir.join("good.json"),
        format!(r#"{{"daemonPid":{DEAD_PID}}}"#),
    )
    .unwrap();
    for (name, content) in [
        ("broken", "{"),
        ("array", "[]"),
        ("wrongtype", r#"{"command":42}"#),
    ] {
        std::fs::write(dir.join(format!("{name}.json")), content).unwrap();
    }
    for (name, content) in [
        ("empty", ""),
        ("zero", "0"),
        ("negative", "-1"),
        ("overflow", "2147483648"),
        ("junk", "42junk"),
    ] {
        std::fs::write(dir.join(format!("{name}.pid")), content).unwrap();
    }
    // A legacy suffix remains valid for classification, but not strict evidence.
    let mut legacy = base("legacy");
    bind_live(&mut legacy);
    std::fs::write(dir.join("legacy.json"), legacy.to_string()).unwrap();
    std::fs::write(
        dir.join("legacy.pid"),
        format!("{}junk", std::process::id()),
    )
    .unwrap();
    let options = registry::ListOptions::default();
    let inventory = registry::list_sessions_strict_in(&dir, &options);
    assert!(!inventory.complete);
    assert_eq!(inventory.entries, registry::list_sessions_in(&dir, &options));
    assert_eq!(
        inventory.entries.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        vec!["good", "legacy"]
    );
    assert_eq!(inventory.entries[1].status, SessionStatus::Running);
    let actual: Vec<_> = inventory
        .errors
        .iter()
        .map(|error| (error.path.file_name().unwrap().to_str().unwrap(), error.kind))
        .collect();
    assert_eq!(
        actual,
        vec![
            ("array.json", MetadataMalformed),
            ("broken.json", MetadataMalformed),
            ("empty.pid", PidMalformed),
            ("junk.pid", PidMalformed),
            ("legacy.pid", PidMalformed),
            ("negative.pid", PidMalformed),
            ("overflow.pid", PidMalformed),
            ("wrongtype.json", MetadataMalformed),
            ("zero.pid", PidMalformed),
        ]
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn strict_reports_unreadable_companions_and_non_utf8_entries() {
    use std::os::unix::ffi::OsStringExt;
    let dir = strict_root();
    std::fs::create_dir(dir.join("bad.json")).unwrap();
    std::fs::create_dir(dir.join("orphan.pid")).unwrap();
    let non_utf8 = dir.join(std::ffi::OsString::from_vec(b"bad-\xff.json".to_vec()));
    let has_non_utf8_entry = match std::fs::write(&non_utf8, b"{}") {
        Ok(()) => true,
        // APFS requires Unicode-normalizable names and rejects this fixture with EILSEQ.
        Err(error) if error.raw_os_error() == Some(libc::EILSEQ) => {
            eprintln!(
                "skipping non-UTF-8 entry sub-case: filesystem cannot represent non-UTF-8 names ({error})"
            );
            false
        }
        Err(error) => panic!("creating non-UTF-8 fixture: {error}"),
    };
    let inventory = registry::list_sessions_strict_in(&dir, &registry::ListOptions::default());
    assert!(!inventory.complete);
    assert!(inventory.entries.is_empty());
    for (path, kind) in [
        (
            dir.join("bad.json"),
            registry::InventoryErrorKind::MetadataUnreadable,
        ),
        (
            dir.join("orphan.pid"),
            registry::InventoryErrorKind::PidUnreadable,
        ),
    ] {
        assert!(
            inventory
                .errors
                .iter()
                .any(|error| error.path == path && error.kind == kind)
        );
    }
    if has_non_utf8_entry {
        assert!(inventory.errors.iter().any(|error| {
            error.path == non_utf8 && error.kind == registry::InventoryErrorKind::EntryUnreadable
        }));
        let encoded = serde_json::to_value(&inventory.errors).unwrap();
        assert!(
            encoded
                .as_array()
                .unwrap()
                .iter()
                .any(|error| error["kind"] == "entry-unreadable")
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn strict_reports_permission_denied_root_and_sidecars() {
    use std::os::unix::fs::PermissionsExt;
    // Root bypasses mode bits, so EACCES cannot be proved under that uid.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let dir = strict_root();
    let options = registry::ListOptions::default();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o0)).unwrap();
    let inventory = registry::list_sessions_strict_in(&dir, &options);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!inventory.complete);
    assert_eq!(
        inventory.errors[0].kind,
        registry::InventoryErrorKind::RootUnreadable
    );
    for file in ["bad.json", "orphan.pid"] {
        let path = dir.join(file);
        std::fs::write(&path, b"{}").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o0)).unwrap();
    }
    let inventory = registry::list_sessions_strict_in(&dir, &options);
    assert!(!inventory.complete);
    assert_eq!(
        inventory
            .errors
            .iter()
            .map(|error| error.kind)
            .collect::<Vec<_>>(),
        vec![
            registry::InventoryErrorKind::MetadataUnreadable,
            registry::InventoryErrorKind::PidUnreadable
        ]
    );
    let socket = dir.join("denied.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o0)).unwrap();
    let inventory = registry::list_sessions_strict_in(&dir, &options);
    assert!(!inventory.complete);
    assert!(inventory.errors.iter().any(|error| {
        error.path == dir.join("denied.json")
            && error.kind == registry::InventoryErrorKind::IdentityUnknown
    }));
    assert_eq!(
        inventory
            .entries
            .iter()
            .find(|session| session.name == "denied")
            .unwrap()
            .status,
        SessionStatus::Running
    );
    drop(listener);
    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn saturated_or_refused_socket_cannot_rescue_a_gone_identity() {
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    let dir = strict_root();
    let busy = dir.join("busy.sock");
    let listener = UnixListener::bind(&busy).unwrap();
    // Linux admits one connection with backlog zero. Hold it without accepting
    // so the scanner's nonblocking connect gets EAGAIN until its deadline.
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
    let queued = UnixStream::connect(&busy).unwrap();
    std::fs::write(dir.join("busy.pid"), DEAD_PID.to_string()).unwrap();
    let options = registry::ListOptions {
        socket_probe_budget: Duration::ZERO,
    };
    let inventory = registry::list_sessions_strict_in(&dir, &options);
    assert!(inventory.complete);
    assert!(inventory.errors.is_empty());
    assert_eq!(inventory.entries[0].status, SessionStatus::Vanished);
    // Unknown identity remains listed, and transport diagnostics honor
    // the shared budget without turning a timeout into loss.
    std::fs::write(dir.join("busy.pid"), std::process::id().to_string()).unwrap();
    let unknown = registry::list_sessions_strict_in(&dir, &options);
    assert!(!unknown.complete);
    assert_eq!(unknown.entries[0].status, SessionStatus::Running);
    assert_eq!(unknown.entries[0].pid, Some(std::process::id() as i32));
    assert!(
        unknown
            .errors
            .iter()
            .any(|error| error.kind == registry::InventoryErrorKind::IdentityUnknown)
    );
    assert!(
        unknown
            .errors
            .iter()
            .any(|error| error.kind == registry::InventoryErrorKind::ProbeTimeout)
    );
    std::fs::write(dir.join("busy.pid"), DEAD_PID.to_string()).unwrap();
    drop(queued);
    drop(listener);
    let inventory = registry::list_sessions_strict_in(&dir, &options);
    assert!(inventory.complete);
    assert!(inventory.errors.is_empty());
    assert_eq!(
        inventory
            .entries
            .iter()
            .map(|session| (session.name.as_str(), session.status, session.pid))
            .collect::<Vec<_>>(),
        vec![("busy", SessionStatus::Vanished, None)]
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn sidecar_reuse_is_vanished_even_with_a_reachable_socket() {
    let dir = strict_root();
    registry::with_root(&dir, || {
        let pid = std::process::id() as i32;
        let mut metadata = SessionMetadata {
            generation: Some("old-generation".into()),
            daemon_pid: Some(pid),
            daemon_start_token: Some("stale:start-token".into()),
            daemon_boot_id: registry::read_boot_id(),
            ..Default::default()
        };
        registry::write_metadata_publication("reused", &metadata).unwrap();
        registry::write_pid("reused", pid as u32).unwrap();
        let _listener = UnixListener::bind(registry::socket_path("reused")).unwrap();
        let before = registry::daemon_identity_counters();
        assert_eq!(
            registry::check_daemon_identity(pid, &metadata),
            registry::DaemonIdentityCheck::Mismatch
        );
        let inventory = registry::list_sessions_strict_in(&dir, &registry::ListOptions::default());
        assert!(inventory.complete);
        assert_eq!(inventory.entries[0].status, SessionStatus::Vanished);
        assert_eq!(inventory.entries[0].pid, None);
        assert_eq!(registry::read_pid("reused"), None);
        assert_eq!(
            registry::read_signal_target_with("reused", Some(&metadata)),
            None
        );
        assert!(registry::daemon_identity_counters().mismatched > before.mismatched);
        metadata.exited_at = Some("2026-10-01T00:00:00.000Z".into());
        registry::write_metadata_publication("reused", &metadata).unwrap();
        assert_eq!(registry::list_sessions()[0].status, SessionStatus::Exited);
    });
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn b1_unknown_identity_retains_running_pid_and_makes_strict_inventory_incomplete() {
    let dir = strict_root();
    registry::with_root(&dir, || {
        let pid = std::process::id() as i32;
        let metadata = SessionMetadata {
            daemon_pid: Some(pid),
            ..Default::default()
        };
        registry::write_metadata_publication("unknown", &metadata).unwrap();
        registry::write_pid("unknown", pid as u32).unwrap();
        let _listener = UnixListener::bind(registry::socket_path("unknown")).unwrap();
        assert_eq!(
            registry::check_daemon_identity(pid, &metadata),
            registry::DaemonIdentityCheck::Unknown
        );
        let inventory = registry::list_sessions_strict_in(&dir, &registry::ListOptions::default());
        assert!(!inventory.complete);
        assert_eq!(inventory.entries[0].status, SessionStatus::Running);
        assert_eq!(inventory.entries[0].pid, Some(pid));
        assert_eq!(inventory.entries[0].identity, registry::DaemonIdentityCheck::Unknown);
        assert_eq!(
            inventory.errors[0].kind,
            registry::InventoryErrorKind::IdentityUnknown
        );
        assert_eq!(
            registry::read_signal_target_with("unknown", Some(&metadata)),
            None
        );
        assert!(!registry::retire_stale_daemon_record("unknown", &metadata));
        assert!(registry::metadata_path("unknown").exists());
    });
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn b3_missing_boot_stale_token_proves_mismatch() {
    let pid = std::process::id() as i32;
    let metadata = SessionMetadata {
        daemon_pid: Some(pid),
        daemon_start_token: Some("stale:previous-owner".into()),
        ..Default::default()
    };
    assert_eq!(
        registry::check_daemon_identity(pid, &metadata),
        registry::DaemonIdentityCheck::Mismatch,
    );
}

#[cfg(target_os = "linux")]
#[test]
fn boot_identity_and_leader_identity_are_required() {
    let pid = std::process::id() as i32;
    let token = registry::read_process_start_token(pid).unwrap();
    let mut metadata = SessionMetadata {
        daemon_pid: Some(pid),
        daemon_start_token: Some(token),
        ephemeral: Some(false),
        ..Default::default()
    };
    // A token-only record cannot prove which boot owns this PID.
    assert_eq!(
        registry::check_daemon_identity(pid, &metadata),
        registry::DaemonIdentityCheck::Unknown
    );
    assert_eq!(
        registry::read_signal_target_with("token-only", Some(&metadata)),
        None
    );
    assert!(!registry::retire_stale_daemon_record(
        "token-only",
        &metadata
    ));
    metadata.daemon_boot_id = registry::read_boot_id();
    assert!(metadata.daemon_boot_id.is_some());
    assert_eq!(
        registry::check_daemon_identity(pid, &metadata),
        registry::DaemonIdentityCheck::Match
    );
    metadata.daemon_boot_id = Some("another-boot".into());
    assert_eq!(
        registry::check_daemon_identity(pid, &metadata),
        registry::DaemonIdentityCheck::Mismatch
    );
    let dir = strict_root();
    registry::with_root(&dir, || {
        registry::write_metadata_publication("old-boot", &metadata).unwrap();
        let inventory = registry::list_sessions_strict_in(&dir, &registry::ListOptions::default());
        assert!(inventory.complete);
        assert_eq!(inventory.entries[0].status, SessionStatus::Vanished);
        assert_eq!(
            registry::read_signal_target_with("old-boot", Some(&metadata)),
            None
        );
        assert!(registry::retire_stale_daemon_record("old-boot", &metadata));
    });
    std::fs::remove_dir_all(dir).unwrap();

    let (tid_tx, tid_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        // SAFETY: gettid has no arguments or side effects.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        tid_tx.send(tid).unwrap();
        release_rx.recv().unwrap();
    });
    let tid = tid_rx.recv().unwrap();
    assert_ne!(tid, pid);
    assert_eq!(registry::read_process_start_token(tid), None);
    metadata.daemon_pid = Some(tid);
    metadata.daemon_boot_id = registry::read_boot_id();
    assert_eq!(
        registry::check_daemon_identity(tid, &metadata),
        registry::DaemonIdentityCheck::Mismatch
    );
    let dir = strict_root();
    registry::with_root(&dir, || {
        registry::write_metadata_publication("thread", &metadata).unwrap();
        registry::write_pid("thread", tid as u32).unwrap();
        let inventory = registry::list_sessions_strict_in(&dir, &registry::ListOptions::default());
        assert!(inventory.complete);
        assert_eq!(inventory.entries[0].status, SessionStatus::Vanished);
        assert_eq!(
            registry::read_signal_target_with("thread", Some(&metadata)),
            None
        );
    });
    std::fs::remove_dir_all(dir).unwrap();
    release_tx.send(()).unwrap();
    thread.join().unwrap();
}

fn seed_retirement_record(name: &str, metadata: &SessionMetadata) {
    registry::write_metadata_publication(name, metadata).unwrap();
    registry::write_pid(name, metadata.daemon_pid.unwrap() as u32).unwrap();
    for path in [
        registry::socket_path(name),
        registry::events_path(name),
        registry::recovery_revision_path(name),
        registry::output_activity_path(name),
    ] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"retained companion").unwrap();
    }
}

fn retirement_artifacts(name: &str) -> Vec<std::path::PathBuf> {
    vec![
        registry::metadata_path(name),
        registry::pid_path(name),
        registry::socket_path(name),
        registry::events_path(name),
        registry::recovery_revision_path(name),
        registry::output_activity_path(name),
    ]
}

#[test]
fn startup_retirement_removes_stale_exited_history_but_not_live_unknown_or_own_name() {
    let dir = strict_root();
    registry::with_root(&dir, || {
        let pid = std::process::id() as i32;
        let stale = SessionMetadata {
            generation: Some("stale-generation".into()),
            daemon_pid: Some(pid),
            daemon_start_token: Some("stale:token".into()),
            daemon_boot_id: registry::read_boot_id(),
            created_at: "1970-01-01T00:00:00.000Z".into(),
            exited_at: Some("2026-10-01T00:00:00.000Z".into()),
            exit_code: Some(0),
            ..Default::default()
        };
        // All four exited records must be retired, not skipped as historical.
        for name in ["stale-a", "stale-b", "gone-a", "gone-b"] {
            let mut record = stale.clone();
            if name.starts_with("gone") {
                record.daemon_pid = Some(DEAD_PID);
                record.daemon_boot_id = None;
            }
            seed_retirement_record(name, &record);
        }
        seed_retirement_record("starting", &stale);
        let live = SessionMetadata {
            generation: Some("live-generation".into()),
            daemon_pid: Some(pid),
            daemon_start_token: registry::read_process_start_token(pid),
            daemon_boot_id: registry::read_boot_id(),
            ..Default::default()
        };
        seed_retirement_record("live", &live);
        let unknown = SessionMetadata {
            daemon_start_token: None,
            ..live.clone()
        };
        seed_retirement_record("unknown", &unknown);
        assert_eq!(registry::retire_stale_daemon_records("starting"), 4);
        for name in ["stale-a", "stale-b", "gone-a", "gone-b"] {
            assert!(retirement_artifacts(name).iter().all(|path| !path.exists()));
        }
        for name in ["starting", "live", "unknown"] {
            assert!(retirement_artifacts(name).iter().all(|path| path.exists()));
        }
        assert_eq!(
            registry::check_daemon_identity(pid, &live),
            registry::DaemonIdentityCheck::Match
        );
        assert_eq!(registry::retire_stale_daemon_records("starting"), 0);
    });
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn retirement_revalidates_generation_sidecar_and_both_locks() {
    let dir = strict_root();
    registry::with_root(&dir, || {
        let pid = std::process::id() as i32;
        let observed = SessionMetadata {
            generation: Some("observed-generation".into()),
            daemon_pid: Some(pid),
            daemon_start_token: Some("stale:token".into()),
            daemon_boot_id: registry::read_boot_id(),
            ephemeral: Some(false),
            ..Default::default()
        };
        seed_retirement_record("fenced", &observed);
        let replacement = SessionMetadata {
            generation: Some("new-generation".into()),
            ..observed.clone()
        };
        registry::write_metadata_publication("fenced", &replacement).unwrap();
        assert!(!registry::retire_stale_daemon_record("fenced", &observed));
        assert_eq!(registry::read_metadata("fenced"), Some(replacement));
        registry::write_metadata_publication("fenced", &observed).unwrap();
        registry::write_pid("fenced", DEAD_PID as u32).unwrap();
        assert!(!registry::retire_stale_daemon_record("fenced", &observed));
        let conflict = registry::list_sessions_strict_in(&dir, &registry::ListOptions::default());
        assert!(!conflict.complete);
        assert!(
            conflict
                .errors
                .iter()
                .any(|error| error.kind == registry::InventoryErrorKind::IdentityUnknown)
        );
        assert_eq!(
            registry::read_signal_target_with("fenced", Some(&observed)),
            None
        );
        registry::write_pid("fenced", pid as u32).unwrap();
        {
            let _lock = registry::acquire_event_lock("fenced").unwrap();
            assert!(!registry::retire_stale_daemon_record("fenced", &observed));
        }
        {
            let _lock = registry::acquire_lock("fenced").unwrap();
            assert!(!registry::retire_stale_daemon_record("fenced", &observed));
        }
        assert!(
            retirement_artifacts("fenced")
                .iter()
                .all(|path| path.exists())
        );
        assert!(registry::retire_stale_daemon_record("fenced", &observed));
        assert!(
            retirement_artifacts("fenced")
                .iter()
                .all(|path| !path.exists())
        );
    });
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_live_orphan_pid_is_incomplete_not_replacement_authority() {
    let dir = strict_root();
    std::fs::write(dir.join("orphan.pid"), std::process::id().to_string()).unwrap();
    let inventory = registry::list_sessions_strict_in(&dir, &registry::ListOptions::default());
    assert!(!inventory.complete);
    assert!(inventory.entries.is_empty());
    assert_eq!(
        inventory.errors[0].kind,
        registry::InventoryErrorKind::IdentityUnknown
    );
    assert_eq!(inventory.errors[0].path, dir.join("orphan.pid"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn retirement_preserves_malformed_and_nonregular_identity_companions() {
    use std::os::unix::ffi::OsStrExt;
    let dir = strict_root();
    registry::with_root(&dir, || {
        let stale = SessionMetadata {
            generation: Some("stale-generation".into()),
            daemon_pid: Some(std::process::id() as i32),
            daemon_start_token: Some("stale:token".into()),
            daemon_boot_id: registry::read_boot_id(),
            ..Default::default()
        };
        seed_retirement_record("invalid", &stale);
        std::fs::write(registry::pid_path("invalid"), b"not-a-pid").unwrap();
        assert!(!registry::retire_stale_daemon_record("invalid", &stale));
        assert!(
            retirement_artifacts("invalid")
                .iter()
                .all(|path| path.exists())
        );
        let fifo_path = registry::metadata_path("fifo");
        let fifo = std::ffi::CString::new(fifo_path.as_os_str().as_bytes()).unwrap();
        // SAFETY: fifo is a valid NUL-terminated path to a missing fixture.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(!registry::retire_stale_daemon_record("fifo", &stale));
        assert_eq!(registry::retire_stale_daemon_records("starting"), 0);
        assert!(fifo_path.exists());
    });
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn startup_retirement_preserves_current_boot_history_and_retention_policy() {
    let dir = strict_root();
    registry::with_root(&dir, || {
        let current_gone = SessionMetadata {
            generation: Some("current-boot-history".into()),
            daemon_pid: Some(DEAD_PID),
            daemon_start_token: Some("departed:token".into()),
            daemon_boot_id: registry::read_boot_id(),
            created_at: registry::now_iso8601(),
            exited_at: Some(registry::now_iso8601()),
            exit_code: Some(0),
            ephemeral: Some(false),
            ..Default::default()
        };
        seed_retirement_record("current-gone", &current_gone);
        let legacy_old = SessionMetadata {
            daemon_boot_id: None,
            created_at: "1970-01-01T00:00:00.000Z".into(),
            ..current_gone.clone()
        };
        seed_retirement_record("legacy-old", &legacy_old);
        let unknown_age = SessionMetadata {
            created_at: String::new(),
            ..legacy_old.clone()
        };
        seed_retirement_record("unknown-age", &unknown_age);
        for (name, base, key, value) in [
            ("keep-current", &current_gone, "keep", "true"),
            ("permanent-current", &current_gone, "strategy", "permanent"),
            ("keep-old", &legacy_old, "keep", "true"),
            ("permanent-old", &legacy_old, "strategy", "permanent"),
        ] {
            let mut retained = (*base).clone();
            retained.tags = Some(registry::TagMap::from([(key.into(), value.into())]));
            seed_retirement_record(name, &retained);
        }
        for (name, key, value) in [
            ("keep-mismatch", "keep", "true"),
            ("permanent-mismatch", "strategy", "permanent"),
        ] {
            let mut retained = legacy_old.clone();
            retained.daemon_pid = Some(std::process::id() as i32);
            retained.daemon_boot_id = registry::read_boot_id();
            retained.tags = Some(registry::TagMap::from([(key.into(), value.into())]));
            seed_retirement_record(name, &retained);
        }
        assert_eq!(registry::retire_stale_daemon_records("starting"), 0);
        for name in [
            "current-gone",
            "legacy-old",
            "unknown-age",
            "keep-current",
            "permanent-current",
            "keep-old",
            "permanent-old",
            "keep-mismatch",
            "permanent-mismatch",
        ] {
            assert!(
                retirement_artifacts(name).iter().all(|path| path.exists()),
                "{name}"
            );
        }
        #[cfg(target_os = "linux")]
        {
            let mut old_boot = current_gone.clone();
            old_boot.daemon_boot_id = Some("previous-boot".into());
            seed_retirement_record("old-boot-gone", &old_boot);
            assert!(registry::retire_stale_daemon_record(
                "old-boot-gone",
                &old_boot
            ));
            assert!(
                retirement_artifacts("old-boot-gone")
                    .iter()
                    .all(|path| !path.exists())
            );
        }
    });
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn startup_retirement_ignores_wall_clock_dates_without_stable_boot_identity() {
    let dir = strict_root();
    registry::with_root(&dir, || {
        for (name, boot, created_at) in [
            ("legacy-past", None, "1970-01-01T00:00:00.000Z"),
            ("legacy-future", None, "2999-01-01T00:00:00.000Z"),
            (
                "legacy-darwin",
                Some("darwin:{ sec = 1, usec = 0 }".to_string()),
                "1970-01-01T00:00:00.000Z",
            ),
        ] {
            let metadata = SessionMetadata {
                generation: Some(name.into()),
                daemon_pid: Some(DEAD_PID),
                daemon_start_token: Some("departed:token".into()),
                daemon_boot_id: boot,
                created_at: created_at.into(),
                exited_at: Some(registry::now_iso8601()),
                exit_code: Some(0),
                ephemeral: Some(false),
                ..Default::default()
            };
            seed_retirement_record(name, &metadata);
            let paths = retirement_artifacts(name);
            let before: Vec<_> = paths.iter().map(|path| std::fs::read(path).unwrap()).collect();
            assert!(!registry::retire_stale_daemon_record(name, &metadata));
            for (path, bytes) in paths.iter().zip(before) {
                assert_eq!(std::fs::read(path).unwrap(), bytes, "{name}: {path:?}");
            }
        }
        assert_eq!(registry::retire_stale_daemon_records("starting"), 0);
    });
    std::fs::remove_dir_all(dir).unwrap();
}
