//! `pty up` / `pty down`: the manifest, the tag-pair binding, tag sync,
//! name decoupling, the per-session lines and the footers.
//!
//! node: tests/up-down.test.ts, tests/up-name-decouple.test.ts

mod cli_common;

use cli_common::Rig;
use serde_json::Value;

fn sessions(rig: &Rig) -> Vec<Value> {
    rig.ok(&["list", "--json"])
        .json()
        .as_array()
        .unwrap()
        .clone()
}

fn by_display(rig: &Rig, dn: &str) -> Value {
    sessions(rig)
        .into_iter()
        .find(|s| s["displayName"] == dn)
        .unwrap_or_else(|| panic!("no session {dn}"))
}

/// node: tests/up-down.test.ts:81-171, 543-601
#[test]
fn up_starts_and_down_stops() {
    let rig = Rig::new();
    let dir = rig.write_toml(
        "proj",
        "[sessions.one]\ncommand = \"cat\"\n\n[sessions.two]\ncommand = \"cat\"\n",
    );
    let out = rig.ok(&["up", dir.to_str().unwrap()]);
    assert_eq!(
        out.stdout,
        "  ● one (started)\n  ● two (started)\nStarted 2 sessions.\n"
    );
    let running: Vec<Value> = sessions(&rig)
        .into_iter()
        .filter(|s| s["status"] == "running")
        .collect();
    assert_eq!(running.len(), 2);
    let one = by_display(&rig, "one");
    assert_eq!(
        one["tags"]["ptyfile"],
        dir.join("pty.toml").to_str().unwrap()
    );
    assert_eq!(one["tags"]["ptyfile.session"], "one");
    assert_eq!(one["tags"]["ptyfile.tags"], "");
    assert_eq!(one["cwd"], dir.to_str().unwrap());
    assert_eq!(one["command"], "cat");
    let name = one["name"].as_str().unwrap();
    assert!(
        regex::Regex::new("^[a-z0-9]{6,12}$")
            .unwrap()
            .is_match(name)
    );

    let out = rig.ok(&["up", dir.to_str().unwrap()]);
    assert_eq!(
        out.stdout,
        "  ● one (already running)\n  ● two (already running)\nAll sessions already running.\n"
    );
    assert_eq!(
        by_display(&rig, "one")["name"],
        name,
        "re-runs bind by the tag pair"
    );

    let out = rig.run(&["up", dir.to_str().unwrap(), "fake"]);
    assert_eq!(out.code, 1);
    assert_eq!(out.stderr, "Unknown session: fake\nAvailable: one, two\n");
    let out = rig.run(&["up", dir.to_str().unwrap(), "fake", "faker"]);
    assert_eq!(
        out.stderr,
        "Unknown sessions: fake, faker\nAvailable: one, two\n"
    );

    let out = rig.ok(&["down", dir.to_str().unwrap(), "one"]);
    assert_eq!(out.stdout, "  ○ one (stopped)\nStopped 1 session.\n");
    assert_eq!(out.stderr, "\nNote: strategy tags will be restored on the next 'pty up'.\n");
    cli_common::wait_until("one gone", || by_display(&rig, "one")["status"] != "running");
    // A gone bound session is cleaned up by `down`.
    let out = rig.ok(&["down", dir.to_str().unwrap()]);
    assert_eq!(out.stdout, "  ○ one (cleaned up)\n  ○ two (stopped)\nStopped 2 sessions.\n");
    cli_common::wait_until("two gone", || by_display(&rig, "two")["status"] != "running");
    let out = rig.ok(&["down", dir.to_str().unwrap()]);
    assert_eq!(out.stdout, "  ○ two (cleaned up)\nStopped 1 session.\n");
    let out = rig.ok(&["down", dir.to_str().unwrap()]);
    assert_eq!(out.stdout, "No sessions to stop.\n");
    assert_eq!(out.stderr, "");
}

/// node: tests/up-down.test.ts:173-353 — tag sync on a running session.
#[test]
fn up_syncs_tags_to_running_sessions() {
    let rig = Rig::new();
    let dir = rig.write_toml("sync", "[sessions.syncme]\ncommand = \"cat\"\n");
    rig.ok(&["up", dir.to_str().unwrap()]);
    let name = by_display(&rig, "syncme")["name"]
        .as_str()
        .unwrap()
        .to_string();
    rig.ok(&["tag", &name, "custom=yes", "strategy.status=flapping"]);

    rig.write_toml(
        "sync",
        "[sessions.syncme]\ncommand = \"cat\"\ntags = { strategy = \"permanent\", role = \"server\" }\n",
    );
    let out = rig.ok(&["up", dir.to_str().unwrap()]);
    assert_eq!(
        out.stdout,
        "  ● syncme (already running, updated tags: strategy=permanent, role=server, -strategy.status)\nAll sessions already running.\n"
    );
    let tags = by_display(&rig, "syncme")["tags"].clone();
    assert_eq!(tags["strategy"], "permanent");
    assert_eq!(tags["role"], "server");
    assert_eq!(tags["custom"], "yes", "manual tags survive");
    assert_eq!(tags["ptyfile.tags"], "role,strategy");
    assert!(tags.get("strategy.status").is_none());

    rig.write_toml(
        "sync",
        "[sessions.syncme]\ncommand = \"cat\"\ntags = { role = \"server\" }\n",
    );
    let out = rig.ok(&["up", dir.to_str().unwrap()]);
    assert_eq!(
        out.stdout,
        "  ● syncme (already running, updated tags: -strategy)\nAll sessions already running.\n"
    );
    let tags = by_display(&rig, "syncme")["tags"].clone();
    assert!(tags.get("strategy").is_none());
    assert_eq!(tags["custom"], "yes");

    let out = rig.ok(&["up", dir.to_str().unwrap()]);
    assert_eq!(
        out.stdout,
        "  ● syncme (already running)\nAll sessions already running.\n"
    );
}

/// node: tests/up-name-decouple.test.ts, tests/up-down.test.ts:130-154, 374-451
#[test]
fn prefix_id_display_name_cwd_and_env() {
    let rig = Rig::new();
    let work = rig.scratch.join("decouple").join("work");
    std::fs::create_dir_all(&work).unwrap();
    let dir = rig.write_toml(
        "decouple",
        "prefix = \"myapp\"\n\n[sessions.web]\ncommand = \"cat\"\n\n[sessions.worker]\nid = \"pinned\"\ndisplay_name = \"My Web Server\"\ncwd = \"work\"\ncommand = \"cat\"\n\n[sessions.worker.env]\nGREETING = \"hello\"\n",
    );
    let out = rig.ok(&["up", dir.to_str().unwrap(), "web", "worker"]);
    assert_eq!(
        out.stdout,
        "  ● myapp-web (started)\n  ● My Web Server (started)\nStarted 2 sessions.\n"
    );
    let web = by_display(&rig, "myapp-web");
    assert_ne!(web["name"], "myapp-web");
    assert_eq!(web["tags"]["ptyfile.session"], "web");
    let worker = by_display(&rig, "My Web Server");
    assert_eq!(worker["name"], "pinned");
    assert_eq!(worker["cwd"], work.to_str().unwrap());
    let meta = rig.read_meta("pinned").unwrap();
    assert_eq!(meta["displayCommand"], "cat");

    // A pinned id that is already in use is refused per session, exit 0.
    let other = rig.write_toml(
        "other",
        "[sessions.dup]\nid = \"pinned\"\ncommand = \"cat\"\n",
    );
    let out = rig.ok(&["up", other.to_str().unwrap()]);
    assert_eq!(out.stderr, "  ✗ dup: id \"pinned\" is already in use.\n");
    assert_eq!(out.stdout, "");
}

/// node: tests/up-down.test.ts:509-539
#[test]
fn manifest_errors() {
    let rig = Rig::new();
    let empty = rig.scratch.join("nomanifest");
    std::fs::create_dir_all(&empty).unwrap();
    let out = rig.run(&["up", empty.to_str().unwrap()]);
    assert_eq!(out.code, 1);
    // Without a pty.toml the token is a session name and the manifest is
    // looked for in the cwd.
    // Compare the DIRECTORY, not the spelling of it. The tool reports where
    // it looked, which is `getcwd()`, and that resolves symlinks; the rig
    // built its path without resolving. On Linux those are the same string
    // and on a Mac they are not, because `/tmp` is a link there.
    let printed = out
        .stderr
        .trim()
        .strip_prefix("No pty.toml found in ")
        .unwrap_or_else(|| panic!("unexpected message: {:?}", out.stderr));
    assert_eq!(
        std::fs::canonicalize(printed).unwrap(),
        std::fs::canonicalize(&rig.scratch).unwrap(),
        "reported {printed:?}, which is not the rig's scratch directory"
    );
    let dir = rig.write_toml("emptycfg", "# empty config\n");
    let out = rig.run(&["up", dir.to_str().unwrap()]);
    assert_eq!(
        out.stderr,
        format!(
            "No sessions defined in {}\n",
            dir.join("pty.toml").display()
        )
    );
    let dir = rig.write_toml("nocmd", "[sessions.x]\ncwd = \".\"\n");
    let out = rig.run(&["down", dir.to_str().unwrap()]);
    assert_eq!(
        out.stderr,
        format!(
            "Session \"x\" in {} is missing a \"command\" field\n",
            dir.join("pty.toml").display()
        )
    );
}

fn missing_boot_manifest_fixture(
    rig: &Rig,
) -> (std::path::PathBuf, cli_common::MissingBootSession) {
    let marker = rig.scratch.join("unknown-replacement");
    let command = format!("touch {}", marker.display());
    let dir = rig.write_toml(
        "unknown",
        &format!(
            "[sessions.bound]\ncommand = {}\n",
            serde_json::to_string(&command).unwrap()
        ),
    );
    let fixture = cli_common::MissingBootSession::new(
        rig,
        "unknown",
        serde_json::json!({
            "ptyfile": dir.join("pty.toml").to_str().unwrap(),
            "ptyfile.session": "bound",
            "ptyfile.tags": ""
        }),
    );
    assert_eq!(fixture.replacement_marker, marker);
    (dir, fixture)
}

#[test]
fn run_same_name_preserves_missing_boot_live_child() {
    let rig = Rig::new();
    let mut fixture = cli_common::MissingBootSession::new(&rig, "unknown", serde_json::json!({}));
    let marker = fixture.replacement_marker.to_str().unwrap();
    let out = rig.run(&["run", "-d", "--id", "unknown", "--", "touch", marker]);
    assert_eq!(out.code, 1, "{out:?}");
    assert!(out.stderr.contains("already in use"), "{out:?}");
    fixture.assert_preserved(&rig);
}

#[test]
fn up_bound_manifest_preserves_missing_boot_live_child() {
    let rig = Rig::new();
    let (dir, mut fixture) = missing_boot_manifest_fixture(&rig);
    let out = rig.run(&["up", dir.to_str().unwrap()]);
    // Manifest commands retain their per-session refusal / exit-0 contract.
    assert_eq!(out.code, 0, "{out:?}");
    assert!(out.stderr.contains("identity is unknown"), "{out:?}");
    assert!(!out.stdout.contains("(started)"), "{out:?}");
    fixture.assert_preserved(&rig);
}

#[test]
fn down_bound_manifest_preserves_missing_boot_live_child() {
    let rig = Rig::new();
    let (dir, mut fixture) = missing_boot_manifest_fixture(&rig);
    let out = rig.run(&["down", dir.to_str().unwrap()]);
    assert_eq!(out.code, 1, "{out:?}");
    assert!(out.stderr.contains("identity is unknown"), "{out:?}");
    assert!(
        !out.stdout.contains("(cleaned up)") && !out.stdout.contains("(stopped)"),
        "{out:?}"
    );
    fixture.assert_preserved(&rig);
}

#[test]
fn restart_yes_preserves_missing_boot_live_child() {
    let rig = Rig::new();
    let mut fixture = cli_common::MissingBootSession::new(&rig, "unknown", serde_json::json!({}));
    let out = rig.run(&["restart", "-y", "unknown"]);
    assert_eq!(out.code, 1, "{out:?}");
    assert!(out.stderr.contains("identity is unknown"), "{out:?}");
    assert!(!out.stdout.contains("restarted"), "{out:?}");
    fixture.assert_preserved(&rig);
}

#[test]
fn run_same_name_preserves_conflicting_live_sidecar() {
    let rig = Rig::new();
    let mut fixture =
        cli_common::MissingBootSession::new(&rig, "conflicting-owner", serde_json::json!({}));
    fixture.retain_departed_metadata(&rig);
    let out = rig.run(&["run", "-d", "--id", "conflicting-owner", "--", "true"]);
    assert_eq!(out.code, 1, "{out:?}");
    assert!(out.stderr.contains("already in use"), "{out:?}");
    fixture.assert_preserved(&rig);
}

#[test]
fn run_reuses_pidless_metadata_name_without_companions() {
    let rig = Rig::new();
    rig.write_meta(
        "legacy",
        serde_json::json!({"generation": "legacy-history", "displayName": "legacy recipe"}),
    );
    assert!(!rig.path("legacy.pid").exists());
    assert!(!rig.path("legacy.sock").exists());
    rig.spawn_cat("legacy", &[]);
    let current = rig.read_meta("legacy").unwrap();
    assert_ne!(current["generation"], "legacy-history");
    assert!(current["daemonPid"].as_u64().is_some());
    assert_eq!(
        rig.ok(&["list", "--json"]).json()[0]["status"],
        "running",
    );
}
