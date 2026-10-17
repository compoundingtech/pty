//! Publication-final fences and survivor preservation through the real binary.

mod cli_common;

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::process::{Child, Stdio};

use cli_common::{Rig, wait_until};
use serde_json::json;

const OLD: &str = "revoked-start";
const NEW: &str = "replacement-start";
const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NEW_DIGEST: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const INTENT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn rig() -> Rig {
    let rig = Rig::new();
    std::fs::set_permissions(&rig.root, std::fs::Permissions::from_mode(0o700)).unwrap();
    rig
}

fn identity(rig: &Rig) -> String {
    rig.ok(&["publication-revocation", "identity", "--json"]).json()["root_id"]
        .as_str().unwrap().to_owned()
}

fn install_args<'a>(name: &'a str, root: &'a str) -> Vec<&'a str> {
    vec!["publication-revocation", "install", "--id", name, "--operation", OLD,
         "--binding", DIGEST, "--intent", INTENT, "--root-id", root, "--json"]
}

fn managed(rig: &Rig, name: &str, root: &str, operation: &str, digest: &str) -> cli_common::Out {
    rig.run(&["run", "-d", "--force", "--id", name,
        "--publication-operation", operation, "--publication-binding", digest,
        "--publication-root-id", root, "--tag", "keep=true",
        "--tag", &format!("st3.start-operation={operation}"), "--", "cat"])
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Snapshot {
    pid: Vec<u8>,
    metadata: Vec<u8>,
    events: Vec<u8>,
    socket: (u64, u64),
}

impl Snapshot {
    fn take(rig: &Rig, name: &str) -> Self {
        let socket = std::fs::metadata(rig.path(&format!("{name}.sock"))).unwrap();
        Self {
            pid: std::fs::read(rig.path(&format!("{name}.pid"))).unwrap(),
            metadata: std::fs::read(rig.path(&format!("{name}.json"))).unwrap(),
            events: std::fs::read(rig.path(&format!("{name}.events.jsonl"))).unwrap(),
            socket: (socket.dev(), socket.ino()),
        }
    }

    fn assert_unchanged(&self, rig: &Rig, name: &str) {
        let after = Self::take(rig, name);
        assert_eq!(after.pid, self.pid);
        assert_eq!(after.metadata, self.metadata);
        assert_eq!(after.events, self.events);
        assert_eq!(after.socket, self.socket);
        assert!(std::os::unix::net::UnixStream::connect(rig.path(&format!("{name}.sock"))).is_ok());
    }
}

fn direct_daemon(rig: &Rig, name: &str, root: &str, barriers: &std::path::Path) -> ChildGuard {
    let config = json!({
        "name": name,
        "command": "/bin/sh",
        "args": ["-c", "printf ran > \"$1\"; exec cat", "sh", rig.scratch.join("child-ran")],
        "displayCommand": "publication fence child",
        "cwd": rig.scratch,
        "rows": 24,
        "cols": 80,
        "tags": {"keep": "true", "st3.start-operation": OLD},
        "publicationBinding": {"operation": OLD, "digest": DIGEST},
        "publicationRootId": root,
    });
    ChildGuard(rig.cmd(&["__daemon"])
        .env("PTY_SERVER_CONFIG", config.to_string())
        .env("PTY_TEST_OPERATION_BARRIERS", barriers)
        .env_remove("PTY_CREATION_LOCK_OWNER_PID")
        .env_remove("PTY_SPAWNER_PID")
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(rig.scratch.join(format!("{name}.daemon.stderr"))).unwrap()))
        .spawn().unwrap())
}

#[test]
fn revoked_force_run_refuses_but_distinct_binding_publishes_and_adopts() {
    let rig = rig();
    let root = identity(&rig);
    rig.ok(&install_args("worker", &root));
    let refused = managed(&rig, "worker", &root, OLD, DIGEST);
    assert_eq!(refused.code, 1, "{}", refused.stderr);
    assert!(refused.stderr.contains("operation-revoked"), "{}", refused.stderr);
    for suffix in ["json", "pid", "sock", "events.jsonl"] {
        assert!(!rig.exists(&format!("worker.{suffix}")));
    }
    // A different digest cannot bypass a revoked operation ID.
    let mismatched = managed(&rig, "worker", &root, OLD, NEW_DIGEST);
    assert_eq!(mismatched.code, 1);
    assert!(mismatched.stderr.contains("operation-revoked"), "{}", mismatched.stderr);

    let replacement = managed(&rig, "worker", &root, NEW, NEW_DIGEST);
    assert_eq!(replacement.code, 0, "{}", replacement.stderr);
    let meta = rig.read_meta("worker").unwrap();
    assert_eq!(meta["publicationBinding"], json!({"operation": NEW, "digest": NEW_DIGEST}));
    assert_eq!(meta["publicationRootId"], root);
    assert_eq!(meta["publicationCapability"], 1);
    let rows = rig.ok(&["list", "--strict", "--json"]).json();
    assert_eq!(rows["complete"], true);
    let adopted = rows["entries"].as_array().unwrap().iter()
        .find(|row| row["name"] == "worker").unwrap();
    assert_eq!(adopted["generation"], meta["generation"]);
    assert_eq!(adopted["status"], "running");
}

#[test]
fn managed_launch_refuses_operation_tag_as_startup_lifecycle_tag() {
    let rig = rig();
    let root = identity(&rig);
    let marker = rig.scratch.join("lease-child-ran");
    let refused = rig.run(&[
        "run", "-d", "--force", "--id", "lease-conflict",
        "--publication-operation", NEW,
        "--publication-binding", NEW_DIGEST,
        "--publication-root-id", &root,
        "--tag", &format!("st3.start-operation={NEW}"),
        "--lifecycle-tag", "st3.start-operation",
        "--startup-timeout-ms", "1000",
        "--", "/bin/sh", "-c", "printf ran > \"$1\"", "sh",
        marker.to_str().unwrap(),
    ]);
    assert_ne!(refused.code, 0, "{}", refused.stdout);
    assert!(!refused.stderr.is_empty());
    assert!(!marker.exists());
    for suffix in ["json", "pid", "sock", "events.jsonl"] {
        assert!(!rig.exists(&format!("lease-conflict.{suffix}")));
    }
}

#[test]
fn restart_refuses_revoked_binding_before_stopping_survivor() {
    let rig = rig();
    let root = identity(&rig);
    let started = managed(&rig, "survivor", &root, OLD, DIGEST);
    assert_eq!(started.code, 0, "{}", started.stderr);
    let before = Snapshot::take(&rig, "survivor");
    let installed = rig.ok(&install_args("survivor", &root)).json();
    assert_eq!(installed["occupancy"]["entries"][0]["status"], "running");
    let out = rig.run(&["restart", "survivor", "--force"]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(out.stderr.contains("operation-revoked"), "{}", out.stderr);
    before.assert_unchanged(&rig, "survivor");
}

#[test]
fn immutable_launch_provenance_cannot_be_forged_through_metadata_or_tags() {
    let rig = rig();
    let root = identity(&rig);
    let started = managed(&rig, "survivor", &root, NEW, NEW_DIGEST);
    assert_eq!(started.code, 0, "{}", started.stderr);
    let before = Snapshot::take(&rig, "survivor");
    for patch in [
        json!({"publicationBinding": {"operation": OLD, "digest": DIGEST}}),
        json!({"publicationRootId": "forged-root"}),
        json!({"publicationCapability": 2}),
        json!({"tags": {"st3.start-operation": OLD}}),
    ] {
        let out = rig.run_stdin(&["metadata", "patch", "--id", "survivor"], &patch.to_string());
        assert_ne!(out.code, 0, "accepted forged patch: {patch}");
        before.assert_unchanged(&rig, "survivor");
    }
    let forged = rig.run(&["tag", "survivor", &format!("st3.start-operation={OLD}")]);
    assert_ne!(forged.code, 0, "accepted forged operation tag");
    before.assert_unchanged(&rig, "survivor");
}

#[test]
fn ended_managed_row_requires_explicit_recreation_binding() {
    let rig = rig();
    let root = identity(&rig);
    rig.write_meta("ended", json!({
        "generation": "ended-generation",
        "publicationBinding": {"operation": OLD, "digest": DIGEST},
        "publicationRootId": root,
        "publicationCapability": 1,
        "tags": {"keep": "true", "st3.start-operation": OLD},
        "exitedAt": cli_common::iso_now(-1000),
        "exitCode": 0
    }));
    let metadata = std::fs::read(rig.path("ended.json")).unwrap();
    std::fs::write(rig.path("ended.events.jsonl"), "{\"type\":\"fixture\"}\n").unwrap();
    let events = std::fs::read(rig.path("ended.events.jsonl")).unwrap();
    let refused = rig.run(&["run", "-d", "--force", "--id", "ended", "--", "cat"]);
    assert_ne!(refused.code, 0);
    assert_eq!(std::fs::read(rig.path("ended.json")).unwrap(), metadata);
    assert_eq!(std::fs::read(rig.path("ended.events.jsonl")).unwrap(), events);
    assert!(!rig.exists("ended.pid"));
    assert!(!rig.exists("ended.sock"));
    let replacement = managed(&rig, "ended", &root, NEW, NEW_DIGEST);
    assert_eq!(replacement.code, 0, "{}", replacement.stderr);
    let meta = rig.read_meta("ended").unwrap();
    assert_ne!(meta["generation"], "ended-generation");
    assert_eq!(meta["publicationBinding"], json!({"operation": NEW, "digest": NEW_DIGEST}));
    assert_eq!(meta["tags"]["st3.start-operation"], NEW);
}

#[test]
fn capability_advertises_final_publication_and_immutable_binding() {
    let rig = rig();
    let capabilities = rig.ok(&["capabilities", "--json"]).json();
    let publication = &capabilities["capabilities"]["publication-revocation"];
    assert_eq!(publication["versions"], json!([1]));
    assert_eq!(publication["recordSchemas"], json!([1]));
    assert_eq!(publication["atomicNamePublication"], true);
    assert_eq!(publication["managedImmutableBinding"], true);
}

#[test]
#[cfg(debug_assertions)]
fn already_spawned_daemon_revoked_before_lock_cannot_touch_survivor() {
    let rig = rig();
    let root = identity(&rig);
    rig.spawn_cat("survivor", &["--tag", "keep=true"]);
    let before = Snapshot::take(&rig, "survivor");
    let barriers = rig.scratch.join("barriers");
    std::fs::create_dir(&barriers).unwrap();
    std::fs::write(barriers.join("publication-before-lock.hold"), "").unwrap();
    let mut daemon = direct_daemon(&rig, "survivor", &root, &barriers);
    wait_until("daemon before publication lock", || barriers.join("publication-before-lock.reached").exists());
    let installed = rig.ok(&install_args("survivor", &root)).json();
    assert_eq!(installed["occupancy"]["entries"][0]["status"], "running");
    std::fs::write(barriers.join("publication-before-lock.release"), "").unwrap();
    let mut status = None;
    wait_until("revoked direct daemon exit", || {
        status = daemon.0.try_wait().unwrap();
        status.is_some()
    });
    assert!(!status.unwrap().success());
    let error = std::fs::read_to_string(rig.scratch.join("survivor.daemon.stderr")).unwrap();
    assert!(error.contains("operation-revoked"), "refusal was not the fence: {error}");
    assert!(!rig.scratch.join("child-ran").exists());
    before.assert_unchanged(&rig, "survivor");
}

#[test]
#[cfg(debug_assertions)]
fn already_spawned_daemon_revoked_without_occupancy_cannot_publish() {
    let rig = rig();
    let root = identity(&rig);
    let barriers = rig.scratch.join("barriers");
    std::fs::create_dir(&barriers).unwrap();
    std::fs::write(barriers.join("publication-before-lock.hold"), "").unwrap();
    let mut daemon = direct_daemon(&rig, "vacant", &root, &barriers);
    wait_until("vacant daemon before publication lock", || {
        barriers.join("publication-before-lock.reached").exists()
    });
    let installed = rig.ok(&install_args("vacant", &root)).json();
    assert_eq!(installed["occupancy"]["entries"], json!([]));
    std::fs::write(barriers.join("publication-before-lock.release"), "").unwrap();
    let mut status = None;
    wait_until("revoked vacant daemon exit", || {
        status = daemon.0.try_wait().unwrap();
        status.is_some()
    });
    assert!(!status.unwrap().success());
    let error = std::fs::read_to_string(rig.scratch.join("vacant.daemon.stderr")).unwrap();
    assert!(error.contains("operation-revoked"), "refusal was not the fence: {error}");
    for suffix in ["json", "pid", "sock", "events.jsonl"] {
        assert!(!rig.exists(&format!("vacant.{suffix}")), "published {suffix} after revocation");
    }
    assert!(!rig.scratch.join("child-ran").exists());
}

#[test]
#[cfg(debug_assertions)]
fn publisher_holding_final_effects_lock_wins_without_revocation_killing_it() {
    let rig = rig();
    let root = identity(&rig);
    let barriers = rig.scratch.join("barriers");
    std::fs::create_dir(&barriers).unwrap();
    std::fs::write(barriers.join("publication-before-effects.hold"), "").unwrap();
    let mut daemon = direct_daemon(&rig, "winner", &root, &barriers);
    wait_until("daemon holding final publication lock", || barriers.join("publication-before-effects.reached").exists());
    let busy = rig.run(&install_args("winner", &root));
    assert_ne!(busy.code, 0);
    assert!(busy.stderr.contains("busy"), "{}", busy.stderr);
    assert!(!rig.exists("winner.sock"));
    assert!(!rig.scratch.join("child-ran").exists());
    std::fs::write(barriers.join("publication-before-effects.release"), "").unwrap();
    wait_until("winning publication child", || rig.scratch.join("child-ran").exists());
    // The child can create its marker before the daemon drops the final
    // publication lock. A stats round trip runs on the serving loop, after
    // that lock is released; socket connect alone only reaches the backlog.
    let stats = rig.ok(&["stats", "--json", "winner"]).json();
    assert_eq!(stats["process"]["alive"], true);
    let installed = rig.ok(&install_args("winner", &root)).json();
    assert_eq!(installed["occupancy"]["entries"][0]["status"], "running");
    assert!(daemon.0.try_wait().unwrap().is_none());
    let before = Snapshot::take(&rig, "winner");
    rig.ok(&install_args("winner", &root));
    before.assert_unchanged(&rig, "winner");
}

#[test]
#[cfg(debug_assertions)]
fn lifecycle_publication_waits_for_the_real_event_lock_before_clearing() {
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};

    const TEST: &str = "lifecycle_publication_waits_for_the_real_event_lock_before_clearing";
    const FIXTURE: &str = "PTY_PUBLICATION_EVENT_WAIT_FIXTURE";
    if std::env::var_os(FIXTURE).is_some() {
        lifecycle_event_lock_wait_scenario();
        return;
    }
    // Supervise the whole fixture, including real daemon and Rig cleanup.
    // The direct daemon remains in this process group; closing its PTY on
    // forced cleanup also hangs up the child in its separate session.
    let mut fixture = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture"])
        .env(FIXTURE, "1")
        .process_group(0)
        .stdin(Stdio::null())
        .spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = fixture.try_wait().unwrap() {
            assert!(status.success(), "event-wait fixture failed: {status}");
            return;
        }
        if Instant::now() >= deadline {
            // SAFETY: the unreaped direct child leads the private group we
            // created, so this group ID cannot have been recycled.
            unsafe { libc::kill(-(fixture.id() as i32), libc::SIGKILL); }
            let reap_deadline = Instant::now() + Duration::from_secs(5);
            while fixture.try_wait().unwrap().is_none() {
                assert!(Instant::now() < reap_deadline, "event-wait fixture was not reaped");
                std::thread::sleep(Duration::from_millis(20));
            }
            panic!("event-wait fixture exceeded 30 seconds");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(debug_assertions)]
fn lifecycle_event_lock_wait_scenario() {
    use pty_core::registry::{take_event_lock, with_root};

    let rig = rig();
    let root = identity(&rig);
    let name = "event-wait";
    let barriers = rig.scratch.join("event-wait-barriers");
    std::fs::create_dir(&barriers).unwrap();
    std::fs::write(barriers.join("publication-before-effects.hold"), "").unwrap();
    let old_events = b"{\"type\":\"predecessor-fixture\"}\n";
    std::fs::write(rig.path(&format!("{name}.events.jsonl")), old_events).unwrap();
    let mut daemon = direct_daemon(&rig, name, &root, &barriers);
    wait_until("real daemon at publication effects", || {
        barriers.join("publication-before-effects.reached").exists()
    });
    let guard = with_root(&rig.root, || take_event_lock(name).unwrap());
    std::fs::write(barriers.join("publication-before-effects.release"), "").unwrap();
    // This marker is emitted only by the bounded event-lock acquisition,
    // not by the publication barrier. Reverting the lifecycle caller to
    // clear_events fails here, even though the waiting primitive still exists.
    wait_until("lifecycle event clear waiting on its real lock", || {
        assert!(daemon.0.try_wait().unwrap().is_none(), "publication refused transient contention");
        barriers.join("event-lock-busy.reached").exists()
    });
    let waiter: u32 = std::fs::read_to_string(barriers.join("event-lock-busy.reached"))
        .unwrap().trim().parse().unwrap();
    assert_eq!(waiter, daemon.0.id());
    assert_eq!(std::fs::read(rig.path(&format!("{name}.events.jsonl"))).unwrap(), old_events);
    assert!(!rig.exists(&format!("{name}.sock")));
    assert!(!rig.scratch.join("child-ran").exists());
    drop(guard);
    wait_until("publication after predecessor event writer releases", || {
        assert!(daemon.0.try_wait().unwrap().is_none(), "publication failed after event lock release");
        rig.scratch.join("child-ran").exists()
    });
    assert!(rig.exists(&format!("{name}.sock")));
    assert!(rig.read_meta(name).is_some());
    wait_until("new session start event", || {
        let events = rig.events(name);
        !events.iter().any(|event| event["type"] == "predecessor-fixture")
            && events.iter().any(|event| event["type"] == "session_start")
    });
}
