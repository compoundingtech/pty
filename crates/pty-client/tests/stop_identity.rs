#![cfg(any(target_os = "linux", target_os = "macos"))]

#[cfg(target_os = "linux")]
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Child, Command};
#[cfg(target_os = "linux")]
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(any(target_os = "linux", debug_assertions))]
use std::time::Duration;
#[cfg(debug_assertions)]
use std::time::Instant;

struct Fixture {
    child: Child,
    root: PathBuf,
}
impl Fixture {
    fn new(child: Child) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "pty-stop-identity-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        Self { child, root }
    }
    fn record(&self, pid: i32, token: Option<String>, boot: Option<String>) -> UnixListener {
        let socket = UnixListener::bind(self.root.join("owner.sock")).unwrap();
        std::fs::write(self.root.join("owner.pid"), pid.to_string()).unwrap();
        // JSON keeps this test source compilable on the unfixed base too.
        std::fs::write(
            self.root.join("owner.json"),
            serde_json::to_vec(&serde_json::json!({
                "generation": "original", "daemonPid": pid,
                "daemonStartToken": token, "daemonBootId": boot
            }))
            .unwrap(),
        )
        .unwrap();
        socket
    }
    fn refused(&mut self) {
        let result = pty_client::stop_in(&self.root, "owner", Some("original"));
        assert!(
            result.is_err(),
            "unverified owner must refuse stop: {result:?}"
        );
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "stop signalled an unrelated owner"
        );
        assert!(
            self.root.join("owner.sock").exists(),
            "refusal must keep evidence"
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn child() -> Child {
    Command::new("sleep").arg("60").spawn().unwrap()
}
#[cfg(target_os = "linux")]
fn boot() -> String {
    format!(
        "linux:{}",
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .unwrap()
            .trim()
    )
}
#[cfg(target_os = "macos")]
fn boot() -> String {
    // Read real evidence even in the child with the debug unreadable-boot
    // hook. Only the production reader sees that injected failure.
    let output = Command::new("/usr/sbin/sysctl")
        .args(["-n", "kern.bootsessionuuid"])
        .output()
        .unwrap();
    assert!(output.status.success());
    format!("darwin:{}", String::from_utf8(output.stdout).unwrap().trim())
}

#[cfg(target_os = "macos")]
fn token(pid: i32) -> String {
    pty_core::registry::read_process_start_token(pid).expect("child has a start token")
}

#[cfg(target_os = "linux")]
fn token(pid: i32) -> String {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    format!(
        "linux:{}",
        stat[stat.rfind(')').unwrap() + 2..]
            .split_whitespace()
            .nth(19)
            .unwrap()
    )
}

#[test]
fn reused_pid_token_mismatch_refuses_stop() {
    let mut fixture = Fixture::new(child());
    let pid = fixture.child.id() as i32;
    let _socket = fixture.record(pid, Some("linux:0".into()), Some(boot()));
    fixture.refused();
}

#[test]
fn boot_mismatch_refuses_stop() {
    let mut fixture = Fixture::new(child());
    let pid = fixture.child.id() as i32;
    let _socket = fixture.record(pid, Some(token(pid)), Some("previous-boot".into()));
    fixture.refused();
}

#[test]
fn missing_identity_refuses_stop() {
    let mut fixture = Fixture::new(child());
    let pid = fixture.child.id() as i32;
    let _socket = fixture.record(pid, None, Some(boot()));
    fixture.refused();
}

#[test]
fn live_legacy_missing_boot_lists_unknown_but_explicit_stop_works() {
    let mut fixture = Fixture::new(child());
    let pid = fixture.child.id() as i32;
    let _socket = fixture.record(pid, Some(token(pid)), None);
    let sessions = pty_core::registry::list_sessions_in(
        &fixture.root,
        &pty_core::registry::ListOptions::default(),
    );
    assert_eq!(sessions[0].status, pty_core::registry::SessionStatus::Running);
    assert_eq!(sessions[0].identity, pty_core::registry::DaemonIdentityCheck::Unknown);
    assert!(matches!(
        pty_client::remove_in(&fixture.root, "owner"),
        Err(pty_client::RemoveError::StillRunning { .. })
    ));
    assert!(pty_client::stop_in(&fixture.root, "owner", Some("original"))
        .unwrap()
        .verified_empty());
    fixture.child.wait().unwrap();
    pty_client::remove_in(&fixture.root, "owner").unwrap();
}

#[test]
fn legacy_sidecar_without_a_recorded_start_token_stays_refused() {
    let mut fixture = Fixture::new(child());
    let pid = fixture.child.id() as i32;
    let _socket = fixture.record(pid, None, None);
    assert!(matches!(
        pty_client::signal_in(&fixture.root, "owner", libc::SIGTERM, Some("original")),
        Err(pty_client::SignalError::NotRunning { .. })
    ));
    assert_unknown_listing_and_refusal(&mut fixture, pid);
}

#[test]
fn a_legacy_verified_stop_is_distinguishable_from_a_match() {
    for boot_evidence in [true, false] {
        let mut fixture = Fixture::new(child());
        let pid = fixture.child.id() as i32;
        let _socket = fixture.record(pid, Some(token(pid)), boot_evidence.then(boot));
        let stopped = pty_client::stop_in(&fixture.root, "owner", Some("original")).unwrap();
        assert!(stopped.verified_empty(), "{stopped:?}");
        assert_eq!(
            stopped.identity,
            if boot_evidence {
                pty_core::registry::ControlIdentityCheck::Match
            } else {
                pty_core::registry::ControlIdentityCheck::LegacyVerified
            },
            "only full identity proof may report Match",
        );
        fixture.child.wait().unwrap();
    }
}

#[test]
fn legacy_recorded_pid_without_sidecar_or_recovery_token_cannot_stop() {
    let mut fixture = Fixture::new(child());
    let pid = fixture.child.id() as i32;
    let _socket = fixture.record(pid, None, None);
    std::fs::remove_file(fixture.root.join("owner.pid")).unwrap();
    fixture.refused();
}

fn assert_unknown_listing_and_refusal(fixture: &mut Fixture, pid: i32) {
    let sessions = pty_core::registry::list_sessions_in(
        &fixture.root, &pty_core::registry::ListOptions::default(),
    );
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].status, pty_core::registry::SessionStatus::Running);
    assert_eq!(sessions[0].pid, Some(pid));
    let record = std::fs::read(fixture.root.join("owner.json")).unwrap();
    fixture.refused();
    assert!(pty_client::remove_in(&fixture.root, "owner").is_err());
    assert!(fixture.child.try_wait().unwrap().is_none());
    assert_eq!(std::fs::read(fixture.root.join("owner.json")).unwrap(), record);
    assert_eq!(std::fs::read_to_string(fixture.root.join("owner.pid")).unwrap(), pid.to_string());
    assert!(fixture.root.join("owner.sock").exists());
}

#[test]
fn matching_owner_remains_stoppable() {
    let fixture = Fixture::new(child());
    let pid = fixture.child.id() as i32;
    let _socket = fixture.record(pid, Some(token(pid)), Some(boot()));
    let stopped = pty_client::stop_in(&fixture.root, "owner", Some("original")).unwrap();
    assert!(
        stopped.verified_empty(),
        "matching owner must stop completely: {stopped:?}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn thread_fixture() {
    if std::env::var_os("PTY_IDENTITY_THREAD_CHILD").is_none() {
        return;
    }
    let (send, recv) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        // SAFETY: gettid has no arguments or side effects.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) };
        send.send(tid).unwrap();
        std::thread::sleep(Duration::from_secs(60));
    });
    println!("IDENTITY_THREAD={}", recv.recv().unwrap());
    worker.join().unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn reused_pid_is_nonleader_thread_refuses_stop() {
    let process = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "thread_fixture", "--nocapture"])
        .env("PTY_IDENTITY_THREAD_CHILD", "1")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut fixture = Fixture::new(process);
    let stdout = fixture.child.stdout.take().unwrap();
    let tid = BufReader::new(stdout)
        .lines()
        .find_map(|line| {
            line.unwrap()
                .strip_prefix("IDENTITY_THREAD=")
                .map(|value| value.parse::<i32>().unwrap())
        })
        .expect("child publishes its worker thread ID");
    assert_ne!(tid, fixture.child.id() as i32);
    let _socket = fixture.record(tid, Some(token(tid)), Some(boot()));
    fixture.refused();
}

// PTY_TEST_BOOT_ID is deliberately unavailable in release builds.
#[cfg(debug_assertions)]
#[test]
fn unreadable_identity_child() {
    if std::env::var_os("PTY_IDENTITY_UNREADABLE_CHILD").is_none() {
        return;
    }
    let mut fixture = Fixture::new(child());
    let pid = fixture.child.id() as i32;
    let _socket = fixture.record(pid, Some(token(pid)), Some(boot()));
    assert_unknown_listing_and_refusal(&mut fixture, pid);
    let mut metadata: pty_core::registry::SessionMetadata =
        serde_json::from_slice(&std::fs::read(fixture.root.join("owner.json")).unwrap()).unwrap();
    metadata.daemon_start_token = Some("stale:previous-owner".into());
    assert_eq!(
        pty_core::registry::check_daemon_identity(pid, &metadata),
        pty_core::registry::DaemonIdentityCheck::Mismatch,
        "B3: stale tokens disprove ownership even while current boot is unreadable",
    );
}

#[cfg(debug_assertions)]
#[test]
fn b1_transient_unreadable_boot_lists_running_and_refuses_stop_remove() {
    let mut process = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "unreadable_identity_child", "--nocapture"])
        .env("PTY_IDENTITY_UNREADABLE_CHILD", "1")
        .env("PTY_TEST_BOOT_ID", "unavailable")
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = process.try_wait().unwrap() {
            assert!(
                status.success(),
                "unreadable boot identity must dispatch no signal"
            );
            break;
        }
        if Instant::now() >= deadline {
            let _ = process.kill();
            let _ = process.wait();
            panic!("unreadable identity probe timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn exit_evidence_keeps_unknown_owner_exited_with_named_pid() {
    for (field, value) in [
        ("exitCode", serde_json::json!(0)),
        ("exitedAt", serde_json::json!("2026-10-15T00:00:00.000Z")),
    ] {
        let mut fixture = Fixture::new(child());
        let pid = fixture.child.id() as i32;
        let _socket = fixture.record(pid, Some(token(pid)), None);
        let path = fixture.root.join("owner.json");
        let mut record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        record[field] = value;
        let bytes = serde_json::to_vec(&record).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let sessions = pty_core::registry::list_sessions_in(
            &fixture.root,
            &pty_core::registry::ListOptions::default(),
        );
        assert_eq!(sessions[0].status, pty_core::registry::SessionStatus::Exited);
        assert_eq!(sessions[0].pid, Some(pid));
        assert_eq!(sessions[0].identity, pty_core::registry::DaemonIdentityCheck::Unknown);
        fixture.refused();
        fixture.child.kill().unwrap();
        fixture.child.wait().unwrap();
        pty_client::remove_in(&fixture.root, "owner").unwrap();
        assert!(!path.exists());
    }
}

#[test]
fn metadata_without_pid_or_socket_is_gone_and_removable() {
    let mut fixture = Fixture::new(child());
    for exited in [false, true] {
        let mut record = serde_json::json!({"generation": "original", "command": "cat"});
        if exited {
            record["exitCode"] = serde_json::json!(0);
        }
        std::fs::write(
            fixture.root.join("owner.json"),
            serde_json::to_vec(&record).unwrap(),
        ).unwrap();
        let sessions = pty_core::registry::list_sessions_in(
            &fixture.root,
            &pty_core::registry::ListOptions::default(),
        );
        assert_eq!(sessions[0].identity, pty_core::registry::DaemonIdentityCheck::Gone);
        assert_eq!(sessions[0].pid, None);
        assert_eq!(
            sessions[0].status,
            if exited {
                pty_core::registry::SessionStatus::Exited
            } else {
                pty_core::registry::SessionStatus::Vanished
            },
        );
        pty_client::remove_in(&fixture.root, "owner").unwrap();
        assert!(!fixture.root.join("owner.json").exists());
        assert!(fixture.child.try_wait().unwrap().is_none());
    }
}

#[test]
fn pidless_metadata_with_companions_remains_unknown_and_not_removable() {
    let fixture = Fixture::new(child());
    let record = br#"{"generation":"original","command":"cat"}"#;
    std::fs::write(fixture.root.join("owner.json"), record).unwrap();
    for companion in ["owner.sock", "owner.pid"] {
        let path = fixture.root.join(companion);
        std::fs::write(&path, b"unproven companion").unwrap();
        let sessions = pty_core::registry::list_sessions_in(
            &fixture.root,
            &pty_core::registry::ListOptions::default(),
        );
        assert_eq!(sessions[0].identity, pty_core::registry::DaemonIdentityCheck::Unknown);
        assert!(pty_client::remove_in(&fixture.root, "owner").is_err());
        assert_eq!(
            std::fs::read(fixture.root.join("owner.json")).unwrap().as_slice(),
            record.as_slice(),
        );
        assert!(path.exists());
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn legacy_token_mismatch_and_dead_pid_refuse_explicit_controls() {
    for dead in [false, true] {
        let mut fixture = Fixture::new(child());
        let pid = fixture.child.id() as i32;
        let recorded_token = if dead { token(pid) } else { "stale:owner".into() };
        let _socket = fixture.record(pid, Some(recorded_token), None);
        if dead {
            fixture.child.kill().unwrap();
            fixture.child.wait().unwrap();
        }
        assert!(pty_client::stop_in(&fixture.root, "owner", None).is_err());
        assert!(matches!(
            pty_client::signal_in(&fixture.root, "owner", libc::SIGTERM, None),
            Err(pty_client::SignalError::NotRunning { .. })
        ));
        assert!(fixture.root.join("owner.sock").exists());
        if !dead {
            assert!(fixture.child.try_wait().unwrap().is_none());
        }
    }
}

#[test]
fn modern_wrong_boot_and_missing_token_refuse_signal_before_socket_query() {
    for missing_token in [false, true] {
        let mut fixture = Fixture::new(child());
        let pid = fixture.child.id() as i32;
        let socket = fixture.record(
            pid,
            (!missing_token).then(|| token(pid)),
            Some(if missing_token { boot() } else { "previous-boot".into() }),
        );
        socket.set_nonblocking(true).unwrap();
        assert!(matches!(
            pty_client::signal_in(&fixture.root, "owner", libc::SIGTERM, None),
            Err(pty_client::SignalError::NotRunning { .. })
        ));
        assert_eq!(socket.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        assert!(fixture.child.try_wait().unwrap().is_none());
    }
}

#[test]
fn pre_141_records_signal_owned_child_without_upgrading_inventory_identity() {
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    for recorded_token in [true, false] {
        let mut fixture = Fixture::new(child());
        let mut program = Fixture::new(child());
        let daemon_pid = fixture.child.id() as i32;
        let program_pid = program.child.id() as i32;
        let socket = fixture.record(
            daemon_pid,
            recorded_token.then(|| token(daemon_pid)),
            None,
        );
        // Old producers omitted the boot field entirely, not merely JSON null.
        let path = fixture.root.join("owner.json");
        let mut record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        record.as_object_mut().unwrap().remove("daemonBootId");
        if !recorded_token {
            record.as_object_mut().unwrap().remove("daemonStartToken");
        }
        let original = serde_json::to_vec(&record).unwrap();
        std::fs::write(&path, &original).unwrap();
        socket.set_nonblocking(true).unwrap();
        if !recorded_token {
            assert!(matches!(
                pty_client::signal_in(&fixture.root, "owner", libc::SIGTERM, Some("original")),
                Err(pty_client::SignalError::NotRunning { .. })
            ));
            assert_eq!(socket.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
            assert!(program.child.try_wait().unwrap().is_none());
            assert_eq!(std::fs::read(&path).unwrap(), original);
            continue;
        }
        let responder = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut stream = loop {
                match socket.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "control never queried STATUS");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept STATUS: {error}"),
                }
            };
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut request = [0_u8; 64];
            assert!(stream.read(&mut request).unwrap() > 0);
            let body = serde_json::json!({
                "name": "example-legacy-control",
                "terminal": {"cols":80,"rows":24,"cursorX":0,"cursorY":0,
                    "scrollbackUsed":0,"scrollbackCapacity":0},
                "process": {"alive":true,"pid":program_pid},
                "daemon": {"pid":daemon_pid},
                "clients": {"total":0,"attached":0,"readOnly":0},
                "modes": {"sgrMouse":false,"cursorHidden":false,
                    "kittyKeyboard":false,"kittyKeyboardFlags":[]}
            });
            stream.write_all(&pty_core::protocol::encode_status_response(&body.to_string())).unwrap();
        });
        let sent = pty_client::signal_in(
            &fixture.root, "owner", libc::SIGTERM, Some("original"),
        ).unwrap();
        responder.join().unwrap();
        assert_eq!(sent.pid, program_pid);
        let deadline = Instant::now() + Duration::from_secs(2);
        while program.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "child did not receive SIGTERM");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(fixture.child.try_wait().unwrap().is_none());
        assert_eq!(std::fs::read(path).unwrap(), original);
        assert_eq!(
            pty_core::registry::list_sessions_in(
                &fixture.root, &pty_core::registry::ListOptions::default(),
            )[0].identity,
            pty_core::registry::DaemonIdentityCheck::Unknown,
        );
    }
}

#[test]
fn exited_legacy_record_waits_for_owned_daemon_before_removal() {
    let mut fixture = Fixture::new(child());
    let pid = fixture.child.id() as i32;
    let _socket = fixture.record(pid, Some(token(pid)), None);
    let path = fixture.root.join("owner.json");
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    record["exitCode"] = serde_json::json!(0);
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    std::thread::scope(|scope| {
        let child = &mut fixture.child;
        scope.spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            child.kill().unwrap();
            child.wait().unwrap();
        });
        pty_client::remove_in(&fixture.root, "owner").unwrap();
    });
    assert!(!path.exists());
    assert!(!fixture.root.join("owner.sock").exists());
    assert!(!fixture.root.join("owner.pid").exists());
}
