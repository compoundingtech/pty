//! `pty gc`: permanent respawn, flapping, abandoned reap, debris, orphan
//! children, the sweep and `keep`, layout-tag pruning, dry runs, the footer,
//! and `--print-launchd-plist`.
//!
//! node: tests/gc.test.ts, tests/gc-parent-child.test.ts,
//! tests/gc-permanent.test.ts, tests/gc-flapping.test.ts,
//! tests/gc-abandoned.test.ts, tests/exit-reap.test.ts:875-932,
//! tests/pty-root.test.ts:146-194

mod cli_common;

use std::process::Stdio;

use cli_common::{DEAD_PID, Rig, iso_now, wait_until};
use pty_core::registry::{now_epoch_ms, parse_iso8601_ms};
use serde_json::{Value, json};

const DAY_MS: i64 = 86_400_000;

fn mutate_meta(rig: &Rig, name: &str, mutate: impl FnOnce(&mut Value)) {
    let mut meta = rig.read_meta(name).expect("metadata");
    mutate(&mut meta);
    std::fs::write(
        rig.path(&format!("{name}.json")),
        serde_json::to_string_pretty(&meta).unwrap(),
    )
    .unwrap();
}

/// Policy fixtures need explicit gone-process evidence; absence of a PID or
/// token is deliberately not deletion/respawn authority.
fn write_gone_meta(rig: &Rig, name: &str, mut metadata: Value) {
    assert!(!pty_core::registry::pid_alive(DEAD_PID));
    metadata["daemonPid"] = json!(DEAD_PID);
    metadata["daemonStartToken"] = json!("fixture:dead");
    metadata["daemonBootId"] = json!(pty_core::registry::read_boot_id().unwrap());
    rig.write_meta(name, metadata);
}

/// node: tests/gc.test.ts:121-135, 198-229, 268-286; tests/exit-reap.test.ts:875-932
#[test]
fn sweeps_gone_sessions_and_keeps_kept_ones() {
    let rig = Rig::new();
    assert_eq!(rig.ok(&["gc"]).stdout, "Nothing to clean up.\n");
    assert_eq!(
        rig.ok(&["gc", "--dry-run"]).stdout,
        "Nothing would be cleaned up.\n"
    );
    write_gone_meta(&rig, "van", json!({}));
    write_gone_meta(&rig, "ex", json!({"exitCode": 1, "exitedAt": iso_now(0)}));
    write_gone_meta(
        &rig,
        "kept",
        json!({"exitCode": 0, "exitedAt": iso_now(0), "tags": {"keep": "true"}}),
    );
    let out = rig.ok(&["gc", "-n"]);
    assert_eq!(
        out.stdout,
        "Would remove: ex\nWould remove: van\nKept (keep tag): kept — swept once dead for 7d, or remove the keep tag to reap it now\nWould clean up 2 stale sessions. (Dry run — no changes made.)\n"
    );
    assert!(rig.exists("van.json") && rig.exists("ex.json"));
    let out = rig.ok(&["gc"]);
    assert_eq!(
        out.stdout,
        "Removed: ex\nRemoved: van\nKept (keep tag): kept — swept once dead for 7d, or remove the keep tag to reap it now\nCleaned up 2 stale sessions.\n"
    );
    assert!(!rig.exists("van.json") && !rig.exists("ex.json"));
    assert!(rig.exists("kept.json"));
    let out = rig.ok(&["gc"]);
    assert_eq!(
        out.stdout,
        "Kept (keep tag): kept — swept once dead for 7d, or remove the keep tag to reap it now\nNothing to clean up.\n"
    );
    write_gone_meta(&rig, "one", json!({}));
    assert_eq!(
        rig.ok(&["gc"]).stdout,
        "Removed: one\nKept (keep tag): kept — swept once dead for 7d, or remove the keep tag to reap it now\nCleaned up 1 stale session.\n"
    );
}

/// Unbound runtime debris is retained rather than treating Unknown as dead;
/// malformed metadata without runtime files can still be reclaimed.
#[test]
fn reclaims_raw_debris() {
    let rig = Rig::new();
    std::fs::write(rig.path("debris.pid"), DEAD_PID.to_string()).unwrap();
    std::fs::write(rig.path("debris.events.jsonl"), "").unwrap();
    std::fs::write(rig.path("broken.json"), "not json").unwrap();
    std::fs::write(rig.path("mine.pid"), std::process::id().to_string()).unwrap();
    let out = rig.ok(&["gc"]);
    assert_eq!(out.stdout, "Removed: broken\nCleaned up 1 stale session.\n");
    assert!(rig.exists("debris.pid") && rig.exists("debris.events.jsonl"));
    assert!(!rig.exists("broken.json"));
    assert!(rig.exists("mine.pid"));
}

/// node: tests/gc-parent-child.test.ts:121-224
#[test]
fn kills_orphan_children() {
    let rig = Rig::new();
    write_gone_meta(
        &rig,
        "child-missing",
        json!({"tags": {"parent": "nonexistent-parent"}}),
    );
    write_gone_meta(
        &rig,
        "dead-parent",
        json!({"exitCode": 0, "exitedAt": iso_now(0), "tags": {"keep": "true"}}),
    );
    write_gone_meta(
        &rig,
        "child-dead",
        json!({"tags": {"parent": "dead-parent"}}),
    );
    let out = rig.ok(&["gc", "--dry-run"]);
    assert_eq!(
        out.stdout,
        "Would kill orphan child: child-dead (parent dead-parent dead)\nWould kill orphan child: child-missing (parent nonexistent-parent missing)\nWould remove: child-dead\nWould remove: child-missing\nKept (keep tag): dead-parent — swept once dead for 7d, or remove the keep tag to reap it now\nWould clean up 2 orphan children, 2 stale sessions. (Dry run — no changes made.)\n"
    );
    assert!(rig.exists("child-dead.json"));

    // A live parent keeps its child.
    rig.spawn_cat("live-parent", &[]);
    rig.spawn_cat("happy-child", &["--tag", "parent=live-parent"]);
    let out = rig.ok(&["gc"]);
    assert_eq!(
        out.stdout,
        "Killed orphan child: child-dead (parent dead-parent dead)\nKilled orphan child: child-missing (parent nonexistent-parent missing)\nKept (keep tag): dead-parent — swept once dead for 7d, or remove the keep tag to reap it now\nCleaned up 2 orphan children.\n"
    );
    assert!(!rig.exists("child-dead.json") && !rig.exists("child-missing.json"));
    assert!(rig.exists("happy-child.json") && rig.exists("live-parent.json"));
    let names: Vec<String> = rig
        .ok(&["list", "--json", "--status", "running"])
        .json()
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["happy-child", "live-parent"]);
}

/// node: tests/gc-parent-child.test.ts:121-138 — a live orphan is SIGTERMed,
/// waited for, and its files removed. The observation is re-checked by
/// `generation`; the interim daemon publishes none, so its exit rewrite
/// reads as stale (`Skipped orphan reap: ... (stale, after signalling)`)
/// until the daemon lane lands.
#[test]
#[ignore = "needs the daemon to publish `generation` (daemon lane)"]
fn reaps_a_live_orphan() {
    let rig = Rig::new();
    rig.spawn_cat(
        "orphan",
        &[
            "--tag",
            "parent=nonexistent-parent",
            "--tag",
            "strategy=permanent",
        ],
    );
    let out = rig.ok(&["gc"]);
    assert_eq!(
        out.stdout,
        "Killed orphan child: orphan (parent nonexistent-parent missing)\nCleaned up 1 orphan child.\n"
    );
    assert!(!rig.exists("orphan.json") && !rig.exists("orphan.sock"));
}

#[cfg(target_os = "linux")]
#[test]
fn mismatched_permanent_respawns_without_signalling_the_reused_pid() {
    struct OwnedChild(std::process::Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let rig = Rig::new();
    let mut unrelated = OwnedChild(
        std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn owned unrelated process"),
    );
    let pid = unrelated.0.id();
    let listener = std::os::unix::net::UnixListener::bind(rig.path("reused-permanent.sock"))
        .expect("stale socket");
    rig.write_meta(
        "reused-permanent",
        json!({
            "generation": "old-generation",
            "daemonPid": pid,
            "daemonStartToken": "linux:1",
            "daemonBootId": pty_core::registry::read_boot_id().unwrap(),
            "command": "cat",
            "args": [],
            "tags": {"strategy": "permanent"}
        }),
    );
    std::fs::write(rig.path("reused-permanent.pid"), pid.to_string()).unwrap();
    let dry = rig.ok(&["gc", "--dry-run"]);
    assert!(dry.stdout.contains("Would respawn: reused-permanent"));
    assert!(unrelated.0.try_wait().unwrap().is_none());
    let actual = rig.ok(&["gc"]);
    assert!(
        actual.stdout.contains("Respawned: reused-permanent"),
        "{}",
        actual.stdout
    );
    wait_until("replacement permanent publication", || {
        rig.read_meta("reused-permanent").is_some_and(|metadata| {
            metadata["generation"] != "old-generation" && metadata["daemonPid"] != pid
        })
    });
    assert!(
        unrelated.0.try_wait().unwrap().is_none(),
        "GC must not signal the reused PID"
    );
    drop(listener);
}

/// node: tests/gc-permanent.test.ts — stopped permanents respawn once under
/// their stable id, while ordinary stopped sessions are swept.
#[test]
fn respawns_permanent_sessions_and_sweeps_non_permanent() {
    let rig = Rig::new();
    write_gone_meta(
        &rig,
        "perm",
        json!({"command": "cat", "args": [], "tags": {"strategy": "permanent"}}),
    );
    write_gone_meta(&rig, "plain", json!({"command": "cat", "args": []}));

    let dry = rig.ok(&["gc", "--dry-run"]);
    assert!(dry.stdout.contains("Would respawn: perm\n"));
    assert!(dry.stdout.contains("Would remove: plain\n"));
    assert!(rig.exists("perm.json") && rig.exists("plain.json"));

    let out = rig.ok(&["gc"]);
    assert!(out.stdout.contains("Respawned: perm\n"));
    assert!(out.stdout.contains("Removed: plain\n"));
    wait_until("permanent respawn", || {
        std::os::unix::net::UnixStream::connect(rig.path("perm.sock")).is_ok()
    });
    assert!(!rig.exists("plain.json"));
    let meta = rig.read_meta("perm").unwrap();
    assert_eq!(meta["tags"]["strategy"], "permanent");
    assert_eq!(meta["tags"]["strategy.consecutive-fast-fails"], "0");
    assert!(meta["tags"]["strategy.last-respawn-at"].is_string());
    assert!(meta["tags"]["strategy.command-hash"].is_string());
    let events = rig.events("perm");
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "session_respawn")
            .count(),
        1
    );
    let event_types: Vec<&str> = events
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect();
    assert!(
        event_types.ends_with(&["session_start", "session_respawn"]),
        "respawn must be part of the daemon publication batch: {event_types:?}"
    );
}

/// GC rebuilds a permanent session with the same terminal and environment
/// policy that its metadata recorded.
#[test]
fn permanent_respawn_preserves_persisted_launch_options() {
    let rig = Rig::new();
    write_gone_meta(
        &rig,
        "options",
        json!({
            "command": "cat",
            "args": [],
            "rows": 31,
            "cols": 101,
            "ephemeral": true,
            "isolateEnv": true,
            "extraEnv": {"EXTRA": "kept"},
            "unsetEnv": ["DROP"],
            "tags": {"strategy": "permanent"}
        }),
    );
    write_gone_meta(
        &rig,
        "exact-env",
        json!({
            "command": "cat",
            "args": [],
            "env": {"ONLY": "preserved"},
            "tags": {"strategy": "permanent"}
        }),
    );

    let out = rig.ok(&["gc"]);
    assert!(
        out.stdout.contains("Respawned: options") && out.stdout.contains("Respawned: exact-env"),
        "unexpected gc output:\nstdout:\n{}\nstderr:\n{}",
        out.stdout,
        out.stderr
    );
    wait_until("option-preserving respawn", || {
        std::os::unix::net::UnixStream::connect(rig.path("options.sock")).is_ok()
    });
    wait_until("verbatim-environment respawn", || {
        std::os::unix::net::UnixStream::connect(rig.path("exact-env.sock")).is_ok()
    });
    let meta = rig.read_meta("options").unwrap();
    assert_eq!(meta["rows"], 31);
    assert_eq!(meta["cols"], 101);
    assert_eq!(meta["ephemeral"], true);
    assert_eq!(meta["isolateEnv"], true);
    assert_eq!(meta["extraEnv"], json!({"EXTRA": "kept"}));
    assert_eq!(meta["unsetEnv"], json!(["DROP"]));
    let exact_env = rig.read_meta("exact-env").unwrap();
    assert_eq!(exact_env["env"], json!({"ONLY": "preserved"}));
}

/// The flapping clock is sampled only after earlier reaps and after this
/// permanent's locks are held, rather than once at the start of the GC pass.
#[test]
fn permanent_respawn_timestamp_is_taken_at_its_reconciliation() {
    let rig = Rig::new();
    let gone = rig.scratch.join("slow-gone");
    std::fs::create_dir_all(&gone).unwrap();
    let ready = rig.scratch.join("slow-ready");
    let mut delayed = std::process::Command::new("sh")
        .args([
            "-c",
            "trap 'sleep 0.35; exit 0' TERM; : > \"$READY\"; while :; do sleep 1; done",
        ])
        .env("READY", &ready)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until("delayed reap process ready", || ready.exists());
    rig.write_meta(
        "a-slow-abandon",
        json!({
            "daemonPid": delayed.id(),
            "daemonStartToken": pty_core::registry::read_process_start_token(delayed.id() as i32)
                .expect("owned child identity"),
            "daemonBootId": pty_core::registry::read_boot_id().unwrap(),
            "cwd": gone,
            "tags": {"strategy": "permanent"}
        }),
    );
    std::fs::write(rig.path("a-slow-abandon.pid"), delayed.id().to_string()).unwrap();
    std::fs::remove_dir_all(rig.scratch.join("slow-gone")).unwrap();
    write_gone_meta(
        &rig,
        "z-respawn",
        json!({"command": "cat", "args": [], "tags": {"strategy": "permanent"}}),
    );

    let not_before = now_epoch_ms() + 100;
    let out = rig.ok(&["gc"]);
    assert!(out.stdout.contains("Abandoned: a-slow-abandon (cwd-gone)"));
    assert!(out.stdout.contains("Respawned: z-respawn"));
    let meta = rig.read_meta("z-respawn").unwrap();
    let respawned_at =
        parse_iso8601_ms(meta["tags"]["strategy.last-respawn-at"].as_str().unwrap()).unwrap();
    assert!(
        respawned_at >= not_before,
        "respawn timestamp {respawned_at} predates its reconciliation {not_before}"
    );
    let _ = delayed.wait();
}

/// A pty.toml-bound respawn reuses the stable id, re-reads command and cwd,
/// and refreshes manifest tags without dropping manual tags.
#[test]
fn permanent_respawn_rereads_ptyfile_without_changing_identity() {
    let rig = Rig::new();
    let manifest = rig.write_toml(
        "manifest",
        "[sessions.worker]\ncommand = \"exit 0\"\ntags = { strategy = \"permanent\", version = \"one\" }\n",
    );
    rig.ok(&["up", manifest.to_str().unwrap()]);
    let listed = rig.ok(&["list", "--json"]).json();
    let id = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["displayName"] == "worker")
        .unwrap()["name"]
        .as_str()
        .unwrap()
        .to_string();
    wait_until("first manifest command exit", || {
        rig.read_meta(&id)
            .and_then(|meta| meta.get("exitedAt").cloned())
            .is_some_and(|value| value.is_string())
            && std::os::unix::net::UnixStream::connect(rig.path(&format!("{id}.sock"))).is_err()
    });
    mutate_meta(&rig, &id, |meta| {
        meta["tags"]["manual"] = json!("keep");
    });

    std::fs::write(
        manifest.join("pty.toml"),
        "[sessions.worker]\ncommand = \"cat\"\ncwd = \"..\"\ntags = { strategy = \"permanent\", role = \"worker\" }\n",
    )
    .unwrap();
    let out = rig.ok(&["gc"]);
    assert!(
        out.stdout
            .contains(&format!("Respawned: {id} (pty.toml re-read)\n")),
        "unexpected gc output:\nstdout:\n{}\nstderr:\n{}",
        out.stdout,
        out.stderr
    );
    wait_until("manifest respawn", || {
        std::os::unix::net::UnixStream::connect(rig.path(&format!("{id}.sock"))).is_ok()
    });
    let meta = rig.read_meta(&id).unwrap();
    assert_eq!(meta["displayName"], "worker");
    assert_eq!(meta["cwd"], rig.scratch.to_string_lossy().to_string());
    assert_eq!(meta["tags"]["role"], "worker");
    assert!(meta["tags"].get("version").is_none());
    assert_eq!(meta["tags"]["manual"], "keep");
    assert_eq!(meta["tags"]["ptyfile.session"], "worker");
    assert_eq!(meta["tags"]["ptyfile.tags"], "role,strategy");
}

/// Editing a bound manifest command is an operator recovery action even when
/// the previous command has already been marked as flapping.
#[test]
fn manifest_command_change_recovers_a_flapping_session() {
    let rig = Rig::new();
    let manifest = rig.write_toml(
        "flapping-manifest",
        "[sessions.worker]\ncommand = \"exit 1\"\ntags = { strategy = \"permanent\", \"strategy.fast-fail-limit\" = \"1\" }\n",
    );
    rig.ok(&["up", manifest.to_str().unwrap()]);
    let listed = rig.ok(&["list", "--json"]).json();
    let id = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["displayName"] == "worker")
        .unwrap()["name"]
        .as_str()
        .unwrap()
        .to_string();
    wait_until("initial manifest command exit", || {
        rig.read_meta(&id)
            .and_then(|meta| meta.get("exitedAt").cloned())
            .is_some_and(|value| value.is_string())
            && std::os::unix::net::UnixStream::connect(rig.path(&format!("{id}.sock"))).is_err()
    });

    assert!(rig.ok(&["gc"]).stdout.contains(&format!("Respawned: {id}")));
    wait_until("respawned manifest command exit", || {
        rig.read_meta(&id)
            .and_then(|meta| meta.get("exitedAt").cloned())
            .is_some_and(|value| value.is_string())
            && std::os::unix::net::UnixStream::connect(rig.path(&format!("{id}.sock"))).is_err()
    });
    assert!(rig.ok(&["gc"]).stdout.contains(&format!("Flapping: {id}")));

    std::fs::write(
        manifest.join("pty.toml"),
        "[sessions.worker]\ncommand = \"cat\"\ntags = { strategy = \"permanent\", \"strategy.fast-fail-limit\" = \"1\" }\n",
    )
    .unwrap();
    let dry = rig.ok(&["gc", "--dry-run"]);
    assert!(dry.stdout.contains(&format!("Would respawn: {id}")));
    assert!(!dry.stdout.contains(&format!("Skipped (flapping): {id}")));
    let out = rig.ok(&["gc"]);
    assert!(out.stdout.contains(&format!("Respawned: {id}")));
    wait_until("recovered manifest session", || {
        std::os::unix::net::UnixStream::connect(rig.path(&format!("{id}.sock"))).is_ok()
    });
    let meta = rig.read_meta(&id).unwrap();
    assert_eq!(meta["displayCommand"], "cat");
    assert!(meta["tags"].get("strategy.status").is_none());
    assert_eq!(meta["tags"]["strategy.consecutive-fast-fails"], "0");
}

/// The per-session creation lock makes repeated/concurrent reconciliation
/// idempotent: one replacement and one respawn event.
#[test]
fn concurrent_gc_respawns_a_permanent_only_once() {
    let rig = Rig::new();
    write_gone_meta(
        &rig,
        "perm",
        json!({"command": "cat", "args": [], "tags": {"strategy": "permanent"}}),
    );
    let mut first = rig.cmd(&["gc"]);
    first.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut second = rig.cmd(&["gc"]);
    second.stdout(Stdio::piped()).stderr(Stdio::piped());
    let first = first.spawn().unwrap();
    let second = second.spawn().unwrap();
    let outputs = [
        first.wait_with_output().unwrap(),
        second.wait_with_output().unwrap(),
    ];
    assert!(outputs.iter().all(|out| out.status.success()));
    let respawn_lines = outputs
        .iter()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .matches("Respawned: perm")
                .count()
        })
        .sum::<usize>();
    assert_eq!(respawn_lines, 1);
    wait_until("single concurrent respawn", || {
        std::os::unix::net::UnixStream::connect(rig.path("perm.sock")).is_ok()
    });
    assert_eq!(
        rig.events("perm")
            .iter()
            .filter(|event| event["type"] == "session_respawn")
            .count(),
        1
    );
}

/// A pre-launch failure leaves desired metadata intact, so a later pass
/// retries instead of silently losing permanent supervision.
#[test]
fn failed_permanent_respawn_is_retried_on_the_next_gc() {
    let rig = Rig::new();
    write_gone_meta(
        &rig,
        "broken",
        json!({
            "command": "pty-command-that-does-not-exist",
            "args": [],
            "tags": {"strategy": "permanent"}
        }),
    );
    for _ in 0..2 {
        let out = rig.ok(&["gc"]);
        assert!(
            out.stdout
                .contains("Respawn failed: broken — Command not found:")
        );
        assert!(rig.exists("broken.json"));
    }
}

/// node: tests/gc-flapping.test.ts — the threshold transition is persisted
/// once, repeated ticks skip, slow failures reset, command edits retry, and
/// per-session tuning wins over global/default tuning.
#[test]
fn fast_fail_accounting_flaps_resets_and_honours_overrides() {
    let rig = Rig::new();
    let hash = "bb9f87a41097ca1f";
    write_gone_meta(
        &rig,
        "flap",
        json!({
            "command": "sh",
            "args": ["-c", "exit 1"],
            "exitedAt": iso_now(-4_000),
            "tags": {
                "strategy": "permanent",
                "strategy.last-respawn-at": iso_now(-5_000),
                "strategy.consecutive-fast-fails": "2",
                "strategy.command-hash": hash
            }
        }),
    );
    let dry = rig.ok(&["gc", "--dry-run"]);
    assert!(
        dry.stdout
            .contains("Would flap: flap (3 fast-fails in 60s, limit 3)")
    );
    assert!(
        rig.read_meta("flap").unwrap()["tags"]
            .get("strategy.status")
            .is_none()
    );

    let out = rig.ok(&["gc"]);
    assert!(
        out.stdout
            .contains("Flapping: flap (3 fast-fails in 60s, limit 3)")
    );
    let meta = rig.read_meta("flap").unwrap();
    assert_eq!(meta["tags"]["strategy.status"], "flapping");
    assert_eq!(meta["tags"]["strategy.consecutive-fast-fails"], "3");
    let again = rig.ok(&["gc"]);
    assert!(again.stdout.contains("Skipped (flapping): flap"));
    let flapping_events: Vec<_> = rig
        .events("flap")
        .into_iter()
        .filter(|event| event["type"] == "session_flapping")
        .collect();
    assert_eq!(flapping_events.len(), 1);
    assert_eq!(flapping_events[0]["counter"], 3);
    assert_eq!(flapping_events[0]["limit"], 3);
    assert_eq!(flapping_events[0]["window"], 60);

    write_gone_meta(
        &rig,
        "slow",
        json!({
            "command": "sh",
            "args": ["-c", "exit 1"],
            "exitedAt": iso_now(-1_000),
            "tags": {
                "strategy": "permanent",
                "strategy.last-respawn-at": iso_now(-120_000),
                "strategy.consecutive-fast-fails": "2",
                "strategy.command-hash": hash
            }
        }),
    );
    write_gone_meta(
        &rig,
        "changed",
        json!({
            "command": "sh",
            "args": ["-c", "exit 1"],
            "exitedAt": iso_now(-1_000),
            "tags": {
                "strategy": "permanent",
                "strategy.status": "flapping",
                "strategy.consecutive-fast-fails": "9",
                "strategy.last-respawn-at": iso_now(-2_000),
                "strategy.command-hash": "0000000000000000"
            }
        }),
    );
    write_gone_meta(
        &rig,
        "override",
        json!({
            "command": "sh",
            "args": ["-c", "exit 1"],
            "exitedAt": iso_now(-1_000),
            "tags": {
                "strategy": "permanent",
                "strategy.fast-fail-limit": "2",
                "strategy.last-respawn-at": iso_now(-2_000),
                "strategy.consecutive-fast-fails": "1",
                "strategy.command-hash": hash
            }
        }),
    );
    let controls = rig.ok(&[
        "gc",
        "--dry-run",
        "--fast-fail-window=10",
        "--fast-fail-limit=10",
    ]);
    assert!(controls.stdout.contains("Would respawn: slow"));
    assert!(controls.stdout.contains("Would respawn: changed"));
    assert!(!controls.stdout.contains("Skipped (flapping): changed"));
    assert!(
        controls
            .stdout
            .contains("Would flap: override (2 fast-fails in 10s, limit 2)")
    );
}

/// node: tests/gc-abandoned.test.ts — cwd-gone is on by default only for
/// permanent sessions, with an exact `false` opt-out.
#[test]
fn cwd_gone_abandons_only_eligible_permanent_sessions() {
    let rig = Rig::new();
    let gone = rig.scratch.join("gone");
    let opted = rig.scratch.join("opted");
    let plain = rig.scratch.join("plain");
    for cwd in [&gone, &opted, &plain] {
        std::fs::create_dir_all(cwd).unwrap();
    }
    rig.spawn_cat(
        "abandon",
        &[
            "--cwd",
            gone.to_str().unwrap(),
            "--tag",
            "strategy=permanent",
        ],
    );
    rig.spawn_cat(
        "opted",
        &[
            "--cwd",
            opted.to_str().unwrap(),
            "--tag",
            "strategy=permanent",
            "--tag",
            "strategy.abandon-if-cwd-gone=false",
        ],
    );
    rig.spawn_cat("plain", &["--cwd", plain.to_str().unwrap()]);
    std::fs::remove_dir_all(&gone).unwrap();
    mutate_meta(&rig, "abandon", |meta| {
        meta["lastAttachAt"] = json!(iso_now(-31 * DAY_MS));
    });
    std::fs::remove_dir_all(&opted).unwrap();
    std::fs::remove_dir_all(&plain).unwrap();

    let dry = rig.ok(&["gc", "--dry-run", "--idle-days", "1"]);
    assert!(dry.stdout.contains("Would abandon: abandon (cwd-gone)"));
    assert!(!dry.stdout.contains("Would abandon: abandon (idle"));
    assert!(!dry.stdout.contains("Would abandon: opted"));
    assert!(!dry.stdout.contains("Would abandon: plain"));
    assert!(rig.exists("abandon.json"));

    let out = rig.ok(&["gc", "--idle-days", "1"]);
    assert!(out.stdout.contains("Abandoned: abandon (cwd-gone)"));
    assert!(!rig.exists("abandon.json"));
    assert!(rig.exists("opted.json") && rig.exists("plain.json"));
}

/// A reachable daemon whose registry pid identity cannot be confirmed must
/// never have its socket and metadata unlinked by abandonment reap.
#[test]
fn abandonment_does_not_unlink_a_reachable_session_without_confirmed_pid() {
    let rig = Rig::new();
    let gone = rig.scratch.join("gone-without-pid");
    std::fs::create_dir_all(&gone).unwrap();
    rig.spawn_cat(
        "unconfirmed",
        &[
            "--cwd",
            gone.to_str().unwrap(),
            "--tag",
            "strategy=permanent",
        ],
    );
    std::fs::remove_dir_all(&gone).unwrap();

    let original_meta = rig.read_meta("unconfirmed").unwrap();
    let pid_path = rig.path("unconfirmed.pid");
    let original_pid = std::fs::read_to_string(&pid_path).unwrap();
    mutate_meta(&rig, "unconfirmed", |meta| {
        let object = meta.as_object_mut().unwrap();
        object.remove("daemonPid");
        object.remove("recovery");
    });
    std::fs::remove_file(&pid_path).unwrap();

    let out = rig.ok(&["gc"]);
    assert!(
        out.stdout
            .contains("Skipped abandoned reap: unconfirmed (identity-unknown, before signalling)"),
        "unexpected gc output:\n{}",
        out.stdout
    );
    assert!(rig.exists("unconfirmed.json") && rig.exists("unconfirmed.sock"));
    assert!(std::os::unix::net::UnixStream::connect(rig.path("unconfirmed.sock")).is_ok());

    std::fs::write(
        rig.path("unconfirmed.json"),
        serde_json::to_string_pretty(&original_meta).unwrap(),
    )
    .unwrap();
    std::fs::write(pid_path, original_pid).unwrap();
}

/// Idle reap is opt-in, the session tag takes precedence, and sessions below
/// threshold or never attached are retained.
#[test]
fn idle_day_reap_honours_thresholds_and_negative_controls() {
    let rig = Rig::new();
    for name in ["old", "tagged", "fresh", "never"] {
        let mut args = vec!["--tag", "strategy=permanent"];
        if name == "tagged" {
            args.extend_from_slice(&["--tag", "strategy.idle-days=10"]);
        }
        rig.spawn_cat(name, &args);
    }
    mutate_meta(&rig, "old", |meta| {
        meta["lastAttachAt"] = json!(iso_now(-31 * DAY_MS))
    });
    mutate_meta(&rig, "tagged", |meta| {
        meta["lastAttachAt"] = json!(iso_now(-11 * DAY_MS));
    });
    mutate_meta(&rig, "fresh", |meta| {
        meta["lastAttachAt"] = json!(iso_now(-3 * DAY_MS))
    });

    let out = rig.ok(&["gc", "--idle-days", "14"]);
    assert!(out.stdout.contains("Abandoned: old (idle 31d)"));
    assert!(out.stdout.contains("Abandoned: tagged (idle 11d)"));
    assert!(!out.stdout.contains("Abandoned: fresh"));
    assert!(!out.stdout.contains("Abandoned: never"));
    assert!(!rig.exists("old.json") && !rig.exists("tagged.json"));
    assert!(rig.exists("fresh.json") && rig.exists("never.json"));
}

/// node: tests/gc.test.ts:137-196, 231-253
#[test]
fn prunes_dead_layout_tags_on_running_sessions() {
    let rig = Rig::new();
    let dead = format!(":l{DEAD_PID}-abc=1");
    let live = format!(":l{}-xyz=1", std::process::id());
    rig.spawn_cat(
        "lay",
        &[
            "--tag",
            &dead,
            "--tag",
            &live,
            "--tag",
            ":layout=grid",
            "--tag",
            "role=web",
        ],
    );
    let out = rig.ok(&["gc", "-n"]);
    assert_eq!(
        out.stdout,
        format!(
            "Would prune orphan tags on lay: #:l{DEAD_PID}-abc\nWould clean up 1 orphan tag. (Dry run — no changes made.)\n"
        )
    );
    let out = rig.ok(&["gc"]);
    assert_eq!(
        out.stdout,
        format!("Pruned orphan tags on lay: #:l{DEAD_PID}-abc\nCleaned up 1 orphan tag.\n")
    );
    let tags = rig.read_meta("lay").unwrap()["tags"].clone();
    assert!(tags.get(format!(":l{DEAD_PID}-abc")).is_none());
    assert_eq!(tags[format!(":l{}-xyz", std::process::id())], "1");
    assert_eq!(tags[":layout"], "grid");
    assert_eq!(tags["role"], "web");
    assert_eq!(rig.ok(&["gc"]).stdout, "Nothing to clean up.\n");
    // Pruning is a tag write: one tags_change event.
    assert_eq!(
        rig.events("lay")
            .iter()
            .filter(|e| e["type"] == "tags_change")
            .count(),
        1
    );
}

/// node: tests/gc.test.ts:288-327, tests/pty-root.test.ts:146-194
#[test]
fn launchd_plist() {
    let rig = Rig::new();
    let out = rig.ok(&["gc", "--print-launchd-plist"]);
    let base = rig.root.file_name().unwrap().to_str().unwrap();
    let expected = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>com.compoundingtech.pty.gc.{base}</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string>{bin}</string>\n    <string>gc</string>\n  </array>\n  <key>StartInterval</key>\n  <integer>30</integer>\n  <key>RunAtLoad</key>\n  <true/>\n  <key>StandardOutPath</key>\n  <string>{root}/gc.log</string>\n  <key>StandardErrorPath</key>\n  <string>{root}/gc.log</string>\n  <key>EnvironmentVariables</key>\n  <dict>\n    <key>PATH</key>\n    <string>{path}</string>\n    <key>PTY_ROOT</key>\n    <string>{root}</string>\n  </dict>\n</dict>\n</plist>\n",
        bin = cli_common::pty_bin(),
        root = rig.root.display(),
        path = std::env::var("PATH").unwrap_or_default()
    );
    assert_eq!(out.stdout, expected);
    assert!(
        out.stdout
            .contains(&format!("<string>{}</string>", cli_common::pty_bin()))
    );
    assert!(!out.stdout.contains("PTY_SESSION_DIR"));

    let out = rig.ok(&["gc", "--print-launchd-plist", "--interval=15"]);
    assert!(out.stdout.contains("<integer>15</integer>"));
    let out = rig.ok(&["gc", "--print-launchd-plist", "--interval", "45"]);
    assert!(out.stdout.contains("<integer>45</integer>"));
    for bad in ["--interval=0", "--interval=abc"] {
        let out = rig.run(&["gc", "--print-launchd-plist", bad]);
        assert_eq!(out.code, 1, "{bad}");
        assert_eq!(
            out.stderr,
            format!(
                "pty gc: --interval expects a positive integer (got \"{}\")\n",
                &bad["--interval=".len()..]
            )
        );
    }

    // The label suffix is the sanitized basename; the default root has none.
    let weird = rig.scratch.join("weird name with spaces");
    std::fs::create_dir_all(&weird).unwrap();
    let out = rig.run(&[
        "--root",
        weird.to_str().unwrap(),
        "gc",
        "--print-launchd-plist",
    ]);
    assert!(
        out.stdout
            .contains("<string>com.compoundingtech.pty.gc.weird-name-with-spaces</string>")
    );
    let ampersand = rig.scratch.join("a&b");
    std::fs::create_dir_all(&ampersand).unwrap();
    let out = rig.run(&[
        "--root",
        ampersand.to_str().unwrap(),
        "gc",
        "--print-launchd-plist",
    ]);
    assert!(out.stdout.contains(&format!(
        "<string>{}/a&amp;b</string>",
        rig.scratch.display()
    )));
    let home = rig.scratch.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut c = rig.cmd(&["gc", "--print-launchd-plist"]);
    c.env_remove("PTY_ROOT").env("HOME", &home);
    let out = c.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("<string>com.compoundingtech.pty.gc</string>"),
        "{stdout}"
    );
    assert!(!stdout.contains("<string>com.compoundingtech.pty.gc."));
}

/// Node validates every lifecycle tuning flag as a positive integer.
#[test]
fn lifecycle_tuning_flags_are_validated() {
    let rig = Rig::new();
    let out = rig.ok(&[
        "gc",
        "--idle-days",
        "14",
        "--fast-fail-window=10",
        "--fast-fail-limit",
        "2",
    ]);
    assert_eq!(out.stdout, "Nothing to clean up.\n");
    for (flag, value) in [
        ("--idle-days", "0"),
        ("--fast-fail-window", "-1"),
        ("--fast-fail-limit", "no"),
    ] {
        let arg = format!("{flag}={value}");
        let out = rig.run(&["gc", &arg]);
        assert_eq!(out.code, 1);
        assert_eq!(
            out.stderr,
            format!("pty gc: {flag} expects a positive integer (got \"{value}\")\n")
        );
    }
}
