//! `pty list` / `ls`: JSON shape and key order, filters, `--summary`, sort
//! order, tag rendering, and the exact text layout.
//!
//! node: tests/list-filters.test.ts, tests/tags.test.ts:118-240,
//! tests/gc-flap-clear-badge-root-len.test.ts:118-158

mod cli_common;

use cli_common::{DEAD_PID, Rig, iso_now};
use serde_json::json;

#[test]
fn strict_requires_json_and_rejects_other_inventory_views() {
    let rig = Rig::new();
    for args in [
        vec!["list", "--strict"],
        vec!["ls", "--strict", "--clients"],
    ] {
        let out = rig.run(&args);
        assert_eq!(out.code, 1);
        assert_eq!(out.stderr, "--strict requires --json\n");
        assert!(out.stdout.is_empty());
    }
    for flag in ["--summary", "--remote"] {
        let out = rig.run(&["list", "--json", "--strict", flag]);
        assert_eq!(out.code, 1);
        assert_eq!(
            out.stderr,
            "--strict is incompatible with --summary and --remote\n"
        );
        assert!(out.stdout.is_empty());
    }
    let out = rig.run(&["list", "--json", "--strict", "--remote", "peer"]);
    assert_eq!(out.code, 1);
    assert!(out.stdout.is_empty());
}

#[test]
fn strict_preserves_entry_serialization_and_filters() {
    let rig = Rig::new();
    rig.write_meta(
        "old",
        json!({"createdAt": iso_now(-7_200_000), "tags": {"env": "prod"}}),
    );
    rig.write_meta("recent", json!({"exitCode": 0, "exitedAt": iso_now(0)}));
    let filters = [
        "--status",
        "vanished",
        "--older-than",
        "1h",
        "--filter-tag",
        "env=prod",
    ];
    let mut ordinary = vec!["list", "--json"];
    ordinary.extend(filters);
    let expected = rig.ok(&ordinary).json();
    let mut strict = vec!["ls", "--json", "--strict"];
    strict.extend(filters);
    let out = rig.ok(&strict);
    let inventory = out.json();
    assert_eq!(inventory["root"], json!(rig.root));
    assert_eq!(inventory["complete"], true);
    assert_eq!(inventory["entries"], expected);
    assert_eq!(inventory["errors"], json!([]));
    assert_eq!(out.stdout.lines().count(), 1);
}

#[test]
fn strict_reports_full_scan_errors_even_when_entries_are_filtered_out() {
    let rig = Rig::new();
    rig.write_meta("good", json!({"exitCode": 0}));
    std::fs::write(rig.path("bad.json"), "{").unwrap();
    let out = rig.run(&["list", "--json", "--strict", "--status", "running"]);
    assert_eq!(out.code, 3);
    assert!(out.stderr.is_empty());
    let inventory = out.json();
    assert_eq!(inventory["root"], json!(rig.root));
    assert_eq!(inventory["complete"], false);
    assert_eq!(inventory["entries"], json!([]));
    assert!(inventory["errors"].as_array().unwrap().iter().any(|error| {
        error["path"] == json!(rig.path("bad.json")) && error["kind"] == "metadata-malformed"
    }));
    assert_eq!(
        rig.ok(&["list", "--json", "--status", "exited"]).json()[0]["name"],
        "good"
    );
}

#[test]
fn strict_missing_root_uses_global_root_and_returns_an_envelope() {
    let rig = Rig::new();
    let missing = rig.scratch.join("missing");
    let out = rig.run(&[
        "list",
        "--json",
        "--strict",
        "--root",
        missing.to_str().unwrap(),
    ]);
    assert_eq!(out.code, 3);
    assert!(out.stderr.is_empty());
    assert!(!missing.exists());
    let inventory = out.json();
    assert_eq!(inventory["root"], json!(missing));
    assert_eq!(inventory["complete"], false);
    assert_eq!(inventory["entries"], json!([]));
    assert_eq!(inventory["errors"][0]["path"], json!(missing));
    assert_eq!(inventory["errors"][0]["kind"], "root-missing");
}

/// node: tests/list-filters.test.ts:119-159, 193-210
#[test]
fn json_status_and_key_order() {
    let rig = Rig::new();
    rig.write_meta(
        "van",
        json!({"displayCommand": "sleep 1", "tags": {"a": "1"}}),
    );
    rig.write_meta(
        "ex",
        json!({"exitCode": 0, "exitedAt": iso_now(0), "displayName": "friendly"}),
    );
    // A dead pid without a socket is vanished; its files are kept.
    rig.write_meta("deadpid", json!({}));
    std::fs::write(rig.path("deadpid.pid"), DEAD_PID.to_string()).unwrap();

    let out = rig.ok(&["list", "--json"]);
    let arr = out.json();
    let arr = arr.as_array().unwrap();
    let names: Vec<&str> = arr.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["deadpid", "ex", "van"]);
    let van = arr.iter().find(|s| s["name"] == "van").unwrap();
    assert_eq!(van["status"], "vanished");
    assert_eq!(van["exitCode"], serde_json::Value::Null);
    assert_eq!(van["exitedAt"], serde_json::Value::Null);
    assert_eq!(van["pid"], serde_json::Value::Null);
    let keys: Vec<&String> = van.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "name",
            "status",
            "pid",
            "command",
            "cwd",
            "createdAt",
            "exitCode",
            "exitedAt",
            "tags"
        ]
    );
    let ex = arr.iter().find(|s| s["name"] == "ex").unwrap();
    assert_eq!(ex["status"], "exited");
    assert_eq!(ex["exitCode"], 0);
    let keys: Vec<&String> = ex.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "name",
            "status",
            "pid",
            "command",
            "cwd",
            "createdAt",
            "exitCode",
            "exitedAt",
            "displayName"
        ]
    );
    assert_eq!(
        arr.iter().find(|s| s["name"] == "deadpid").unwrap()["status"],
        "vanished"
    );
    assert!(rig.exists("deadpid.pid"));
    assert!(rig.exists("deadpid.json"));
    // Single line, no pretty print.
    assert_eq!(out.stdout.lines().count(), 1);
}

/// node: tests/list-filters.test.ts:214-294
#[test]
fn status_and_age_filters() {
    let rig = Rig::new();
    rig.write_meta(
        "old",
        json!({"createdAt": iso_now(-2 * 3_600_000), "tags": {"env": "prod"}}),
    );
    rig.write_meta("recent", json!({"exitCode": 1, "exitedAt": iso_now(0)}));
    let names = |args: &[&str]| -> Vec<String> {
        rig.ok(args)
            .json()
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(names(&["list", "--json", "--status", "vanished"]), ["old"]);
    assert_eq!(names(&["list", "--json", "--status", "exited"]), ["recent"]);
    assert_eq!(
        names(&["list", "--json", "--status", "running"]),
        Vec::<String>::new()
    );
    assert_eq!(names(&["list", "--json", "--older-than", "1h"]), ["old"]);
    assert_eq!(names(&["list", "--json", "--newer-than", "1h"]), ["recent"]);
    assert_eq!(
        names(&[
            "list",
            "--json",
            "--older-than",
            "1h",
            "--filter-tag",
            "env=prod"
        ]),
        ["old"]
    );
    assert_eq!(
        names(&[
            "list",
            "--json",
            "--older-than",
            "1h",
            "--filter-tag",
            "env=dev"
        ]),
        Vec::<String>::new()
    );

    let out = rig.run(&["list", "--status", "bogus"]);
    assert_eq!(out.code, 1);
    assert_eq!(
        out.stderr,
        "--status expects one of: running, exited, vanished\n"
    );
    let out = rig.run(&["list", "--status"]);
    assert_eq!(
        out.stderr,
        "--status expects one of: running, exited, vanished\n"
    );
    let out = rig.run(&["list", "--older-than", "1week"]);
    assert_eq!(out.code, 1);
    assert_eq!(
        out.stderr,
        "--older-than expects a duration like 30s, 5m, 2h, 1d\n"
    );
    let out = rig.run(&["list", "--newer-than"]);
    assert_eq!(
        out.stderr,
        "--newer-than expects a duration like 30s, 5m, 2h, 1d\n"
    );
    let out = rig.run(&["list", "--filter-tag", "nope"]);
    assert_eq!(out.code, 1);
    assert_eq!(out.stderr, "--filter-tag expects \"key=value\"\n");
    // Unknown tokens are ignored.
    assert_eq!(
        rig.ok(&["list", "--json", "--bogus", "extra"])
            .json()
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

/// node: tests/list-filters.test.ts:298-362
#[test]
fn summary_text_and_json() {
    let rig = Rig::new();
    rig.write_meta(
        "old",
        json!({"createdAt": iso_now(-2 * 3_600_000), "exitCode": 0, "exitedAt": iso_now(-3_600_000), "displayName": "Old One"}),
    );
    rig.write_meta("recent", json!({"createdAt": iso_now(-5_000)}));
    let out = rig.ok(&["list", "--summary"]);
    assert_eq!(
        out.stdout,
        "2 sessions — 1 exited, 1 vanished\noldest: Old One (old) (exited, 2h)\nnewest: recent (vanished, 5s)\n"
    );
    let s = rig.ok(&["list", "--json", "--summary"]).json();
    assert_eq!(s["total"], 2);
    assert_eq!(
        s["byStatus"],
        json!({"running": 0, "exited": 1, "vanished": 1})
    );
    assert_eq!(s["oldest"]["name"], "old");
    assert_eq!(s["oldest"]["status"], "exited");
    assert_eq!(s["oldest"]["displayName"], "Old One");
    assert!(s["oldest"]["ageSeconds"].as_i64().unwrap() >= 7195);
    assert_eq!(s["newest"]["name"], "recent");
    assert!(s["newest"].get("displayName").is_none());
    let keys: Vec<&String> = s.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["total", "byStatus", "oldest", "newest"]);
    let keys: Vec<&String> = s["oldest"].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["name", "status", "ageSeconds", "displayName"]);

    let s = rig
        .ok(&["list", "--json", "--summary", "--status", "vanished"])
        .json();
    assert_eq!(s["total"], 1);
    assert_eq!(s["oldest"]["name"], "recent");
    assert_eq!(s["newest"]["name"], "recent");
    assert_eq!(
        rig.ok(&["list", "--summary", "--status", "running"]).stdout,
        "No matching sessions.\n"
    );
    // One session: singular, and no `newest` line when it is the oldest.
    let rig2 = Rig::new();
    rig2.write_meta("only", json!({"createdAt": iso_now(-65_000)}));
    assert_eq!(
        rig2.ok(&["list", "--summary"]).stdout,
        "1 session — 1 vanished\noldest: only (vanished, 1m5s)\n"
    );
    assert_eq!(
        rig2.ok(&["list", "--json", "--summary", "--status", "exited"])
            .json(),
        json!({"total": 0, "byStatus": {"running": 0, "exited": 0, "vanished": 0}, "oldest": null, "newest": null})
    );
}

/// node: tests/list-filters.test.ts:386-437
#[test]
fn sorted_by_display_name_or_name() {
    let rig = Rig::new();
    rig.write_meta("zzz-raw", json!({}));
    rig.write_meta("aaa", json!({"displayName": "mmm-friendly"}));
    rig.write_meta("bbb-raw", json!({}));
    rig.write_meta("yyy", json!({"displayName": "bbb-friendly"}));
    let names: Vec<String> = rig
        .ok(&["list", "--json"])
        .json()
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["yyy", "bbb-raw", "aaa", "zzz-raw"]);
}

/// node: tests/tags.test.ts:154-218, tests/gc-flap-clear-badge-root-len.test.ts:118-158,
/// src/cli.ts:2314-2445 — the exact text, SGR codes included.
#[test]
fn text_layout_is_byte_exact() {
    let rig = Rig::new();
    assert_eq!(rig.ok(&["list"]).stdout, "No active sessions.\n");
    let home = std::env::var("HOME").unwrap();
    let created = iso_now(-90_000);
    rig.write_meta(
        "van1",
        json!({"createdAt": created, "cwd": format!("{home}/proj"), "displayCommand": "node app.js",
               "tags": {"role": "web", "ptyfile": "/x/pty.toml", "ptyfile.session": "w", "ptyfile.tags": "role",
                        "strategy": "permanent", ":l1234-abc": "1", "env": "dev"}}),
    );
    rig.write_meta(
        "ex1",
        json!({"exitCode": 3, "exitedAt": iso_now(-5_000), "cwd": home, "displayCommand": "cat",
               "displayName": "Pretty", "tags": {"strategy.status": "flapping", "strategy": "permanent"}}),
    );
    let out = rig.ok(&["list"]);
    assert_eq!(
        out.stdout,
        "Exited sessions:\n  \x1b[1mPretty\x1b[0m \x1b[2m(ex1)\x1b[0m \x1b[31m[flapping]\x1b[0m #strategy.status=flapping (exited with code 3, 5s ago) — ~ — \x1b[2mcat\x1b[0m\n\n\x1b[33mVanished sessions (no exit record — killed or crashed):\x1b[0m\n  ⚠ \x1b[1;33mvan1\x1b[0m \x1b[33m[permanent]\x1b[0m #role=web #env=dev (vanished, started 1m ago) — ~/proj — \x1b[2mnode app.js\x1b[0m\n"
    );
    let out = rig.ok(&["list", "--tags", "--status", "vanished"]);
    assert_eq!(
        out.stdout,
        "\x1b[33mVanished sessions (no exit record — killed or crashed):\x1b[0m\n  ⚠ \x1b[1;33mvan1\x1b[0m \x1b[33m[permanent]\x1b[0m #role=web #ptyfile=/x/pty.toml #ptyfile.session=w #ptyfile.tags=role #strategy=permanent #:l1234-abc=1 #env=dev (vanished, started 1m ago) — ~/proj — \x1b[2mnode app.js\x1b[0m\n"
    );
    // A `--filter-tag` narrows the text listing too.
    let out = rig.ok(&["list", "--filter-tag", "env=dev"]);
    assert!(out.stdout.contains("van1"));
    assert!(!out.stdout.contains("Pretty"));
}

/// node: tests/tags.test.ts:118-152 — `tags` in JSON, AND filtering.
#[test]
fn json_tags_and_filters() {
    let rig = Rig::new();
    rig.write_meta("a", json!({"tags": {"owner": "myapp", "layout": "work"}}));
    rig.write_meta("b", json!({"tags": {"layout": "work"}}));
    rig.write_meta("c", json!({"tags": {}}));
    rig.write_meta("d", json!({}));
    let arr = rig.ok(&["list", "--json"]).json();
    let a = arr
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "a")
        .unwrap();
    assert_eq!(a["tags"], json!({"owner": "myapp", "layout": "work"}));
    let c = arr
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "c")
        .unwrap();
    assert_eq!(c["tags"], json!({}));
    let d = arr
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "d")
        .unwrap();
    assert!(d.get("tags").is_none());
    let names = |args: &[&str]| -> Vec<String> {
        rig.ok(args)
            .json()
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        names(&["list", "--json", "--filter-tag", "layout=work"]),
        ["a", "b"]
    );
    assert_eq!(
        names(&[
            "list",
            "--json",
            "--filter-tag",
            "layout=work",
            "--filter-tag",
            "owner=myapp"
        ]),
        ["a"]
    );
}

/// node: src/cli.ts:2223-2247, 2307-2311 — bare `--remote` asks pty-relay
/// which peers to try, so with no relay there are no host groups and the
/// output is the plain local array.
#[test]
fn bare_remote_without_hosts_prints_the_local_array() {
    let rig = Rig::new();
    rig.write_meta("x", json!({}));
    let out = rig.ok(&["list", "--json", "--remote"]);
    assert!(out.json().is_array());
}

/// A NAMED peer is always a host group, even when it cannot be dialed: the
/// group carries the reason and the output takes the `{local, remote}`
/// shape. `crates/pty-conformance/tests/remote_fabric.rs` pins the same
/// thing against the Node binary.
///
/// node: src/cli.ts:2223-2247
#[test]
fn a_named_peer_that_cannot_be_dialed_is_a_host_group_with_an_error() {
    let rig = Rig::new();
    rig.write_meta("x", json!({}));
    let out = rig.ok(&["list", "--json", "--remote", "somepeer"]);
    let v = out.json();
    assert!(v["local"].is_array(), "{v}");
    assert_eq!(v["remote"][0]["label"], "somepeer", "{v}");
    assert!(
        !v["remote"][0]["error"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "{v}"
    );
}
