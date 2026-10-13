//! Launch operations end to end: an owner holding the control socket,
//! `pty operation reserve`, the launcher, the gated daemon and its held
//! child, and the recovery verbs, through the real binary.
//!
//! Each test plays the owner. Barriers (`PTY_TEST_OPERATION_BARRIERS`, debug
//! builds only) hold the launcher or the daemon at a named point while the
//! test delivers a cancel, closes its end, or kills a participant, and then
//! the test checks the exact record, frames and `settle` verdict.

mod cli_common;

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use cli_common::{Rig, wait_until};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The descriptor number the launcher is handed in most tests.
const FD: i32 = 7;

/// Hand `fd` to the child as `target`, inheritable. `dup2` onto itself is a
/// no-op that leaves close-on-exec set, so that case clears the flag.
fn inherit_as(cmd: &mut Command, fd: i32, target: i32) {
    // SAFETY: dup2 and fcntl in the child before exec.
    unsafe {
        cmd.pre_exec(move || {
            let ok = if fd == target {
                libc::fcntl(fd, libc::F_SETFD, 0) >= 0
            } else {
                libc::dup2(fd, target) >= 0
            };
            if ok {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn envelope(operation_id: &str, runtime_id: &str, expected: Option<&str>) -> String {
    serde_json::to_string(&json!({
        "type": "envelope",
        "version": 1,
        "operationId": operation_id,
        "owner": "agent/test",
        "origin": "test",
        "subject": "launch",
        "runtimeId": runtime_id,
        "expectedIncarnation": expected,
        "bindings": {"attempt": 1}
    }))
    .unwrap()
}

/// What a test wants from one launch.
#[derive(Default)]
struct Plan {
    /// Do not reserve first.
    unreserved: bool,
    /// The previous incarnation, for a replacement.
    expected: Option<String>,
    /// Barriers to hold.
    hold: Vec<&'static str>,
    /// Lines written right after the envelope, before anything is read.
    queued: Vec<String>,
    /// The command after `--`.
    command: Vec<String>,
    /// Extra environment for the launcher (and so the daemon).
    env: Vec<(String, String)>,
    /// The executable to launch with, when not the built binary.
    executable: Option<PathBuf>,
    /// The session id, when not `rt-<tag>`.
    runtime_id: Option<String>,
}

/// The owner of one launch operation.
struct Owner {
    socket: UnixStream,
    buf: Vec<u8>,
    operation_id: String,
    runtime_id: String,
    envelope: String,
    sha: String,
    digest: String,
    launcher: Option<Child>,
    barriers: PathBuf,
    frames: Vec<Value>,
    marker: PathBuf,
}

impl Owner {
    fn start(rig: &Rig, tag: &str, plan: Plan) -> Owner {
        let operation_id = format!("op-{tag}-{}", std::process::id());
        let runtime_id = plan
            .runtime_id
            .clone()
            .unwrap_or_else(|| format!("rt-{tag}"));
        let line = envelope(&operation_id, &runtime_id, plan.expected.as_deref());
        if !plan.unreserved {
            let out = rig.run_stdin(&["operation", "reserve", "--json"], &format!("{line}\n"));
            assert_eq!(out.code, 0, "reserve: {}{}", out.stdout, out.stderr);
            assert_eq!(out.json()["verdict"], "reserved");
        }
        let barriers = rig.scratch.join(format!("barriers-{tag}"));
        std::fs::create_dir_all(&barriers).unwrap();
        for name in &plan.hold {
            std::fs::write(barriers.join(format!("{name}.hold")), "").unwrap();
        }
        let marker = rig.scratch.join(format!("ran-{tag}"));
        let command = if plan.command.is_empty() {
            vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("echo ran > {}; exec sleep 60", marker.display()),
            ]
        } else {
            plan.command.clone()
        };
        let (owner_end, launcher_end) = UnixStream::pair().unwrap();
        let digest = sha256_hex(operation_id.as_bytes());
        let mut cmd = match &plan.executable {
            Some(executable) => {
                let mut cmd = Command::new(executable);
                cmd.env_remove("PTY_SESSION")
                    .env_remove("PTY_SESSION_DIR")
                    .env("PTY_ROOT", &rig.root)
                    .env("PTY_ROOT_LEGACY_SILENT", "1")
                    .current_dir(&rig.scratch);
                cmd
            }
            None => rig.cmd(&[]),
        };
        cmd.args([
            "run",
            "-d",
            "--operation-fd",
            &FD.to_string(),
            "--launch-operation",
            &digest,
            "--id",
            &runtime_id,
            "--",
        ])
        .args(&command)
        .env("PTY_TEST_OPERATION_BARRIERS", &barriers)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        for (key, value) in &plan.env {
            cmd.env(key, value);
        }
        inherit_as(&mut cmd, launcher_end.as_raw_fd(), FD);
        let launcher = cmd.spawn().expect("spawn the launcher");
        drop(launcher_end);
        let mut owner = Owner {
            socket: owner_end,
            buf: Vec::new(),
            sha: sha256_hex(line.as_bytes()),
            envelope: line.clone(),
            operation_id,
            runtime_id,
            digest,
            launcher: Some(launcher),
            barriers,
            frames: Vec::new(),
            marker,
        };
        owner.write(&line);
        for queued in &plan.queued {
            owner.write(queued);
        }
        owner
    }

    fn write(&mut self, line: &str) {
        let _ = self.socket.write_all(format!("{line}\n").as_bytes());
    }

    fn cancel_line(&self, request_id: &str) -> String {
        json!({"type":"cancel","version":1,"operationId":self.operation_id,"envelopeSha256":self.sha,"requestId":request_id}).to_string()
    }

    fn cancel(&mut self, request_id: &str) {
        let line = self.cancel_line(request_id);
        self.write(&line);
    }

    fn transfer_line(&self, request_id: &str, incarnation: &Value) -> String {
        json!({"type":"transfer","version":1,"operationId":self.operation_id,"envelopeSha256":self.sha,"requestId":request_id,
               "incarnation":{"id":incarnation["id"],"generation":incarnation["generation"]}}).to_string()
    }

    fn transfer(&mut self, request_id: &str, incarnation: &Value) {
        let line = self.transfer_line(request_id, incarnation);
        self.write(&line);
    }

    /// The next frame, or `None` at end of file or after `wait`.
    fn next(&mut self, wait: Duration) -> Option<Value> {
        let deadline = Instant::now() + wait;
        loop {
            if let Some(end) = self.buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=end).collect();
                let frame: Value = serde_json::from_slice(&line[..line.len() - 1])
                    .expect("a frame is one JSON object");
                self.frames.push(frame.clone());
                return Some(frame);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            self.socket.set_read_timeout(Some(left)).unwrap();
            let mut chunk = [0u8; 65536];
            match self.socket.read(&mut chunk) {
                Ok(0) => return None,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return None;
                }
                Err(e) => panic!("reading frames: {e}"),
            }
        }
    }

    /// Frames until one of `kind`; panics after 20 s.
    fn wait_for(&mut self, kind: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            match self.next(deadline - Instant::now()) {
                Some(frame) if frame["type"] == kind => return frame,
                Some(_) => {}
                None => break,
            }
        }
        panic!("no {kind} frame; frames so far: {:#?}", self.frames);
    }

    /// Every frame until end of file.
    fn drain(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        while let Some(frame) = self.next(Duration::from_secs(20)) {
            out.push(frame);
        }
        out
    }

    fn reached(&self, barrier: &str) -> bool {
        self.barriers.join(format!("{barrier}.reached")).exists()
    }

    fn wait_reached(&self, barrier: &str) {
        wait_until(&format!("barrier {barrier}"), || self.reached(barrier));
    }

    fn release(&self, barrier: &str) {
        std::fs::write(self.barriers.join(format!("{barrier}.release")), "").unwrap();
    }

    /// The pid that reached `barrier`.
    fn holder(&self, barrier: &str) -> i32 {
        std::fs::read_to_string(self.barriers.join(format!("{barrier}.reached")))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    fn launcher_exit(&mut self) -> i32 {
        let mut child = self.launcher.take().expect("launcher");
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status.code().unwrap_or(-1);
            }
            assert!(Instant::now() < deadline, "the launcher did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn settle(&self, rig: &Rig) -> Value {
        self.settle_env(rig, &[])
    }

    fn settle_env(&self, rig: &Rig, env: &[(&str, &str)]) -> Value {
        let out = rig.run_env(
            &[
                "operation",
                "settle",
                &self.operation_id,
                "--envelope-sha256",
                &self.sha,
                "--json",
            ],
            env,
        );
        assert_eq!(out.code, 0, "settle: {}", out.stderr);
        out.json()
    }

    /// Settle until the verdict is `verdict` (participants take a moment to
    /// leave the process table).
    /// Settle until the verdict is `verdict`; see [`settle_reactively`].
    fn settle_until(&self, rig: &Rig, verdict: &str) -> Value {
        settle_reactively(rig, &self.operation_id, &self.sha, verdict, None)
    }

    fn settle_until_kind(&self, rig: &Rig, verdict: &str, kind: &str) -> Value {
        settle_reactively(rig, &self.operation_id, &self.sha, verdict, Some(kind))
    }

    fn retire(&self, rig: &Rig, request_id: &str, incarnation: Option<&str>) -> (i32, Value) {
        let mut args = vec![
            "operation",
            "retire",
            self.operation_id.as_str(),
            "--envelope-sha256",
            self.sha.as_str(),
            "--request-id",
            request_id,
            "--json",
        ];
        if let Some(inc) = incarnation {
            args.extend(["--incarnation", inc]);
        }
        let out = rig.run(&args);
        (out.code, out.json())
    }

    fn record(&self, rig: &Rig) -> Value {
        rig.ok(&["operation", "show", &self.operation_id, "--json"])
            .json()["record"]
            .clone()
    }

    fn ran(&self) -> bool {
        self.marker.exists()
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        // Never leave a barrier holding a process.
        for barrier in pty_core::operation::barrier::BARRIERS {
            let _ = std::fs::write(self.barriers.join(format!("{barrier}.release")), "");
        }
        if let Some(mut child) = self.launcher.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn verdict(v: &Value) -> (String, Option<String>) {
    (
        v["verdict"].as_str().unwrap().to_string(),
        v["kind"].as_str().map(str::to_string),
    )
}

fn component<'a>(v: &'a Value, name: &str) -> &'a Value {
    v["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no {name} component in {v:#}"))
}

/// Every live process whose argv carries `--launch-operation <digest>`.
fn marked(digest: &str) -> Vec<i32> {
    // SAFETY: getuid(2) has no preconditions.
    let scan = pty_core::proctable::argv::scan_argv(unsafe { libc::getuid() });
    scan.rows
        .iter()
        .filter(|row| pty_core::operation::settle::argv_carries_marker(&row.argv, digest))
        .map(|row| row.pid)
        .collect()
}

/// The held child has not exec'd: its argv is still the daemon's, birth
/// marker and all. Positive evidence, not the absence of a side effect.
fn still_held(child: i32, digest: &str) -> bool {
    // SAFETY: getuid(2) has no preconditions.
    let scan = pty_core::proctable::argv::scan_argv(unsafe { libc::getuid() });
    scan.rows.iter().any(|row| {
        row.pid == child && pty_core::operation::settle::argv_carries_marker(&row.argv, digest)
    })
}

fn child_of(frame: &Value) -> i32 {
    frame["incarnation"]["child"]["pid"].as_i64().unwrap() as i32
}

fn daemon_of(frame: &Value) -> i32 {
    frame["incarnation"]["daemon"]["pid"].as_i64().unwrap() as i32
}

fn alive(pid: i32) -> bool {
    matches!(pty_core::proctable::process(pid), pty_core::proctable::Answer::Known(row) if !row.is_zombie())
}

/// Wait for `pid` to exit: on its exit (a pidfd or a kqueue), not on a
/// timer. A zombie has exited.
fn wait_gone(pid: i32) {
    use pty_core::operation::exact::{Exit, wait_for_exit};
    match wait_for_exit(pid, Duration::from_secs(20)) {
        Exit::Exited => {}
        Exit::Running => panic!("pid {pid} did not exit"),
        Exit::Unsupported => wait_until(&format!("pid {pid} to exit"), || !alive(pid)),
    }
}

/// Settle until the verdict is `verdict` (and its kind `kind`), waiting in
/// between on exactly the processes the evidence names (a live writer or
/// participant, a marked or unreadable process) to exit. A process of this
/// user caught exiting is honestly unreadable for that capture; the next
/// capture follows its exit, not a timer. A verdict that names no process to
/// wait for fails at once.
fn settle_reactively(
    rig: &Rig,
    operation_id: &str,
    sha: &str,
    verdict: &str,
    kind: Option<&str>,
) -> Value {
    for _ in 0..20 {
        let out = rig.run(&[
            "operation",
            "settle",
            operation_id,
            "--envelope-sha256",
            sha,
            "--json",
        ]);
        assert_eq!(out.code, 0, "settle: {}", out.stderr);
        let v = out.json();
        if v["verdict"] == verdict && kind.is_none_or(|kind| v["kind"] == kind) {
            return v;
        }
        let pids = named_processes(&v);
        assert!(
            !pids.is_empty(),
            "settle is {}/{} and names no process to wait for (wanted {verdict}/{kind:?}): {v:#}",
            v["verdict"],
            v["kind"]
        );
        for pid in pids {
            wait_gone(pid);
        }
    }
    panic!("settle never became {verdict}/{kind:?}");
}

/// Every process a settle verdict names as still live or unreadable.
fn named_processes(v: &Value) -> Vec<i32> {
    let mut pids = Vec::new();
    for component in v["components"].as_array().into_iter().flatten() {
        let detail = &component["detail"];
        match component["name"].as_str() {
            Some("writer") if component["status"] == "pending" => {
                let pid = detail["pid"]
                    .as_i64()
                    .or_else(|| detail["holderPid"].as_i64())
                    .or_else(|| v["record"]["writer"]["id"]["pid"].as_i64());
                pids.extend(pid.map(|pid| pid as i32));
            }
            Some("participants") => {
                for p in detail["participants"].as_array().into_iter().flatten() {
                    if p["status"] == "live" {
                        pids.extend(p["pid"].as_i64().map(|pid| pid as i32));
                    }
                }
            }
            Some("marker-scan") => {
                for m in detail["matches"].as_array().into_iter().flatten() {
                    pids.extend(m["pid"].as_i64().map(|pid| pid as i32));
                }
                for u in detail["unreadable"].as_array().into_iter().flatten() {
                    pids.extend(u.as_i64().map(|pid| pid as i32));
                }
            }
            _ => {}
        }
    }
    pids.sort_unstable();
    pids.dedup();
    pids
}

fn kill(pid: i32, signal: i32) {
    // SAFETY: a signal to a test-owned process.
    unsafe {
        libc::kill(pid, signal);
    }
}

fn row_exists(rig: &Rig, name: &str) -> bool {
    rig.exists(&format!("{name}.json"))
        || rig.exists(&format!("{name}.sock"))
        || rig.exists(&format!("{name}.pid"))
}

// ── Discovery and reservation ────────────────────────────────────────────

#[test]
fn capabilities_advertise_version_one() {
    let rig = Rig::new();
    let v = rig.ok(&["capabilities", "--json"]).json();
    assert_eq!(
        v["capabilities"]["launch-operation"]["versions"],
        json!([1])
    );
    assert_eq!(
        v["capabilities"]["launch-operation"]["recordSchemas"],
        json!(["pty.launch-operation.v1"])
    );
    assert!(v["version"].as_str().is_some());
}

#[test]
fn reserve_refuses_an_invalid_envelope_and_writes_nothing() {
    let rig = Rig::new();
    let bad = envelope("op-bad", "rt", None).replace(r#""version":1"#, r#""version":2"#);
    let out = rig.run_stdin(&["operation", "reserve", "--json"], &format!("{bad}\n"));
    assert_eq!(out.code, 1);
    let v = out.json();
    assert_eq!(
        (v["code"].as_str(), v["path"].as_str()),
        (Some("envelope-invalid"), Some("version"))
    );
    assert!(
        !rig.path("operations")
            .join(format!("{}.json", sha256_hex(b"op-bad")))
            .exists()
    );

    let huge = format!("{}\n", "x".repeat(32 * 1024 + 1));
    let out = rig.run_stdin(&["operation", "reserve", "--json"], &huge);
    assert_eq!(out.json()["reason"], "too-large");
}

#[test]
fn a_second_reserve_is_refused_and_the_record_is_untouched() {
    let rig = Rig::new();
    let line = envelope("op-dup", "rt-dup", None);
    assert_eq!(
        rig.run_stdin(&["operation", "reserve", "--json"], &format!("{line}\n"))
            .code,
        0
    );
    let path = rig
        .path("operations")
        .join(format!("{}.json", sha256_hex(b"op-dup")));
    let before = std::fs::read(&path).unwrap();
    let out = rig.run_stdin(&["operation", "reserve", "--json"], &format!("{line}\n"));
    assert_eq!(
        (out.code, out.json()["code"].clone()),
        (1, json!("operation-exists"))
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    // A reservation is claimable until it is withdrawn: never closure.
    let v = settle_reactively(
        &rig,
        "op-dup",
        &sha256_hex(line.as_bytes()),
        "retire-required",
        Some("reserved"),
    );
    assert_eq!(v["noFurtherEffects"], false);
}

#[test]
fn concurrent_reserves_create_exactly_one_record() {
    let rig = std::sync::Arc::new(Rig::new());
    let line = envelope("op-race", "rt-race", None);
    let handles: Vec<_> = (0..6)
        .map(|_| {
            let rig = std::sync::Arc::clone(&rig);
            let line = line.clone();
            std::thread::spawn(move || {
                rig.run_stdin(&["operation", "reserve", "--json"], &format!("{line}\n"))
                    .code
            })
        })
        .collect();
    let codes: Vec<i32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(codes.iter().filter(|c| **c == 0).count(), 1, "{codes:?}");
    assert_eq!(codes.iter().filter(|c| **c == 1).count(), 5, "{codes:?}");
}

#[test]
fn retire_withdraws_an_unclaimed_reservation_so_no_launcher_can_claim_it() {
    let rig = Rig::new();
    let mut owner = Owner::start(
        &rig,
        "withdraw",
        Plan {
            hold: vec!["launcher-before-claim"],
            ..Plan::default()
        },
    );
    owner.wait_reached("launcher-before-claim");
    // The launcher is alive but has not claimed: the reservation is
    // withdrawn under it.
    let (code, v) = owner.retire(&rig, "w1", None);
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["record"]["state"], "cancelled");
    assert_eq!(v["record"]["decision"]["cause"], "withdrawn");
    owner.release("launcher-before-claim");
    let refused = owner.wait_for("refused");
    assert_eq!(refused["code"], "not-claimable");
    assert_eq!(owner.launcher_exit(), 1);
    assert!(!row_exists(&rig, &owner.runtime_id));
    let v = owner.settle_until(&rig, "closed");
    assert_eq!(
        verdict(&v),
        ("closed".into(), Some("never-launched".into()))
    );
    assert_eq!(v["noFurtherEffects"], true);
}

// ── The successful launch ────────────────────────────────────────────────

#[test]
fn the_command_runs_only_after_transfer() {
    let rig = Rig::new();
    let mut owner = Owner::start(&rig, "happy", Plan::default());
    assert_eq!(owner.wait_for("accepted")["state"], "pending");
    let decision = owner.wait_for("decision");
    assert_eq!(decision["decision"]["cause"], "launch");
    assert_eq!(decision["state"], "launched");
    let held = owner.wait_for("held");
    assert_eq!(held["phase"], "held");
    let ready = owner.wait_for("ready");
    assert_eq!(owner.launcher_exit(), 0);
    let incarnation = ready["incarnation"].clone();
    let meta = rig.read_meta(&owner.runtime_id).unwrap();
    assert_eq!(
        meta["launchOperation"],
        json!({"digest": owner.digest, "phase": "startup"})
    );
    assert_eq!(
        incarnation["id"],
        json!(format!(
            "{}:{}",
            meta["daemonPid"],
            meta["createdAt"].as_str().unwrap()
        ))
    );
    assert_eq!(incarnation["generation"], meta["generation"]);
    // Held: published and prepared, and the command has not run: the child
    // still carries the daemon's argv.
    assert!(still_held(child_of(&ready), &owner.digest));
    assert!(!owner.ran());

    owner.transfer("t1", &incarnation);
    let transferred = owner.wait_for("transferred");
    assert_eq!(transferred["state"], "transferred");
    assert_eq!(transferred["transfer"]["released"], true);
    assert_eq!(transferred["transfer"]["requestId"], "t1");
    wait_until("the command to run", || owner.ran());
    assert_eq!(
        rig.read_meta(&owner.runtime_id).unwrap()["launchOperation"]["phase"],
        "transferred"
    );
    // The channel closes after transfer.
    assert!(owner.next(Duration::from_secs(5)).is_none());
    let v = owner.settle(&rig);
    assert_eq!(verdict(&v), ("transferred".into(), None));
    assert_eq!(v["record"]["transfer"]["released"], true);
    assert_eq!(v["noFurtherEffects"], false);
    // A transferred operation is steady state: retire refuses it.
    let (code, refusal) = owner.retire(&rig, "late", incarnation["id"].as_str());
    assert_eq!((code, refusal["code"].clone()), (1, json!("transferred")));
    // Every frame echoes the envelope and the record revision rises.
    let mut last = 0;
    for frame in &owner.frames {
        assert_eq!(frame["envelopeSha256"], owner.sha);
        assert_eq!(
            frame["envelope"],
            serde_json::from_str::<Value>(&owner.envelope).unwrap()
        );
        let revision = frame["recordRevision"].as_u64().unwrap();
        assert!(revision >= last);
        last = revision;
    }
    rig.ok(&["kill", &owner.runtime_id]);
}

#[test]
fn a_replacement_clears_only_the_exact_ended_predecessor() {
    let rig = Rig::new();
    // An ended row for the same id: the prior incarnation.
    rig.write_meta(
        "rt-replace",
        json!({"generation": "g-old", "daemonPid": 424242, "createdAt": "2026-10-13T10:00:00.000Z", "exitCode": 0, "exitedAt": "2026-10-13T10:01:00.000Z"}),
    );
    // Null expectedIncarnation: an ended row is a mismatch.
    let mut owner = Owner::start(
        &rig,
        "replace",
        Plan {
            runtime_id: Some("rt-replace".into()),
            ..Plan::default()
        },
    );
    let decision = owner.wait_for("decision");
    assert_eq!(decision["decision"]["cause"], "predecessor-mismatch");
    assert_eq!(decision["state"], "cancelled");
    assert!(
        rig.read_meta("rt-replace").is_some(),
        "an unrelated ended row is never cleared"
    );
    drop(owner);
    // A different prior incarnation: also a mismatch.
    let mut owner = Owner::start(
        &rig,
        "replace-other",
        Plan {
            expected: Some("424243:2026-10-13T10:00:00.000Z".into()),
            runtime_id: Some("rt-replace".into()),
            ..Plan::default()
        },
    );
    assert_eq!(
        owner.wait_for("decision")["decision"]["cause"],
        "predecessor-mismatch"
    );
    drop(owner);
    // The exact one: cleared and replaced.
    let mut owner = Owner::start(
        &rig,
        "replace-exact",
        Plan {
            expected: Some("424242:2026-10-13T10:00:00.000Z".into()),
            runtime_id: Some("rt-replace".into()),
            ..Plan::default()
        },
    );
    let decision = owner.wait_for("decision");
    assert_eq!(decision["decision"]["cause"], "launch");
    assert_eq!(
        decision["decision"]["observed"],
        "424242:2026-10-13T10:00:00.000Z"
    );
    owner.wait_for("ready");
    assert_ne!(rig.read_meta("rt-replace").unwrap()["generation"], "g-old");
    owner.cancel("c");
    owner.wait_for("retirement");
}

#[test]
fn a_live_row_is_occupied_and_is_left_alone() {
    let rig = Rig::new();
    rig.spawn_cat("rt-busy", &[]);
    let before = rig.read_meta("rt-busy").unwrap();
    let mut owner = Owner::start(&rig, "busy", Plan::default());
    let decision = owner.wait_for("decision");
    assert_eq!(decision["decision"]["cause"], "occupied");
    assert!(
        decision["decision"]["observed"]
            .as_str()
            .unwrap()
            .starts_with("live:")
    );
    assert_eq!(owner.launcher_exit(), 1);
    assert_eq!(rig.read_meta("rt-busy").unwrap(), before);
    let v = owner.settle_until(&rig, "closed");
    assert_eq!(
        verdict(&v),
        ("closed".into(), Some("never-launched".into()))
    );
}

// ── Cancel at each barrier ───────────────────────────────────────────────

/// Hold `barrier`, deliver a cancel while it holds, release it, and return
/// the owner and every frame up to end of file.
fn cancel_at(rig: &Rig, tag: &str, barrier: &'static str) -> (Owner, Vec<Value>) {
    let mut owner = Owner::start(
        rig,
        tag,
        Plan {
            hold: vec![barrier],
            ..Plan::default()
        },
    );
    owner.wait_reached(barrier);
    owner.cancel("c-held");
    // Delivery is the owner's write succeeding; the decision is separate.
    std::thread::sleep(Duration::from_millis(200));
    owner.release(barrier);
    let frames = owner.drain();
    (owner, frames)
}

fn kinds(frames: &[Value]) -> Vec<String> {
    frames
        .iter()
        .map(|f| f["type"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn a_cancel_before_the_fork_cancels_without_any_daemon() {
    for barrier in ["launcher-before-claim", "launcher-before-peek"] {
        let rig = Rig::new();
        let (mut owner, frames) = cancel_at(&rig, "early", barrier);
        assert_eq!(kinds(&frames), vec!["decision"], "{barrier}: {frames:#?}");
        assert_eq!(frames[0]["decision"]["cause"], "cancel");
        assert_eq!(frames[0]["decision"]["requestId"], "c-held");
        assert!(
            frames[0]["participants"]["producer"].is_null(),
            "{barrier}: no daemon was forked"
        );
        assert_eq!(owner.launcher_exit(), 1);
        assert!(!row_exists(&rig, &owner.runtime_id));
        let v = owner.settle_until(&rig, "closed");
        assert_eq!(
            verdict(&v),
            ("closed".into(), Some("never-launched".into())),
            "{barrier}"
        );
        assert_eq!(v["noFurtherEffects"], true);
    }
}

#[test]
fn a_cancel_before_the_decision_wins_and_publishes_nothing() {
    // No actor exists yet at these barriers: the cancel is delivered (it sits
    // in the socket) while held, and decided once released.
    for barrier in ["launcher-before-fork", "daemon-gated"] {
        let rig = Rig::new();
        let (mut owner, frames) = cancel_at(&rig, "decide", barrier);
        let decision = frames
            .iter()
            .find(|f| f["type"] == "decision")
            .unwrap_or_else(|| panic!("{barrier}: {frames:#?}"));
        assert_eq!(decision["decision"]["cause"], "cancel", "{barrier}");
        assert_eq!(decision["state"], "cancelled");
        assert!(!kinds(&frames).contains(&"held".to_string()));
        owner.launcher_exit();
        assert!(
            !row_exists(&rig, &owner.runtime_id),
            "{barrier}: a cancelled decision publishes nothing"
        );
        let v = owner.settle_until(&rig, "closed");
        assert_eq!(
            verdict(&v),
            ("closed".into(), Some("never-launched".into())),
            "{barrier}"
        );
        assert!(marked(&owner.digest).is_empty());
    }
}

#[test]
fn a_cancel_while_a_ticket_is_issued_revokes_it() {
    for barrier in [
        "publish-issued-before-commit",
        "published-before-child-fork",
    ] {
        let rig = Rig::new();
        let mut owner = Owner::start(
            &rig,
            "revoke",
            Plan {
                hold: vec![barrier],
                ..Plan::default()
            },
        );
        owner.wait_reached(barrier);
        owner.cancel("c-revoke");
        // The actor decides and retires while the worker is still held.
        let decision = owner.wait_for("decision");
        let decision = if decision["decision"]["cause"] == "launch" {
            owner.wait_for("decision")
        } else {
            decision
        };
        assert_eq!(decision["decision"]["cause"], "cancel", "{barrier}");
        let retirement = owner.wait_for("retirement");
        assert_eq!(
            retirement["retirement"]["phase"], "verified",
            "{barrier}: {retirement:#}"
        );
        assert!(
            owner.reached(barrier) && !owner.barriers.join(format!("{barrier}.release")).exists()
        );
        let revoked = retirement["tickets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["state"] == "revoked");
        assert!(revoked, "{barrier}: {retirement:#}");
        let daemon = daemon_of(&retirement);
        owner.release(barrier);
        owner.drain();
        owner.launcher_exit();
        // No effect happens after a revoked ticket: the worker, released,
        // asks to commit, is refused, and the daemon ends without publishing
        // or forking.
        wait_until("the daemon to end", || {
            !pty_core::registry::pid_alive(daemon)
        });
        assert!(!row_exists(&rig, &owner.runtime_id), "{barrier}");
        assert!(!owner.ran());
        let v = owner.settle_until(&rig, "closed");
        assert_eq!(
            verdict(&v),
            ("closed".into(), Some("retired".into())),
            "{barrier}"
        );
    }
}

#[test]
fn a_cancel_while_a_ticket_is_committed_waits_for_its_effect() {
    for barrier in [
        "publish-committed-before-effect",
        "child-forked-before-record",
    ] {
        let rig = Rig::new();
        let mut owner = Owner::start(
            &rig,
            "commit",
            Plan {
                hold: vec![barrier],
                ..Plan::default()
            },
        );
        owner.wait_reached(barrier);
        owner.cancel("c-commit");
        let mut cause = Value::Null;
        while cause != "cancel" {
            cause = owner.wait_for("decision")["decision"]["cause"].clone();
        }
        // The request is durable before it is acknowledged, and the
        // committed effect is still in flight: the retirement is requested,
        // not finished.
        let record = owner.record(&rig);
        assert_eq!(record["retirement"]["phase"], "requested", "{barrier}");
        assert_eq!(record["retirement"]["requestId"], "c-commit", "{barrier}");
        assert!(
            owner.next(Duration::from_millis(500)).is_none(),
            "{barrier}: {:#?}",
            owner.frames
        );
        owner.release(barrier);
        let retirement = owner.wait_for("retirement");
        assert_eq!(
            retirement["retirement"]["phase"], "verified",
            "{barrier}: {retirement:#}"
        );
        owner.drain();
        owner.launcher_exit();
        assert!(!row_exists(&rig, &owner.runtime_id), "{barrier}");
        assert!(!owner.ran());
        assert!(
            marked(&owner.digest).is_empty() || {
                std::thread::sleep(Duration::from_millis(500));
                marked(&owner.digest).is_empty()
            }
        );
        let v = owner.settle_until(&rig, "closed");
        assert_eq!(
            verdict(&v),
            ("closed".into(), Some("retired".into())),
            "{barrier}"
        );
    }
}

#[test]
fn a_cancel_after_preparation_retires_without_running_the_command() {
    for barrier in ["child-prepared-before-ack-read", "before-ready"] {
        let rig = Rig::new();
        let mut owner = Owner::start(
            &rig,
            "prepared",
            Plan {
                hold: vec![barrier],
                ..Plan::default()
            },
        );
        owner.wait_reached(barrier);
        owner.cancel("c-prep");
        let retirement = owner.wait_for("retirement");
        assert_eq!(
            retirement["retirement"]["phase"], "verified",
            "{barrier}: {retirement:#}"
        );
        owner.release(barrier);
        let rest = owner.drain();
        assert!(
            !kinds(&owner.frames).contains(&"ready".to_string()),
            "{barrier}: {rest:#?}"
        );
        owner.launcher_exit();
        assert!(!row_exists(&rig, &owner.runtime_id));
        assert!(!owner.ran());
        let v = owner.settle_until(&rig, "closed");
        assert_eq!(
            verdict(&v),
            ("closed".into(), Some("retired".into())),
            "{barrier}"
        );
    }
}

#[test]
fn owner_end_of_file_after_ready_retires() {
    let rig = Rig::new();
    let mut owner = Owner::start(&rig, "eof", Plan::default());
    owner.wait_for("ready");
    owner.socket.shutdown(std::net::Shutdown::Write).unwrap();
    let decision = owner.wait_for("decision");
    assert_eq!(decision["decision"]["cause"], "owner-eof");
    assert_eq!(
        owner.wait_for("retirement")["retirement"]["phase"],
        "verified"
    );
    assert!(!owner.ran());
    let v = owner.settle_until(&rig, "closed");
    assert_eq!(verdict(&v), ("closed".into(), Some("retired".into())));
}

#[test]
fn malformed_input_before_the_decision_cancels() {
    let rig = Rig::new();
    let mut owner = Owner::start(
        &rig,
        "malformed",
        Plan {
            queued: vec!["{not json".into()],
            ..Plan::default()
        },
    );
    let frames = owner.drain();
    let decision = frames
        .iter()
        .find(|f| f["type"] == "decision")
        .expect("a decision");
    assert_eq!(decision["decision"]["cause"], "malformed");
    assert!(!row_exists(&rig, &owner.runtime_id));
}

#[test]
fn a_cancel_split_across_writes_is_one_frame() {
    let rig = Rig::new();
    let mut owner = Owner::start(&rig, "partial", Plan::default());
    owner.wait_for("ready");
    let line = owner.cancel_line("c-split");
    let (a, b) = line.split_at(line.len() / 2);
    owner.socket.write_all(a.as_bytes()).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    owner.socket.write_all(format!("{b}\n").as_bytes()).unwrap();
    let decision = owner.wait_for("decision");
    assert_eq!(decision["decision"]["cause"], "cancel");
    assert_eq!(decision["decision"]["requestId"], "c-split");
    owner.wait_for("retirement");
}

// ── Rejected commands ────────────────────────────────────────────────────

#[test]
fn wrong_commands_are_rejected_and_change_nothing() {
    let rig = Rig::new();
    let mut owner = Owner::start(&rig, "wrong", Plan::default());
    let ready = owner.wait_for("ready");
    let incarnation = ready["incarnation"].clone();
    let revision = ready["recordRevision"].clone();
    let cases = [
        (
            json!({"type":"cancel","version":1,"operationId":"other","envelopeSha256":owner.sha,"requestId":"r1"}),
            "wrong-operation",
        ),
        (
            json!({"type":"cancel","version":1,"operationId":owner.operation_id,"envelopeSha256":"0".repeat(64),"requestId":"r2"}),
            "wrong-envelope",
        ),
        (
            json!({"type":"transfer","version":1,"operationId":owner.operation_id,"envelopeSha256":owner.sha,"requestId":"r3","incarnation":{"id":"1:2026-10-13T10:00:00.000Z","generation":incarnation["generation"]}}),
            "wrong-incarnation",
        ),
        (
            json!({"type":"transfer","version":1,"operationId":owner.operation_id,"envelopeSha256":owner.sha,"requestId":"r4","incarnation":{"id":incarnation["id"],"generation":"other"}}),
            "wrong-incarnation",
        ),
        (
            json!({"type":"resume","version":1,"operationId":owner.operation_id,"envelopeSha256":owner.sha,"requestId":"r5"}),
            "unknown-command",
        ),
    ];
    for (frame, code) in cases {
        owner.write(&frame.to_string());
        let rejected = owner.wait_for("command-rejected");
        assert_eq!(rejected["code"], code, "{frame}");
        assert_eq!(
            rejected["recordRevision"], revision,
            "a rejected command writes nothing"
        );
    }
    assert!(!owner.ran());
    // Still held and transferable.
    owner.transfer("t-ok", &incarnation);
    owner.wait_for("transferred");
    wait_until("the command to run", || owner.ran());
    rig.ok(&["kill", &owner.runtime_id]);
}

#[test]
fn a_transfer_before_the_child_is_held_is_rejected() {
    let rig = Rig::new();
    let mut owner = Owner::start(
        &rig,
        "early-transfer",
        Plan {
            hold: vec!["child-prepared-before-ack-read"],
            ..Plan::default()
        },
    );
    owner.wait_reached("child-prepared-before-ack-read");
    let record = owner.record(&rig);
    owner.transfer("t-early", &record["incarnation"]);
    let rejected = owner.wait_for("command-rejected");
    assert_eq!(rejected["code"], "not-held");
    owner.release("child-prepared-before-ack-read");
    let ready = owner.wait_for("ready");
    owner.transfer("t-late", &ready["incarnation"]);
    owner.wait_for("transferred");
    wait_until("the command to run", || owner.ran());
    rig.ok(&["kill", &owner.runtime_id]);
}

#[test]
fn retire_refuses_mismatches_and_a_live_writer() {
    let rig = Rig::new();
    let mut owner = Owner::start(&rig, "retire-refuse", Plan::default());
    let ready = owner.wait_for("ready");
    let inc = ready["incarnation"]["id"].as_str().unwrap().to_string();
    let (code, v) = owner.retire(&rig, "r", Some(&inc));
    assert_eq!((code, v["code"].clone()), (1, json!("live-writer")));
    let out = rig.run(&[
        "operation",
        "retire",
        &owner.operation_id,
        "--envelope-sha256",
        &"0".repeat(64),
        "--request-id",
        "r",
        "--json",
    ]);
    assert_eq!(out.code, 1);
    // Settle meanwhile reports a live writer, and only observes.
    let v = owner.settle(&rig);
    assert_eq!(verdict(&v), ("live".into(), Some("writer".into())));
    owner.cancel("c");
    owner.wait_for("retirement");
}

// ── Deaths at each phase ─────────────────────────────────────────────────

#[test]
fn the_launcher_dying_after_its_claim_leaves_a_retirable_operation() {
    let rig = Rig::new();
    let mut owner = Owner::start(
        &rig,
        "launcher-dies",
        Plan {
            hold: vec!["launcher-before-fork"],
            ..Plan::default()
        },
    );
    owner.wait_reached("launcher-before-fork");
    let launcher = owner.holder("launcher-before-fork");
    kill(launcher, libc::SIGKILL);
    owner.launcher_exit();
    let v = owner.settle_until(&rig, "retire-required");
    assert_eq!(v["kind"], "pending");
    assert_eq!(v["noFurtherEffects"], false);
    let (code, v) = owner.retire(&rig, "r-lost", None);
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["record"]["decision"]["cause"], "producer-lost");
    let v = owner.settle_until(&rig, "closed");
    assert_eq!(
        verdict(&v),
        ("closed".into(), Some("never-launched".into()))
    );
    // The same request again: the recorded result, no new effect.
    let revision = owner.record(&rig)["revision"].clone();
    let (code, again) = owner.retire(&rig, "r-lost", None);
    assert_eq!(code, 0);
    assert_eq!(again["record"]["revision"], revision);
}

#[test]
fn a_gated_daemon_exits_when_its_launcher_dies() {
    let rig = Rig::new();
    let mut owner = Owner::start(
        &rig,
        "gate-eof",
        Plan {
            hold: vec!["daemon-gated"],
            ..Plan::default()
        },
    );
    owner.wait_reached("daemon-gated");
    let record = owner.record(&rig);
    let daemon = record["participants"]["producer"]["pid"].as_i64().unwrap() as i32;
    assert!(alive(daemon));
    kill(owner.holder("daemon-gated"), libc::SIGKILL);
    owner.launcher_exit();
    wait_gone(daemon);
    assert!(!row_exists(&rig, &owner.runtime_id));
    // The daemon was recorded as writer and is gone without acting.
    let v = owner.settle_until(&rig, "retire-required");
    assert_eq!(v["kind"], "pending");
    let (code, v) = owner.retire(&rig, "r-gate", None);
    assert_eq!(code, 0, "{v:#}");
    let v = owner.settle_until(&rig, "closed");
    assert_eq!(
        verdict(&v),
        ("closed".into(), Some("never-launched".into()))
    );
}

#[test]
fn the_daemon_dying_before_its_decision_is_producer_lost() {
    let rig = Rig::new();
    let mut owner = Owner::start(
        &rig,
        "daemon-dies",
        Plan {
            hold: vec!["before-decision"],
            ..Plan::default()
        },
    );
    owner.wait_reached("before-decision");
    kill(owner.holder("before-decision"), libc::SIGKILL);
    owner.launcher_exit();
    let v = owner.settle_until(&rig, "retire-required");
    assert_eq!(v["kind"], "pending");
    let (code, v) = owner.retire(&rig, "r-dd", None);
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["record"]["state"], "cancelled");
    // A process of this user caught exiting reads as unknown for a moment;
    // closure is what settle reaches, never what it assumes.
    let v = owner.settle_until(&rig, "closed");
    assert_eq!(
        verdict(&v),
        ("closed".into(), Some("never-launched".into()))
    );
    assert!(!row_exists(&rig, &owner.runtime_id));
}

#[test]
fn the_daemon_dying_while_the_child_is_held_never_runs_the_command() {
    let rig = Rig::new();
    let mut owner = Owner::start(&rig, "held-death", Plan::default());
    let ready = owner.wait_for("ready");
    let inc = ready["incarnation"].clone();
    let daemon = inc["daemon"]["pid"].as_i64().unwrap() as i32;
    let child = inc["child"]["pid"].as_i64().unwrap() as i32;
    assert!(alive(child));
    kill(daemon, libc::SIGKILL);
    // End of file on its gate: the child exits without exec. The launcher,
    // which may not have seen readiness yet, is a participant until it ends.
    wait_gone(child);
    owner.launcher_exit();
    assert!(!owner.ran());
    assert!(marked(&owner.digest).is_empty());
    let v = owner.settle_until(&rig, "retire-required");
    assert_eq!(v["kind"], "launched");
    let (code, refusal) = owner.retire(&rig, "r-held", None);
    assert_eq!(
        (code, refusal["code"].clone()),
        (1, json!("incarnation-required"))
    );
    let (code, v) = owner.retire(&rig, "r-held", inc["id"].as_str());
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["record"]["retirement"]["phase"], "verified");
    let v = owner.settle_until(&rig, "closed");
    assert_eq!(verdict(&v), ("closed".into(), Some("retired".into())));
    assert!(!row_exists(&rig, &owner.runtime_id));
}

#[test]
fn a_release_lost_with_the_daemon_leaves_transferred_unreleased() {
    let rig = Rig::new();
    let mut owner = Owner::start(
        &rig,
        "lost-release",
        Plan {
            hold: vec!["transfer-committed-before-release"],
            ..Plan::default()
        },
    );
    let ready = owner.wait_for("ready");
    let inc = ready["incarnation"].clone();
    owner.transfer("t-lost", &inc);
    owner.wait_reached("transfer-committed-before-release");
    kill(inc["daemon"]["pid"].as_i64().unwrap() as i32, libc::SIGKILL);
    wait_gone(inc["child"]["pid"].as_i64().unwrap() as i32);
    assert!(!owner.ran());
    let v = owner.settle(&rig);
    assert_eq!(verdict(&v), ("transferred".into(), None));
    assert!(v["record"]["transfer"]["released"].is_null());
}

// ── The held child ───────────────────────────────────────────────────────

#[test]
fn a_child_stopped_while_preparing_is_never_ready_and_cancel_retires_it() {
    let rig = Rig::new();
    let mut owner = Owner::start(
        &rig,
        "stopped",
        Plan {
            env: vec![("PTY_TEST_HELD_CHILD".into(), "stop-before-ack".into())],
            ..Plan::default()
        },
    );
    owner.wait_for("decision");
    let record = loop {
        let record = owner.record(&rig);
        if !record["participants"]["child"].is_null() {
            break record;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let child = record["participants"]["child"]["pid"].as_i64().unwrap() as i32;
    wait_until(
        "the child to stop",
        || matches!(pty_core::proctable::process(child), pty_core::proctable::Answer::Known(row) if row.is_stopped()),
    );
    #[cfg(target_os = "linux")]
    {
        // Before its acknowledgement the child holds exactly 0, 1, 2 and its
        // gate: nothing of the daemon's.
        let mut fds: Vec<i32> = std::fs::read_dir(format!("/proc/{child}/fd"))
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
            .collect();
        fds.sort();
        assert_eq!(fds.len(), 4, "{fds:?}");
        assert_eq!(&fds[..3], &[0, 1, 2]);
        let target = |fd: i32| std::fs::read_link(format!("/proc/{child}/fd/{fd}")).unwrap();
        assert!(target(fds[3]).to_string_lossy().starts_with("socket:"));
        let tty = target(0);
        assert!(tty.to_string_lossy().starts_with("/dev/pts/"), "{tty:?}");
        assert_eq!(target(1), tty);
        assert_eq!(target(2), tty);
        // Its own session and process group.
        let stat = std::fs::read_to_string(format!("/proc/{child}/stat")).unwrap();
        let fields: Vec<&str> = stat[stat.rfind(')').unwrap() + 2..]
            .split_whitespace()
            .collect();
        assert_eq!(fields[2], child.to_string(), "pgid");
        assert_eq!(fields[3], child.to_string(), "sid");
    }
    assert!(
        owner
            .next(Duration::from_secs(1))
            .is_none_or(|f| f["type"] != "held")
    );
    owner.cancel("c-stopped");
    let retirement = owner.wait_for("retirement");
    assert_eq!(
        retirement["retirement"]["phase"], "verified",
        "{retirement:#}"
    );
    assert!(!alive(child));
    assert!(!kinds(&owner.frames).contains(&"ready".to_string()));
    let v = owner.settle_until(&rig, "closed");
    assert_eq!(verdict(&v), ("closed".into(), Some("retired".into())));
}

#[test]
fn a_group_stop_reaches_the_prepared_child_and_cancel_still_retires_it() {
    let rig = Rig::new();
    let mut owner = Owner::start(&rig, "group", Plan::default());
    let ready = owner.wait_for("ready");
    let child = ready["incarnation"]["child"]["pid"].as_i64().unwrap() as i32;
    // The prepared child leads its own process group.
    kill(-child, libc::SIGSTOP);
    wait_until(
        "the group to stop",
        || matches!(pty_core::proctable::process(child), pty_core::proctable::Answer::Known(row) if row.is_stopped()),
    );
    owner.cancel("c-group");
    assert_eq!(
        owner.wait_for("retirement")["retirement"]["phase"],
        "verified"
    );
    assert!(!alive(child));
    assert!(!owner.ran());
}

#[test]
fn the_daemons_handlers_never_run_in_the_held_child() {
    let rig = Rig::new();
    let mut owner = Owner::start(&rig, "handlers", Plan::default());
    let ready = owner.wait_for("ready");
    let child = ready["incarnation"]["child"]["pid"].as_i64().unwrap() as i32;
    #[cfg(target_os = "linux")]
    {
        // The daemon is a Rust process with handlers of its own (the
        // runtime's stack-overflow guard, at least). The held child catches
        // nothing and blocks nothing before its exec.
        let status = std::fs::read_to_string(format!("/proc/{child}/status")).unwrap();
        let field = |name: &str| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .map(|value| u64::from_str_radix(value.trim(), 16).unwrap())
                .unwrap()
        };
        // glibc keeps signals 32 and 33 for its threads and refuses
        // sigaction on them, here and in today's spawned child alike; they
        // are the C library's, not the daemon's, and exec resets them.
        let glibc_reserved = (1u64 << 31) | (1u64 << 32);
        assert_eq!(
            field("SigCgt:") & !glibc_reserved,
            0,
            "a handler of the daemon's is installed in the held child"
        );
        assert_eq!(field("SigBlk:"), 0);
        let daemon = ready["incarnation"]["daemon"]["pid"].as_i64().unwrap();
        let daemon_status = std::fs::read_to_string(format!("/proc/{daemon}/status")).unwrap();
        assert!(
            daemon_status.lines().any(|l| l.starts_with("SigCgt:")
                && l.split_whitespace().nth(1) != Some("0000000000000000")),
            "the control: the daemon itself does catch signals"
        );
    }
    // SIGTERM, which the daemon catches once serving, is the default action
    // in the held child: it dies rather than running daemon code.
    kill(child, libc::SIGTERM);
    wait_gone(child);
    let retirement = owner.wait_for("retirement");
    assert_eq!(
        retirement["retirement"]["requestId"],
        "producer:child-exited"
    );
    assert!(!owner.ran());
    assert!(!row_exists(&rig, &owner.runtime_id));
}

#[test]
fn a_replaced_binary_cannot_affect_the_held_child() {
    let rig = Rig::new();
    let copy = rig.scratch.join("pty-copy");
    std::fs::copy(cli_common::pty_bin(), &copy).unwrap();
    let mut owner = Owner::start(
        &rig,
        "replaced",
        Plan {
            executable: Some(copy.clone()),
            ..Plan::default()
        },
    );
    let ready = owner.wait_for("ready");
    std::fs::remove_file(&copy).unwrap();
    std::fs::write(&copy, b"#!/bin/sh\nexit 99\n").unwrap();
    owner.transfer("t-replaced", &ready["incarnation"]);
    owner.wait_for("transferred");
    wait_until("the command to run", || owner.ran());
    rig.ok(&["kill", &owner.runtime_id]);
}

#[test]
fn the_released_command_holds_no_operation_descriptor() {
    let rig = Rig::new();
    let listing = rig.scratch.join("fds");
    let mut owner = Owner::start(
        &rig,
        "fds",
        Plan {
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                format!(
                    "ls /dev/fd > {}.tmp; mv {0}.tmp {0}; exec sleep 60",
                    listing.display()
                ),
            ],
            ..Plan::default()
        },
    );
    let ready = owner.wait_for("ready");
    owner.transfer("t-fds", &ready["incarnation"]);
    owner.wait_for("transferred");
    wait_until("the listing", || listing.exists());
    // `ls` itself opens one descriptor to read the directory.
    let fds: Vec<i32> = std::fs::read_to_string(&listing)
        .unwrap()
        .split_whitespace()
        .filter_map(|fd| fd.parse().ok())
        .collect();
    assert!(fds.iter().filter(|fd| **fd > 2).count() <= 1, "{fds:?}");
    rig.ok(&["kill", &owner.runtime_id]);
}

// ── Descriptor validation ────────────────────────────────────────────────

fn launch_with_fd(rig: &Rig, fd: std::os::fd::OwnedFd, target: i32) -> (i32, String) {
    let line = envelope("op-fd", "rt-fd", None);
    let digest = sha256_hex(b"op-fd");
    let mut cmd = rig.cmd(&[
        "run",
        "-d",
        "--operation-fd",
        &target.to_string(),
        "--launch-operation",
        &digest,
        "--id",
        "rt-fd",
        "--",
        "true",
    ]);
    inherit_as(&mut cmd, fd.as_raw_fd(), target);
    let out = cmd.output().unwrap();
    let _ = line;
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_descriptor_that_is_not_a_connected_stream_socket_is_refused_before_anything() {
    let rig = Rig::new();
    let file = std::fs::File::open("/dev/null").unwrap();
    let (code, err) = launch_with_fd(&rig, file.into(), 9);
    assert_eq!(code, 64, "{err}");
    assert!(err.contains("not a socket"), "{err}");
    let (a, _b) = std::os::unix::net::UnixDatagram::pair().unwrap();
    let (code, err) = launch_with_fd(&rig, a.into(), 9);
    assert_eq!(code, 64, "{err}");
    assert!(err.contains("SOCK_STREAM"), "{err}");
    let listener = std::os::unix::net::UnixListener::bind(rig.scratch.join("l.sock")).unwrap();
    let (code, err) = launch_with_fd(&rig, listener.into(), 9);
    assert_eq!(code, 64, "{err}");
    assert!(err.contains("not connected"), "{err}");
    assert!(!rig.path("operations").exists(), "nothing is recorded");
}

#[test]
fn descriptor_three_does_not_collide_with_the_config_descriptor() {
    let rig = Rig::new();
    let line = envelope("op-three", "rt-three", None);
    rig.run_stdin(&["operation", "reserve", "--json"], &format!("{line}\n"));
    let (mut owner_end, launcher_end) = UnixStream::pair().unwrap();
    let digest = sha256_hex(b"op-three");
    let mut cmd = rig.cmd(&[
        "run",
        "-d",
        "--operation-fd",
        "3",
        "--launch-operation",
        &digest,
        "--id",
        "rt-three",
        "--",
        "/bin/sh",
        "-c",
        "exec sleep 60",
    ]);
    inherit_as(&mut cmd, launcher_end.as_raw_fd(), 3);
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = cmd.spawn().unwrap();
    drop(launcher_end);
    owner_end.write_all(format!("{line}\n").as_bytes()).unwrap();
    let mut owner = Owner {
        socket: owner_end,
        buf: Vec::new(),
        sha: sha256_hex(line.as_bytes()),
        envelope: line,
        operation_id: "op-three".into(),
        runtime_id: "rt-three".into(),
        digest,
        launcher: None,
        barriers: rig.scratch.clone(),
        frames: Vec::new(),
        marker: rig.scratch.join("never"),
    };
    owner.wait_for("ready");
    assert_eq!(child.wait().unwrap().code(), Some(0));
    owner.cancel("c3");
    owner.wait_for("retirement");
}

// ── Settle evidence ──────────────────────────────────────────────────────

#[test]
fn settle_treats_unreadable_evidence_as_unknown() {
    let rig = Rig::new();
    let mut owner = Owner::start(&rig, "unknown", Plan::default());
    owner.wait_for("ready");
    owner.cancel("c");
    owner.wait_for("retirement");
    owner.launcher_exit();
    let v = owner.settle_until(&rig, "closed");
    assert_eq!(v["noFurtherEffects"], true);

    let v = owner.settle_env(&rig, &[("PTY_TEST_PROCESS_TABLE", "incomplete")]);
    assert_eq!(verdict(&v), ("unknown".into(), Some("participants".into())));
    let v = owner.settle_env(&rig, &[("PTY_TEST_PROCESS_TABLE", "unreadable")]);
    assert_eq!(verdict(&v), ("unknown".into(), Some("participants".into())));

    // A live process of this user whose argv cannot be read.
    let me = std::process::id().to_string();
    let v = owner.settle_env(&rig, &[("PTY_TEST_ARGV_UNREADABLE", &me)]);
    assert_eq!(verdict(&v), ("unknown".into(), Some("marker-scan".into())));
    assert!(
        component(&v, "marker-scan")["detail"]["unreadable"]
            .as_array()
            .unwrap()
            .contains(&json!(std::process::id()))
    );

    // An unreadable row anywhere in the root.
    std::fs::write(rig.path("other.json"), "{}").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        rig.path("other.json"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    // SAFETY: geteuid(2) has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        let v = owner.settle_until_kind(&rig, "unknown", "registry");
        assert_eq!(verdict(&v), ("unknown".into(), Some("registry".into())));
        // Unproven evidence is never a positive closure claim.
        assert_eq!(v["noFurtherEffects"], false);
    }
    std::fs::set_permissions(
        rig.path("other.json"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
}

#[test]
fn a_decoy_with_the_same_digest_keeps_the_operation_live_and_is_never_signalled() {
    let rig = Rig::new();
    let mut owner = Owner::start(&rig, "decoy", Plan::default());
    let ready = owner.wait_for("ready");
    let inc = ready["incarnation"]["id"].as_str().unwrap().to_string();
    owner.cancel("c");
    owner.wait_for("retirement");
    owner.launcher_exit();
    owner.settle_until(&rig, "closed");
    // Another digest: not a participant.
    let other = sha256_hex(b"someone-else");
    let mut unrelated = Command::new("/bin/sh")
        .args([
            "-c",
            "sleep 30; :",
            "pty",
            "__daemon",
            "--launch-operation",
            &other,
        ])
        .spawn()
        .unwrap();
    // `spawn` returns after the exec: the argv is in place.
    owner.settle_until(&rig, "closed");
    let _ = unrelated.kill();
    let _ = unrelated.wait();
    // The same digest, from anywhere: conservatively live, and left alone.
    let mut decoy = Command::new("/bin/sh")
        .args([
            "-c",
            "sleep 30; :",
            "pty",
            "__daemon",
            "--launch-operation",
            &owner.digest,
        ])
        .spawn()
        .unwrap();
    let v = owner.settle_until_kind(&rig, "live", "marker-scan");
    assert_eq!(verdict(&v), ("live".into(), Some("marker-scan".into())));
    let (code, v) = owner.retire(&rig, "r-decoy", Some(&inc));
    assert_eq!(code, 0, "{v:#}");
    assert!(
        decoy.try_wait().unwrap().is_none(),
        "the decoy was signalled"
    );
    let _ = decoy.kill();
    let _ = decoy.wait();
    owner.settle_until(&rig, "closed");
}

#[test]
fn retire_leaves_a_same_name_replacement_alone() {
    let rig = Rig::new();
    let mut owner = Owner::start(&rig, "same-name", Plan::default());
    let ready = owner.wait_for("ready");
    let inc = ready["incarnation"].clone();
    // The daemon dies with its row published.
    let daemon = inc["daemon"]["pid"].as_i64().unwrap() as i32;
    kill(daemon, libc::SIGKILL);
    wait_gone(inc["child"]["pid"].as_i64().unwrap() as i32);
    owner.launcher_exit();
    wait_until("the daemon to be reaped", || {
        !pty_core::registry::pid_alive(daemon)
    });
    // An ordinary session takes the name over.
    rig.spawn_cat(&owner.runtime_id, &[]);
    let replacement = rig.read_meta(&owner.runtime_id).unwrap();
    assert!(replacement.get("launchOperation").is_none());
    let (code, v) = owner.retire(&rig, "r-same", inc["id"].as_str());
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(
        v["record"]["retirement"]["evidence"]["row"],
        "other-generation"
    );
    let v = owner.settle_until(&rig, "closed");
    assert_eq!(verdict(&v), ("closed".into(), Some("retired".into())));
    assert_eq!(
        rig.read_meta(&owner.runtime_id).unwrap()["generation"],
        replacement["generation"]
    );
    rig.ok(&["kill", &owner.runtime_id]);
}

#[test]
fn a_second_launcher_for_a_claimed_operation_is_refused_without_a_write() {
    let rig = Rig::new();
    let mut first = Owner::start(&rig, "second", Plan::default());
    first.wait_for("ready");
    let path = rig
        .path("operations")
        .join(format!("{}.json", first.digest));
    let before = std::fs::read(&path).unwrap();
    // The same envelope through a second launcher.
    let mut second = Owner::start(
        &rig,
        "second",
        Plan {
            unreserved: true,
            ..Plan::default()
        },
    );
    let refused = second.wait_for("refused");
    assert_eq!(refused["code"], "not-claimable");
    assert_eq!(refused["observed"], "launched", "{refused:#}");
    assert_eq!(second.launcher_exit(), 1);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    first.cancel("c");
    first.wait_for("retirement");
}

#[test]
fn a_launch_without_a_reservation_is_refused() {
    let rig = Rig::new();
    let mut owner = Owner::start(
        &rig,
        "unreserved",
        Plan {
            unreserved: true,
            ..Plan::default()
        },
    );
    let refused = owner.wait_for("refused");
    assert_eq!(refused["code"], "not-claimable");
    assert_eq!(refused["observed"], "absent");
    assert_eq!(owner.launcher_exit(), 1);
    assert!(!row_exists(&rig, &owner.runtime_id));
}

#[test]
fn operation_mode_requires_its_arguments() {
    let rig = Rig::new();
    let digest = sha256_hex(b"x");
    for args in [
        vec![
            "run",
            "--operation-fd",
            "7",
            "--launch-operation",
            digest.as_str(),
            "--id",
            "a",
            "--",
            "true",
        ],
        vec![
            "run",
            "-d",
            "--operation-fd",
            "7",
            "--launch-operation",
            digest.as_str(),
            "--",
            "true",
        ],
        vec![
            "run",
            "-d",
            "--operation-fd",
            "7",
            "--id",
            "a",
            "--",
            "true",
        ],
        vec![
            "run",
            "-d",
            "--operation-fd",
            "7",
            "--launch-operation",
            "abc",
            "--id",
            "a",
            "--",
            "true",
        ],
        vec![
            "run",
            "-d",
            "--operation-fd",
            "2",
            "--launch-operation",
            digest.as_str(),
            "--id",
            "a",
            "--",
            "true",
        ],
        vec![
            "run",
            "-d",
            "-a",
            "--operation-fd",
            "7",
            "--launch-operation",
            digest.as_str(),
            "--id",
            "a",
            "--",
            "true",
        ],
    ] {
        let out = rig.run(&args);
        assert_eq!(out.code, 64, "{args:?}: {}", out.stderr);
    }
}

// ── Parity with today's spawn ────────────────────────────────────────────

/// What a command reports about the state it was started in. Run through
/// `/bin/sh` either way, so the shell's own effect is the same on both
/// sides.
fn probe_script(out: &Path) -> String {
    let linux = if cfg!(target_os = "linux") {
        // Signal mask and dispositions, and session, group and terminal,
        // straight from the kernel. The mask is read with builtins only: a
        // shell waiting on a child blocks every signal while it waits, so
        // reading it from a child would race that wait.
        "while IFS= read -r l; do case $l in SigBlk:*|SigIgn:*|SigCgt:*) echo \"$l\";; esac; done < /proc/$$/status; \
         awk '{ sub(/.*\\) /, \"\"); print \"pgid-is-pid\", ($3 == pid); print \"sid-is-pid\", ($4 == pid); print \"tty\", ($5 != 0) }' pid=$$ /proc/$$/stat; \
         ls /proc/$$/fd | sort -n | tr '\\n' ' '; echo;"
    } else {
        "ps -o pgid=,sess=,tty= -p $$ | awk '{ print \"pgid-is-pid\", ($1 == pid); print \"tty\", ($3 != \"??\") }' pid=$$; ls /dev/fd | sort -n | tr '\\n' ' '; echo;"
    };
    format!(
        "{{ echo umask $(umask); echo cwd \"$(pwd)\"; env | sort; {linux} }} > {out}.tmp; mv {out}.tmp {out}; exec sleep 60",
        out = out.display()
    )
}

/// Lines that legitimately differ between two sessions: their names,
/// generations and anything the test harness itself put in the
/// environment.
fn comparable(report: &str, names: &[&str]) -> Vec<String> {
    report
        .lines()
        .filter(|line| {
            !line.starts_with("PTY_SESSION_GENERATION=")
                && !line.starts_with("PTY_CREATION_LOCK_OWNER_PID=")
                && !line.starts_with("PTY_TEST_")
                && !line.starts_with("_=")
                && !line.starts_with("SHLVL=")
        })
        .map(|line| {
            let mut line = line.to_string();
            for name in names {
                line = line.replace(name, "<session>");
            }
            line
        })
        .collect()
}

/// Ignore `signal` in the launcher, as a caller under `nohup` or a job
/// manager might: the session child inherits that today unless spawn resets
/// it.
fn ignoring(cmd: &mut Command, signals: &'static [i32]) {
    // SAFETY: signal(2) in the child before exec.
    unsafe {
        cmd.pre_exec(move || {
            for signal in signals {
                libc::signal(*signal, libc::SIG_IGN);
            }
            Ok(())
        });
    }
}

#[test]
fn the_released_command_starts_exactly_as_todays_spawn_does() {
    const IGNORED: &[i32] = &[libc::SIGUSR1, libc::SIGHUP, libc::SIGPIPE];
    let rig = Rig::new();
    let today = rig.scratch.join("probe-today");
    let mut cmd = rig.cmd(&[
        "run",
        "-d",
        "--id",
        "parity-today",
        "--",
        "/bin/sh",
        "-c",
        &probe_script(&today),
    ]);
    ignoring(&mut cmd, IGNORED);
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    wait_until("today's probe", || today.exists());

    let held = rig.scratch.join("probe-held");
    let line = envelope("op-parity", "parity-held", None);
    assert_eq!(
        rig.run_stdin(&["operation", "reserve", "--json"], &format!("{line}\n"))
            .code,
        0
    );
    let (owner_end, launcher_end) = UnixStream::pair().unwrap();
    let digest = sha256_hex(b"op-parity");
    let mut cmd = rig.cmd(&[
        "run",
        "-d",
        "--operation-fd",
        &FD.to_string(),
        "--launch-operation",
        &digest,
        "--id",
        "parity-held",
        "--",
        "/bin/sh",
        "-c",
        &probe_script(&held),
    ]);
    inherit_as(&mut cmd, launcher_end.as_raw_fd(), FD);
    ignoring(&mut cmd, IGNORED);
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    let mut launcher = cmd.spawn().unwrap();
    drop(launcher_end);
    let mut owner = Owner {
        socket: owner_end,
        buf: Vec::new(),
        sha: sha256_hex(line.as_bytes()),
        envelope: line.clone(),
        operation_id: "op-parity".into(),
        runtime_id: "parity-held".into(),
        digest,
        launcher: None,
        barriers: rig.scratch.clone(),
        frames: Vec::new(),
        marker: held.clone(),
    };
    owner.write(&line);
    let ready = owner.wait_for("ready");
    assert!(!held.exists());
    owner.transfer("t-parity", &ready["incarnation"]);
    owner.wait_for("transferred");
    wait_until("the held probe", || held.exists());
    let _ = launcher.wait();

    let names = ["parity-today", "parity-held"];
    let today = comparable(&std::fs::read_to_string(&today).unwrap(), &names);
    let held = comparable(&std::fs::read_to_string(&held).unwrap(), &names);
    // Only the differing lines are printed: the rest is this machine's
    // environment, which a failure log has no need to carry.
    let only_today: Vec<&String> = today.iter().filter(|line| !held.contains(line)).collect();
    let only_held: Vec<&String> = held.iter().filter(|line| !today.contains(line)).collect();
    assert!(
        only_today.is_empty() && only_held.is_empty() && today.len() == held.len(),
        "the released command must start exactly as today's\n  only today: {only_today:?}\n  only held:  {only_held:?}"
    );
    assert!(
        today.iter().any(|line| line.starts_with("umask ")),
        "the probe ran: {}",
        today.len()
    );
    #[cfg(target_os = "linux")]
    {
        // The inherited ignore of SIGUSR1 survives both ways; SIGHUP and
        // SIGPIPE are reset both ways.
        let ignored = today.iter().find(|l| l.starts_with("SigIgn:")).unwrap();
        let mask = u64::from_str_radix(ignored.split_whitespace().nth(1).unwrap(), 16).unwrap();
        assert_ne!(mask & (1 << (libc::SIGUSR1 - 1)), 0, "{ignored}");
        assert_eq!(mask & (1 << (libc::SIGHUP - 1)), 0, "{ignored}");
        assert_eq!(mask & (1 << (libc::SIGPIPE - 1)), 0, "{ignored}");
        assert!(today.contains(&"sid-is-pid 1".to_string()));
        assert!(today.contains(&"tty 1".to_string()));
    }
    rig.ok(&["kill", "parity-today"]);
    rig.ok(&["kill", "parity-held"]);
}

// ── The actor keeps answering while a step is held ───────────────────────

#[test]
#[cfg_attr(not(debug_assertions), ignore = "barriers exist only in debug builds")]
fn a_withdrawal_held_at_the_decision_is_decided_while_held() {
    for (tag, withdraw, cause) in [
        ("held-cancel", "cancel", "cancel"),
        ("held-eof", "eof", "owner-eof"),
        ("held-malformed", "malformed", "malformed"),
    ] {
        let rig = Rig::new();
        let mut owner = Owner::start(
            &rig,
            tag,
            Plan {
                hold: vec!["before-decision"],
                ..Plan::default()
            },
        );
        owner.wait_for("accepted");
        owner.wait_reached("before-decision");
        match withdraw {
            "cancel" => owner.cancel("c-decision"),
            "eof" => owner.socket.shutdown(std::net::Shutdown::Write).unwrap(),
            _ => owner.write("{not json"),
        }
        // The decision is made while the barrier still holds.
        let decision = owner.wait_for("decision");
        assert_eq!(decision["decision"]["cause"], cause, "{tag}");
        assert_eq!(decision["state"], "cancelled", "{tag}");
        assert!(!owner.barriers.join("before-decision.release").exists());
        owner.release("before-decision");
        let rest = owner.drain();
        assert!(
            !kinds(&rest).contains(&"decision".to_string()),
            "{tag}: one decision only: {rest:#?}"
        );
        owner.launcher_exit();
        assert!(!row_exists(&rig, &owner.runtime_id), "{tag}");
        let v = owner.settle_until(&rig, "closed");
        assert_eq!(
            verdict(&v),
            ("closed".into(), Some("never-launched".into())),
            "{tag}"
        );
    }
}

#[test]
#[cfg_attr(not(debug_assertions), ignore = "barriers exist only in debug builds")]
fn a_transfer_between_held_and_ready_waits_for_ready_and_is_served() {
    let rig = Rig::new();
    let mut owner = Owner::start(
        &rig,
        "held-transfer",
        Plan {
            hold: vec!["before-ready"],
            ..Plan::default()
        },
    );
    let held = owner.wait_for("held");
    owner.wait_reached("before-ready");
    // A transfer is valid at phase held. It is accepted, and committed once
    // `ready` has gone out, so the daemon is serving when it releases.
    owner.transfer("t-early", &held["incarnation"]);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(owner.record(&rig)["state"], "launched");
    assert!(still_held(child_of(&held), &owner.digest));
    owner.release("before-ready");
    owner.wait_for("ready");
    let transferred = owner.wait_for("transferred");
    assert_eq!(transferred["transfer"]["requestId"], "t-early");
    assert_eq!(transferred["transfer"]["released"], true);
    wait_until("the command to run", || owner.ran());
    assert_eq!(owner.launcher_exit(), 0);
    // The daemon serves the transferred session.
    let daemon = daemon_of(&transferred);
    assert!(alive(daemon));
    let listed = rig.ok(&["list", "--json"]).json();
    let row = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == owner.runtime_id)
        .cloned()
        .expect("the session is listed");
    assert_eq!(row["status"], "running");
    std::os::unix::net::UnixStream::connect(rig.path(&format!("{}.sock", owner.runtime_id)))
        .expect("the daemon accepts clients");
    assert_eq!(owner.settle(&rig)["verdict"], "transferred");
    rig.ok(&["kill", &owner.runtime_id]);
}

#[test]
#[cfg_attr(not(debug_assertions), ignore = "barriers exist only in debug builds")]
fn a_withdrawal_before_the_transfer_commits_wins() {
    for (tag, withdraw, cause) in [
        ("tbc-cancel", "cancel", "cancel"),
        ("tbc-eof", "eof", "owner-eof"),
    ] {
        let rig = Rig::new();
        let mut owner = Owner::start(
            &rig,
            tag,
            Plan {
                hold: vec!["transfer-before-commit"],
                ..Plan::default()
            },
        );
        let ready = owner.wait_for("ready");
        owner.transfer("t-pending", &ready["incarnation"]);
        owner.wait_reached("transfer-before-commit");
        match withdraw {
            "cancel" => owner.cancel("c-pending"),
            _ => owner.socket.shutdown(std::net::Shutdown::Write).unwrap(),
        }
        // All while the transfer is held before its commit: the transfer
        // is refused, the cause acknowledged, and the retirement done.
        let rejected = owner.wait_for("command-rejected");
        assert_eq!(rejected["code"], "not-held", "{tag}");
        assert_eq!(rejected["requestId"], "t-pending", "{tag}");
        let decision = owner.wait_for("decision");
        assert_eq!(decision["decision"]["cause"], cause, "{tag}");
        let retirement = owner.wait_for("retirement");
        assert_eq!(retirement["retirement"]["phase"], "verified", "{tag}");
        assert!(
            !owner
                .barriers
                .join("transfer-before-commit.release")
                .exists()
        );
        owner.release("transfer-before-commit");
        let rest = owner.drain();
        assert!(!kinds(&rest).contains(&"transferred".to_string()), "{tag}");
        assert!(!owner.ran(), "{tag}");
        let v = owner.settle_until(&rig, "closed");
        assert_eq!(
            verdict(&v),
            ("closed".into(), Some("retired".into())),
            "{tag}"
        );
    }
}

#[test]
#[cfg_attr(not(debug_assertions), ignore = "barriers exist only in debug builds")]
fn after_the_transfer_commits_a_cancel_or_end_of_file_changes_nothing() {
    for barrier in ["transfer-committed-before-release", "released-before-ack"] {
        let rig = Rig::new();
        let mut owner = Owner::start(
            &rig,
            "after-commit",
            Plan {
                hold: vec![barrier],
                ..Plan::default()
            },
        );
        let ready = owner.wait_for("ready");
        owner.transfer("t-commit", &ready["incarnation"]);
        owner.wait_reached(barrier);
        assert_eq!(owner.record(&rig)["state"], "transferred", "{barrier}");
        // Frames after the commit are refused while the barrier holds.
        owner.cancel("c-late");
        let rejected = owner.wait_for("command-rejected");
        assert_eq!(rejected["code"], "already-transferred", "{barrier}");
        assert_eq!(rejected["requestId"], "c-late");
        owner.write(&json!({"type":"resume","version":1,"operationId":owner.operation_id,"envelopeSha256":owner.sha,"requestId":"r-late"}).to_string());
        assert_eq!(
            owner.wait_for("command-rejected")["code"],
            "already-transferred",
            "{barrier}"
        );
        // End of file after the commit is never permission to kill.
        owner.socket.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(!owner.barriers.join(format!("{barrier}.release")).exists());
        owner.release(barrier);
        let transferred = owner.wait_for("transferred");
        assert_eq!(transferred["transfer"]["released"], true, "{barrier}");
        wait_until("the command to run", || owner.ran());
        assert_eq!(owner.settle(&rig)["verdict"], "transferred");
        rig.ok(&["kill", &owner.runtime_id]);
    }
}

// ── A record write that does not land ────────────────────────────────────

/// Launch with the daemon's record write `label` made to fail.
fn faulted(rig: &Rig, tag: &str, label: &str, hold: Vec<&'static str>) -> Owner {
    Owner::start(
        rig,
        tag,
        Plan {
            hold,
            env: vec![("PTY_TEST_RECORD_WRITE_FAIL".into(), label.into())],
            ..Plan::default()
        },
    )
}

#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "write faults exist only in debug builds"
)]
fn a_retirement_whose_request_did_not_land_has_no_effect_and_no_acknowledgement() {
    let rig = Rig::new();
    let mut owner = faulted(&rig, "fault-request", "retirement-requested", vec![]);
    let ready = owner.wait_for("ready");
    let child = child_of(&ready);
    let daemon = daemon_of(&ready);
    owner.cancel("c-fault");
    // No acknowledgement and no retirement: the owner reads end of file.
    let rest = owner.drain();
    assert!(
        !kinds(&rest).contains(&"decision".to_string())
            && !kinds(&rest).contains(&"retirement".to_string()),
        "{rest:#?}"
    );
    wait_until("the daemon to stop", || {
        !pty_core::registry::pid_alive(daemon)
    });
    // Nothing was unwound: the row is still there, and nothing was signalled
    // (the child left on its own, at end of file on its gate).
    assert!(row_exists(&rig, &owner.runtime_id));
    wait_gone(child);
    assert!(!owner.ran());
    let record = owner.record(&rig);
    assert!(record["retirement"].is_null(), "{record:#}");
    let v = owner.settle_until(&rig, "retire-required");
    assert_eq!(v["kind"], "launched");
    let (code, v) = owner.retire(&rig, "r-after-fault", ready["incarnation"]["id"].as_str());
    assert_eq!(code, 0, "{v:#}");
    assert!(!row_exists(&rig, &owner.runtime_id));
    let v = owner.settle_until(&rig, "closed");
    assert_eq!(verdict(&v), ("closed".into(), Some("retired".into())));
}

#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "write faults exist only in debug builds"
)]
fn a_retirement_whose_terminal_write_did_not_land_claims_nothing() {
    let rig = Rig::new();
    let mut owner = faulted(&rig, "fault-terminal", "retirement-terminal", vec![]);
    let ready = owner.wait_for("ready");
    owner.cancel("c-terminal");
    let decision = owner.wait_for("decision");
    assert_eq!(decision["decision"]["cause"], "cancel");
    let rest = owner.drain();
    assert!(
        !kinds(&rest).contains(&"retirement".to_string()),
        "{rest:#?}"
    );
    let record = owner.record(&rig);
    assert_eq!(record["retirement"]["phase"], "requested");
    assert_eq!(record["retirement"]["requestId"], "c-terminal");
    // The writer is gone with a request it never finished: recovery resumes
    // that same request.
    let v = owner.settle_until(&rig, "retire-required");
    assert_eq!(v["kind"], "launched");
    let inc = ready["incarnation"]["id"].as_str().unwrap();
    let (code, v) = owner.retire(&rig, "c-terminal", Some(inc));
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["record"]["retirement"]["requestId"], "c-terminal");
    assert_eq!(v["record"]["retirement"]["phase"], "verified");
    let v = owner.settle_until(&rig, "closed");
    assert_eq!(verdict(&v), ("closed".into(), Some("retired".into())));
}

#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "write faults exist only in debug builds"
)]
fn a_decision_that_did_not_land_is_never_acknowledged() {
    let rig = Rig::new();
    let mut owner = faulted(&rig, "fault-decision", "decision", vec!["before-decision"]);
    owner.wait_reached("before-decision");
    owner.cancel("c-decision");
    let rest = owner.drain();
    assert!(!kinds(&rest).contains(&"decision".to_string()), "{rest:#?}");
    owner.release("before-decision");
    owner.launcher_exit();
    let v = owner.settle_until(&rig, "retire-required");
    assert_eq!(v["kind"], "pending");
    let (code, v) = owner.retire(&rig, "r-decision", None);
    assert_eq!(code, 0, "{v:#}");
    let v = owner.settle_until(&rig, "closed");
    assert_eq!(
        verdict(&v),
        ("closed".into(), Some("never-launched".into()))
    );
}

#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "write faults exist only in debug builds"
)]
fn a_transfer_whose_commit_did_not_land_never_releases_the_child() {
    let rig = Rig::new();
    let mut owner = faulted(&rig, "fault-transfer", "transfer-commit", vec![]);
    let ready = owner.wait_for("ready");
    owner.transfer("t-fault", &ready["incarnation"]);
    let rejected = owner.wait_for("command-rejected");
    assert_eq!(rejected["code"], "not-held");
    let rest = owner.drain();
    assert!(
        !kinds(&rest).contains(&"transferred".to_string()),
        "{rest:#?}"
    );
    wait_gone(child_of(&ready));
    assert!(!owner.ran());
    assert_eq!(owner.record(&rig)["state"], "launched");
    let v = owner.settle_until(&rig, "retire-required");
    assert_eq!(v["kind"], "launched");
    let (code, v) = owner.retire(&rig, "r-transfer", ready["incarnation"]["id"].as_str());
    assert_eq!(code, 0, "{v:#}");
    owner.settle_until(&rig, "closed");
}

#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "write faults exist only in debug builds"
)]
fn retire_checks_the_exact_tuple_before_replaying_and_resumes_its_own_request() {
    let rig = Rig::new();
    let mut owner = Owner::start(&rig, "replay", Plan::default());
    let ready = owner.wait_for("ready");
    let inc = ready["incarnation"]["id"].as_str().unwrap().to_string();
    kill(daemon_of(&ready), libc::SIGKILL);
    wait_gone(child_of(&ready));
    owner.settle_until(&rig, "retire-required");
    // Recovery's own terminal write fails: the request stays requested.
    let out = rig.run_env(
        &[
            "operation",
            "retire",
            &owner.operation_id,
            "--envelope-sha256",
            &owner.sha,
            "--request-id",
            "r-replay",
            "--incarnation",
            &inc,
            "--json",
        ],
        &[("PTY_TEST_RECORD_WRITE_FAIL", "recovery-terminal")],
    );
    assert_eq!(out.code, 1);
    assert_eq!(out.json()["code"], "record-write");
    let record = owner.record(&rig);
    assert_eq!(record["retirement"]["phase"], "requested");
    assert_eq!(record["writer"]["role"], "recovery");
    let before = owner.record(&rig);
    // The same request id with a missing or a wrong incarnation is refused
    // before any replay, and changes nothing.
    let (code, refusal) = owner.retire(&rig, "r-replay", None);
    assert_eq!(
        (code, refusal["code"].clone()),
        (1, json!("incarnation-required"))
    );
    let (code, refusal) = owner.retire(&rig, "r-replay", Some("1:2026-10-13T10:00:00.000Z"));
    assert_eq!(
        (code, refusal["code"].clone()),
        (1, json!("wrong-incarnation"))
    );
    assert_eq!(owner.record(&rig), before);
    // The exact tuple resumes the interrupted request.
    let (code, v) = owner.retire(&rig, "r-replay", Some(&inc));
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["record"]["retirement"]["requestId"], "r-replay");
    assert_eq!(v["record"]["retirement"]["phase"], "verified");
    owner.settle_until(&rig, "closed");
    // And replays of the finished request still check the tuple first.
    let (code, refusal) = owner.retire(&rig, "r-replay", None);
    assert_eq!(
        (code, refusal["code"].clone()),
        (1, json!("incarnation-required"))
    );
}

// ── Recovery's signal authority ──────────────────────────────────────────

#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "test hooks exist only in debug builds"
)]
fn a_stopped_child_orphaned_by_its_daemon_is_ended_by_job_control() {
    let rig = Rig::new();
    let mut owner = Owner::start(
        &rig,
        "orphan",
        Plan {
            env: vec![("PTY_TEST_HELD_CHILD".into(), "stop-before-ack".into())],
            ..Plan::default()
        },
    );
    owner.wait_for("decision");
    let record = loop {
        let record = owner.record(&rig);
        if !record["participants"]["child"].is_null() {
            break record;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let child = record["participants"]["child"]["pid"].as_i64().unwrap() as i32;
    wait_until(
        "the child to stop",
        || matches!(pty_core::proctable::process(child), pty_core::proctable::Answer::Known(row) if row.is_stopped()),
    );
    // A stopped child cannot read the end of file on its gate. But it leads
    // its session with the terminal the daemon's master serves: the daemon's
    // death hangs that terminal up, and the kernel sends the session leader
    // SIGHUP and SIGCONT. Its SIGHUP is at its default, so it ends.
    kill(
        record["participants"]["producer"]["pid"].as_i64().unwrap() as i32,
        libc::SIGKILL,
    );
    owner.launcher_exit();
    wait_gone(child);
    assert!(!owner.ran());
    let v = owner.settle_until(&rig, "retire-required");
    assert_eq!(v["kind"], "launched");
    let inc = owner.record(&rig)["incarnation"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (code, v) = owner.retire(&rig, "r-orphan", Some(&inc));
    assert_eq!(code, 0, "{v:#}");
    owner.settle_until(&rig, "closed");
}

/// A launched record, forged as a dead daemon would leave it, whose child is
/// `child` (or a process that only shares its pid when `token` differs).
fn forge_launched(
    rig: &Rig,
    operation_id: &str,
    child: i32,
    token: Option<&str>,
) -> (String, String) {
    let line = envelope(operation_id, &format!("rt-{operation_id}"), None);
    rig.run_stdin(&["operation", "reserve", "--json"], &format!("{line}\n"));
    let sha = sha256_hex(line.as_bytes());
    let path = rig
        .path("operations")
        .join(format!("{}.json", sha256_hex(operation_id.as_bytes())));
    let live_token = match pty_core::proctable::process(child) {
        pty_core::proctable::Answer::Known(row) => row.identity.unwrap().as_str().to_string(),
        other => panic!("{other:?}"),
    };
    let boot = pty_core::boot::read_boot_identity().unwrap();
    // SAFETY: getuid(2) has no preconditions.
    let uid = unsafe { libc::getuid() };
    let dead = json!({"pid": 2147483646, "startToken": "linux:1", "bootId": boot, "uid": uid});
    let child_id = json!({"pid": child, "startToken": token.unwrap_or(&live_token), "bootId": boot, "uid": uid});
    let inc = "2147483646:2026-10-13T10:00:00.000Z".to_string();
    let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    record["state"] = json!("launched");
    record["phase"] = json!("held");
    record["revision"] = json!(9);
    record["writer"] = json!({"role": "producer", "id": dead});
    record["decision"] = json!({"cause": "launch", "at": "2026-10-13T10:00:00.000Z", "requestId": null, "observed": null});
    record["participants"] = json!({"launcher": dead, "producer": dead, "child": child_id});
    record["incarnation"] = json!({"id": inc, "runtimeId": format!("rt-{operation_id}"), "generation": "g-forged", "daemon": dead, "child": child_id});
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    (sha, inc)
}

#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "test hooks exist only in debug builds"
)]
fn recovery_signals_only_a_proven_identity() {
    let rig = Rig::new();
    let mut child = Command::new("sleep").arg("30").spawn().unwrap();
    let pid = child.id() as i32;
    let (sha, inc) = forge_launched(&rig, "op-proof", pid, None);
    let retire = |env: &[(&str, &str)]| {
        rig.run_env(
            &[
                "operation",
                "retire",
                "op-proof",
                "--envelope-sha256",
                &sha,
                "--request-id",
                "r-proof",
                "--incarnation",
                &inc,
                "--json",
            ],
            env,
        )
    };
    // No boot identity: nothing can be proven, so nothing is signalled.
    let v = rig
        .run_env(
            &[
                "operation",
                "settle",
                "op-proof",
                "--envelope-sha256",
                &sha,
                "--json",
            ],
            &[("PTY_TEST_BOOT_ID", "unavailable")],
        )
        .json();
    assert_eq!(v["verdict"], "unknown", "{v:#}");
    let out = retire(&[("PTY_TEST_BOOT_ID", "unavailable")]);
    assert_eq!(out.code, 1, "{}", out.stdout);
    assert_eq!(out.json()["code"], "participant-unknown");
    assert!(
        child.try_wait().unwrap().is_none(),
        "signalled without a proven identity"
    );
    // Proven: the recorded child is ended by its exact identity.
    let out = retire(&[]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let v = out.json();
    assert_eq!(
        v["record"]["retirement"]["evidence"]["child"]["status"],
        "gone"
    );
    let status = child.wait().unwrap();
    assert!(!status.success(), "the recorded child was killed");

    // The same pid with another start token is another process: it is never
    // signalled, and the recorded one is simply gone.
    let mut other = Command::new("sleep").arg("30").spawn().unwrap();
    let (sha, inc) = forge_launched(&rig, "op-reuse", other.id() as i32, Some("linux:1"));
    let out = rig.run(&[
        "operation",
        "retire",
        "op-reuse",
        "--envelope-sha256",
        &sha,
        "--request-id",
        "r-reuse",
        "--incarnation",
        &inc,
        "--json",
    ]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(
        out.json()["record"]["retirement"]["evidence"]["child"]["status"],
        "gone"
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        other.try_wait().unwrap().is_none(),
        "a reused pid was signalled"
    );
    let _ = other.kill();
    let _ = other.wait();
}

#[test]
fn retire_refuses_while_a_recorded_participant_lives() {
    let rig = Rig::new();
    let line = envelope("op-participant", "rt-participant", None);
    rig.run_stdin(&["operation", "reserve", "--json"], &format!("{line}\n"));
    let sha = sha256_hex(line.as_bytes());
    let path = rig
        .path("operations")
        .join(format!("{}.json", sha256_hex(b"op-participant")));
    // A recorded launcher that is still alive, behind a writer that is gone:
    // what a recovery that died part-way leaves when the launcher outlived it.
    let mut launcher = Command::new("sleep").arg("30").spawn().unwrap();
    let pid = launcher.id() as i32;
    let token = match pty_core::proctable::process(pid) {
        pty_core::proctable::Answer::Known(row) => row.identity.unwrap().as_str().to_string(),
        other => panic!("{other:?}"),
    };
    let boot = pty_core::boot::read_boot_identity().unwrap();
    // SAFETY: getuid(2) has no preconditions.
    let uid = unsafe { libc::getuid() };
    let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    record["state"] = json!("pending");
    record["phase"] = json!("claimed");
    record["revision"] = json!(5);
    record["participants"]["launcher"] =
        json!({"pid": pid, "startToken": token, "bootId": boot, "uid": uid});
    record["writer"] = json!({"role": "recovery", "id": {"pid": 2147483646, "startToken": "linux:1", "bootId": boot, "uid": uid}});
    record["retirement"] = json!({"requestId": "r-old", "phase": "requested", "evidence": {}});
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let before = std::fs::read(&path).unwrap();
    let out = rig.run(&[
        "operation",
        "retire",
        "op-participant",
        "--envelope-sha256",
        &sha,
        "--request-id",
        "r-new",
        "--json",
    ]);
    assert_eq!(out.code, 1, "{}", out.stdout);
    assert_eq!(out.json()["code"], "live-participant");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "a refusal writes nothing"
    );
    let _ = launcher.kill();
    let _ = launcher.wait();
    let out = rig.run(&[
        "operation",
        "retire",
        "op-participant",
        "--envelope-sha256",
        &sha,
        "--request-id",
        "r-new",
        "--json",
    ]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.json()["record"]["decision"]["cause"], "producer-lost");

    // An inconsistent record (a writer role with no identity) is refused
    // outright: nothing may act on it.
    let mut broken = record.clone();
    broken["writer"] = json!({"role": "launcher", "id": null});
    broken["retirement"] = Value::Null;
    std::fs::write(&path, serde_json::to_vec(&broken).unwrap()).unwrap();
    let out = rig.run(&[
        "operation",
        "retire",
        "op-participant",
        "--envelope-sha256",
        &sha,
        "--request-id",
        "r-3",
        "--json",
    ]);
    assert_eq!(out.json()["code"], "record-unreadable");
    let v = rig
        .ok(&[
            "operation",
            "settle",
            "op-participant",
            "--envelope-sha256",
            &sha,
            "--json",
        ])
        .json();
    assert_eq!(v["verdict"], "unknown");
}

// ── Descriptors and frames ───────────────────────────────────────────────

#[test]
fn a_leaked_copy_of_the_owners_end_does_not_hide_its_end_of_file() {
    let rig = Rig::new();
    let line = envelope("op-leak", "rt-leak", None);
    assert_eq!(
        rig.run_stdin(&["operation", "reserve", "--json"], &format!("{line}\n"))
            .code,
        0
    );
    let (owner_end, launcher_end) = UnixStream::pair().unwrap();
    let digest = sha256_hex(b"op-leak");
    let mut cmd = rig.cmd(&[
        "run",
        "-d",
        "--operation-fd",
        &FD.to_string(),
        "--launch-operation",
        &digest,
        "--id",
        "rt-leak",
        "--",
        "/bin/sh",
        "-c",
        "exec sleep 60",
    ]);
    // The launcher is handed the protocol end AND an inheritable copy of the
    // owner's own end, as a careless spawner might.
    inherit_as(&mut cmd, launcher_end.as_raw_fd(), FD);
    inherit_as(&mut cmd, owner_end.as_raw_fd(), 8);
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    let mut launcher = cmd.spawn().unwrap();
    drop(launcher_end);
    let mut owner = Owner {
        socket: owner_end,
        buf: Vec::new(),
        sha: sha256_hex(line.as_bytes()),
        envelope: line.clone(),
        operation_id: "op-leak".into(),
        runtime_id: "rt-leak".into(),
        digest,
        launcher: None,
        barriers: rig.scratch.clone(),
        frames: Vec::new(),
        marker: rig.scratch.join("never"),
    };
    owner.write(&line);
    let ready = owner.wait_for("ready");
    let _ = &ready;
    assert_eq!(launcher.wait().unwrap().code(), Some(0));
    // The daemon holds no copy of the owner's end.
    #[cfg(target_os = "linux")]
    {
        let daemon = daemon_of(&ready);
        let mine =
            std::fs::read_link(format!("/proc/self/fd/{}", owner.socket.as_raw_fd())).unwrap();
        let theirs: Vec<std::path::PathBuf> = std::fs::read_dir(format!("/proc/{daemon}/fd"))
            .unwrap()
            .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
            .collect();
        assert!(
            !theirs.contains(&mine),
            "the daemon inherited the owner's own end {mine:?}"
        );
    }
    // The real owner goes away: the daemon must see end of file and retire.
    drop(std::mem::replace(
        &mut owner.socket,
        UnixStream::pair().unwrap().0,
    ));
    let v = owner.settle_until(&rig, "closed");
    assert_eq!(verdict(&v), ("closed".into(), Some("retired".into())));
    assert_eq!(v["record"]["retirement"]["requestId"], "owner:eof");
}

#[test]
fn unsupported_and_oversize_live_frames_are_malformed_and_retire() {
    for (tag, frame) in [
        (
            "version-two",
            Some(r#"{"type":"cancel","version":2,"operationId":"x","envelopeSha256":"y","requestId":"r"}"#.to_string()),
        ),
        ("oversize", None),
    ] {
        let rig = Rig::new();
        let mut owner = Owner::start(&rig, tag, Plan::default());
        owner.wait_for("ready");
        match frame {
            Some(frame) => owner.write(&frame),
            None => {
                let huge = vec![b'x'; 129 * 1024];
                owner.socket.write_all(&huge).unwrap();
            }
        }
        let rejected = owner.wait_for("command-rejected");
        assert_eq!(rejected["code"], "malformed", "{tag}");
        let decision = owner.wait_for("decision");
        assert_eq!(decision["decision"]["cause"], "malformed", "{tag}");
        assert_eq!(
            owner.wait_for("retirement")["retirement"]["phase"],
            "verified",
            "{tag}"
        );
        assert!(!owner.ran());
    }
}

#[test]
fn settle_creates_nothing() {
    let rig = Rig::new();
    let sha = "0".repeat(64);
    let out = rig.run(&[
        "operation",
        "settle",
        "op-none",
        "--envelope-sha256",
        &sha,
        "--json",
    ]);
    assert_eq!(out.code, 0);
    assert_eq!(out.json()["kind"], "record-missing");
    assert!(!rig.path("operations").exists(), "settle created the store");
    let out = rig.run(&[
        "operation",
        "retire",
        "op-none",
        "--envelope-sha256",
        &sha,
        "--request-id",
        "r",
        "--json",
    ]);
    assert_eq!(out.json()["code"], "record-missing");
    assert!(!rig.path("operations").exists(), "retire created the store");

    // A readable store that may not be written: settle still answers.
    let line = envelope("op-ro", "rt-ro", None);
    rig.run_stdin(&["operation", "reserve", "--json"], &format!("{line}\n"));
    let sha = sha256_hex(line.as_bytes());
    use std::os::unix::fs::PermissionsExt;
    let dir = rig.path("operations");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let lock = dir.join(format!("{}.lock", sha256_hex(b"op-ro")));
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o400)).unwrap();
    let v = settle_reactively(&rig, "op-ro", &sha, "retire-required", Some("reserved"));
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        verdict(&v),
        ("retire-required".into(), Some("reserved".into()))
    );
}

// ── A death at every barrier ─────────────────────────────────────────────

#[test]
#[cfg_attr(not(debug_assertions), ignore = "barriers exist only in debug builds")]
fn a_death_at_every_barrier_settles_and_retires() {
    for barrier in [
        "launcher-before-claim",
        "launcher-before-peek",
        "launcher-before-fork",
        "daemon-gated",
        "before-decision",
        "publish-issued-before-commit",
        "publish-committed-before-effect",
        "published-before-child-fork",
        "child-forked-before-record",
        "child-prepared-before-ack-read",
        "before-ready",
    ] {
        let rig = Rig::new();
        let mut owner = Owner::start(
            &rig,
            "death",
            Plan {
                hold: vec![barrier],
                ..Plan::default()
            },
        );
        owner.wait_reached(barrier);
        let holder = owner.holder(barrier);
        kill(holder, libc::SIGKILL);
        owner.launcher_exit();
        wait_until(&format!("{barrier}: the holder to be reaped"), || {
            !pty_core::registry::pid_alive(holder)
        });
        // Settle never calls it closed on its own: a reservation stays
        // claimable, and a claimed or launched operation needs retiring.
        let v = owner.settle_until(&rig, "retire-required");
        let kind = v["kind"].as_str().unwrap().to_string();
        let expected = match barrier {
            "launcher-before-claim" => "reserved",
            "launcher-before-peek"
            | "launcher-before-fork"
            | "daemon-gated"
            | "before-decision" => "pending",
            _ => "launched",
        };
        assert_eq!(kind, expected, "{barrier}");
        let inc = owner.record(&rig)["incarnation"]["id"]
            .as_str()
            .map(str::to_string);
        let (code, v) = owner.retire(&rig, "r-death", inc.as_deref());
        assert_eq!(code, 0, "{barrier}: {v:#}");
        let v = owner.settle_until(&rig, "closed");
        let closed = if expected == "launched" {
            "retired"
        } else {
            "never-launched"
        };
        assert_eq!(
            verdict(&v),
            ("closed".into(), Some(closed.into())),
            "{barrier}"
        );
        assert!(!row_exists(&rig, &owner.runtime_id), "{barrier}");
        assert!(!owner.ran(), "{barrier}");
    }
}

#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "test hooks exist only in debug builds"
)]
fn without_an_identity_safe_signal_recovery_sends_none() {
    let rig = Rig::new();
    let mut child = Command::new("sleep").arg("30").spawn().unwrap();
    let pid = child.id() as i32;
    let (sha, inc) = forge_launched(&rig, "op-nosignal", pid, None);
    // No pidfd (or, on macOS, no audit-token call): refuse to signal.
    let out = rig.run_env(
        &[
            "operation",
            "retire",
            "op-nosignal",
            "--envelope-sha256",
            &sha,
            "--request-id",
            "r-1",
            "--incarnation",
            &inc,
            "--json",
        ],
        &[("PTY_TEST_EXACT_SIGNAL", "unsupported")],
    );
    assert_eq!(out.code, 0, "{}", out.stdout);
    let v = out.json();
    assert_eq!(
        v["record"]["retirement"]["evidence"]["child"]["status"],
        "identity-safe-signal-unavailable"
    );
    assert_eq!(v["record"]["retirement"]["phase"], "incomplete");
    assert_ne!(v["verdict"], "closed");
    assert_eq!(v["noFurtherEffects"], false);
    // Nothing reached the process: not killed, not stopped, not continued.
    assert!(child.try_wait().unwrap().is_none());
    assert!(matches!(
        pty_core::proctable::process(pid),
        pty_core::proctable::Answer::Known(row) if !row.is_stopped() && !row.is_zombie()
    ));
    // With the mechanism, a new request ends it.
    let out = rig.run(&[
        "operation",
        "retire",
        "op-nosignal",
        "--envelope-sha256",
        &sha,
        "--request-id",
        "r-2",
        "--incarnation",
        &inc,
        "--json",
    ]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert_eq!(out.json()["record"]["retirement"]["phase"], "verified");
    assert!(!child.wait().unwrap().success());
}

#[test]
#[cfg_attr(
    not(debug_assertions),
    ignore = "test hooks exist only in debug builds"
)]
fn a_child_stopped_right_after_its_fork_holds_no_lock_and_is_retired() {
    let rig = Rig::new();
    let mut owner = Owner::start(
        &rig,
        "fork-stop",
        Plan {
            env: vec![("PTY_TEST_HELD_CHILD".into(), "stop-after-fork".into())],
            ..Plan::default()
        },
    );
    owner.wait_for("decision");
    let record = loop {
        let record = owner.record(&rig);
        if !record["participants"]["child"].is_null() {
            break record;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let child = record["participants"]["child"]["pid"].as_i64().unwrap() as i32;
    wait_until(
        "the child to stop",
        || matches!(pty_core::proctable::process(child), pty_core::proctable::Answer::Known(row) if row.is_stopped()),
    );
    // Stopped before it shed anything: it holds copies of every descriptor
    // of the daemon's, the lock file's among them, and it is still in the
    // daemon's process group.
    let daemon = record["participants"]["producer"]["pid"].as_i64().unwrap() as i32;
    kill(daemon, libc::SIGKILL);
    owner.launcher_exit();
    wait_gone(daemon);
    assert!(
        alive(child),
        "nothing ends a stopped child that has no terminal"
    );
    // The lock was the daemon's alone and died with it: settle reads the
    // record and names the live child, rather than a writer holding a lock.
    let v = owner.settle_until_kind(&rig, "live", "participants");
    assert_eq!(component(&v, "writer")["status"], "verified", "{v:#}");
    let inc = owner.record(&rig)["incarnation"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (code, v) = owner.retire(&rig, "r-fork-stop", Some(&inc));
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(
        v["record"]["retirement"]["evidence"]["child"]["status"],
        "gone"
    );
    assert!(!alive(child));
    owner.settle_until(&rig, "closed");
    assert!(!owner.ran());
}
