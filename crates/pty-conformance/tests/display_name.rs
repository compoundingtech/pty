//! Port of tests/display-name.test.ts: random ids and auto display names,
//! `--id`/`--name`/`--no-display-name`, display-name validation, `rename`,
//! lookup by display name, ambiguity across every verb, long display names.

use pty_conformance::*;

const ID_RE: &str = r"^[a-z0-9]{6,12}$";

fn run_d(rig: &Rig, args: &[&str]) -> Out {
    let mut argv = vec!["run", "-d"];
    argv.extend_from_slice(args);
    argv.extend_from_slice(&["--", "cat"]);
    let out = rig.pty(&argv);
    expect_status(&out, 0);
    out
}

/// node: tests/display-name.test.ts:67
#[test]
fn default_run_generates_random_name_and_auto_display_name() {
    let rig = Rig::new();
    run_d(&rig, &[]);
    let sessions = rig.list_json();
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    let s = &sessions[0];
    expect_regex(s["name"].as_str().unwrap(), ID_RE);
    let dn = s["displayName"].as_str().expect("displayName is a string");
    assert!(!dn.is_empty());
}

/// node: tests/display-name.test.ts:84
#[test]
fn no_display_name_leaves_display_name_unset() {
    let rig = Rig::new();
    run_d(&rig, &["--no-display-name"]);
    let sessions = rig.list_json();
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    expect_regex(sessions[0]["name"].as_str().unwrap(), ID_RE);
    assert!(sessions[0].get("displayName").is_none(), "{:?}", sessions[0]);
}

/// node: tests/display-name.test.ts:99
#[test]
fn id_pins_the_name_and_auto_generates_display_name() {
    let rig = Rig::new();
    run_d(&rig, &["--id", "mysvc"]);
    let s = rig.list_entry("mysvc").expect("mysvc listed");
    let dn = s["displayName"].as_str().unwrap_or("");
    assert!(!dn.is_empty(), "{s:?}");
}

/// node: tests/display-name.test.ts:111
#[test]
fn id_with_no_display_name_skips_display_name() {
    let rig = Rig::new();
    run_d(&rig, &["--id", "raw", "--no-display-name"]);
    let s = rig.list_entry("raw").expect("raw listed");
    assert!(s.get("displayName").is_none(), "{s:?}");
}

/// node: tests/display-name.test.ts:122
#[test]
fn id_with_name_pins_both() {
    let rig = Rig::new();
    run_d(&rig, &["--id", "svc", "--name", "My Pretty Service"]);
    let s = rig.list_entry("svc").expect("svc listed");
    assert_eq!(s["displayName"], "My Pretty Service");
}

/// node: tests/display-name.test.ts:134
#[test]
fn rejects_an_id_above_the_logical_id_limit() {
    let rig = Rig::new();
    let long_id = "x".repeat(256);
    let out = rig.pty(&["run", "-d", "--id", &long_id, "--", "cat"]);
    expect_failure(&out);
    expect_regex(&out.stderr(), "(?i)too long.*255");
}

/// node: tests/display-name.test.ts:142
#[test]
fn rejects_an_id_that_collides_with_an_existing_session() {
    let rig = Rig::new();
    run_d(&rig, &["--id", "dup"]);
    let out = rig.pty(&["run", "-d", "--id", "dup", "--", "cat"]);
    expect_failure(&out);
    expect_contains(&out.stderr(), "already in use");
}

/// node: tests/display-name.test.ts:153
#[test]
fn accepts_a_long_display_label() {
    let rig = Rig::new();
    let label = "My Very Long Display Label With Spaces and Punctuation";
    run_d(&rig, &["--name", label]);
    let sessions = rig.list_json();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["displayName"], label);
    let name = sessions[0]["name"].as_str().unwrap();
    expect_regex(name, ID_RE);
    assert_ne!(name, label);
}

/// node: tests/display-name.test.ts:168
#[test]
fn allows_an_id_equal_to_the_name() {
    let rig = Rig::new();
    run_d(&rig, &["--id", "same", "--name", "same"]);
    assert_eq!(rig.list_entry("same").unwrap()["displayName"], "same");
}

/// node: tests/display-name.test.ts:176
#[test]
fn allows_duplicate_display_names() {
    let rig = Rig::new();
    run_d(&rig, &["--id", "a1", "--name", "shared"]);
    run_d(&rig, &["--id", "a2", "--name", "shared"]);
    let names: Vec<String> = rig
        .list_json()
        .into_iter()
        .filter(|s| s["displayName"] == "shared")
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["a1", "a2"]);
}

/// node: tests/display-name.test.ts:186
#[test]
fn rejects_invalid_display_names_before_spawning() {
    let cases: Vec<(&str, String)> = vec![
        ("leading whitespace", " Worker".into()),
        ("trailing whitespace", "Worker ".into()),
        ("ASCII control", "Worker\u{7}".into()),
        ("Unicode line separator", "Worker\u{2028}Next".into()),
        ("Unicode paragraph separator", "Worker\u{2029}Next".into()),
        ("more than 160 Unicode scalars", "😀".repeat(161)),
    ];
    for (case, display_name) in cases {
        let rig = Rig::new();
        let id = unique_id("invalid");
        let out = rig.pty(&["run", "-d", "--id", &id, "--name", &display_name, "--", "cat"]);
        assert_ne!(out.status, 0, "{case}: expected rejection: {}", out.summary());
        assert_eq!(rig.list_json(), Vec::<serde_json::Value>::new(), "{case}: session was created");
    }
}

/// node: tests/display-name.test.ts:203
#[test]
fn accepts_160_scalars_with_slash_and_backslash() {
    let rig = Rig::new();
    let display_name = format!("{}/a\\b", "😀".repeat(156));
    run_d(&rig, &["--id", "unicode-boundary", "--name", &display_name]);
    assert_eq!(rig.list_entry("unicode-boundary").unwrap()["displayName"], display_name);
}

// ── pty rename (outside a session) ──

/// node: tests/display-name.test.ts:215
#[test]
fn rename_sets_display_name() {
    let rig = Rig::new();
    run_d(&rig, &["--id", "webapp", "--no-display-name"]);
    let out = rig.pty(&["rename", "webapp", "my-label"]);
    expect_status(&out, 0);
    expect_contains(&out.stdout(), "my-label");
    assert_eq!(rig.list_entry("webapp").unwrap()["displayName"], "my-label");
}

/// node: tests/display-name.test.ts:227
#[test]
fn rename_show_prints_the_current_display_name() {
    let rig = Rig::new();
    run_d(&rig, &["--id", "api", "--no-display-name"]);
    expect_status(&rig.pty(&["rename", "api", "friendly-api"]), 0);
    let out = rig.pty(&["rename", "--show", "api"]);
    expect_status(&out, 0);
    assert_eq!(out.stdout().trim(), "friendly-api");
}

/// node: tests/display-name.test.ts:238
#[test]
fn rename_show_without_display_name_prints_a_hint() {
    let rig = Rig::new();
    run_d(&rig, &["--id", "bare", "--no-display-name"]);
    let out = rig.pty(&["rename", "--show", "bare"]);
    expect_status(&out, 0);
    expect_contains(&out.stdout(), "no displayName");
}

/// node: tests/display-name.test.ts:247
#[test]
fn rename_clear_removes_display_name() {
    let rig = Rig::new();
    run_d(&rig, &["--id", "svc", "--no-display-name"]);
    expect_status(&rig.pty(&["rename", "svc", "pretty"]), 0);
    assert_eq!(rig.list_entry("svc").unwrap()["displayName"], "pretty");
    let out = rig.pty(&["rename", "--clear", "svc"]);
    expect_status(&out, 0);
    assert!(rig.list_entry("svc").unwrap().get("displayName").is_none());
}

/// node: tests/display-name.test.ts:258
#[test]
fn rename_with_one_positional_outside_a_session_errors() {
    let rig = Rig::new();
    let out = rig.pty(&["rename", "only-one"]);
    expect_failure(&out);
    expect_contains(&out.stderr(), "only allowed inside a pty session");
}

/// node: tests/display-name.test.ts:265
#[test]
fn display_name_may_collide_with_another_stable_id() {
    let rig = Rig::new();
    run_d(&rig, &["--id", "aaa", "--no-display-name"]);
    run_d(&rig, &["--id", "bbb", "--no-display-name"]);
    let out = rig.pty(&["rename", "aaa", "bbb"]);
    expect_status(&out, 0);
    assert_eq!(rig.list_entry("aaa").unwrap()["displayName"], "bbb");
}

/// node: tests/display-name.test.ts:276
#[test]
fn display_name_may_equal_the_sessions_own_id() {
    let rig = Rig::new();
    run_d(&rig, &["--id", "same", "--no-display-name"]);
    let out = rig.pty(&["rename", "same", "same"]);
    expect_status(&out, 0);
    assert_eq!(rig.list_entry("same").unwrap()["displayName"], "same");
}

// ── pty rename (inside a session) ──

/// node: tests/display-name.test.ts:287
#[test]
fn rename_inside_a_session_renames_the_current_session() {
    let rig = Rig::new();
    run_d(&rig, &["--id", "insider", "--no-display-name"]);
    let out = rig.pty_env(&[("PTY_SESSION", "insider")], &["rename", "from-inside"]);
    expect_status(&out, 0);
    assert_eq!(rig.list_entry("insider").unwrap()["displayName"], "from-inside");
}

/// node: tests/display-name.test.ts:297
#[test]
fn rename_clear_inside_a_session_clears_the_current_session() {
    let rig = Rig::new();
    run_d(&rig, &["--id", "i2", "--no-display-name"]);
    expect_status(&rig.pty_env(&[("PTY_SESSION", "i2")], &["rename", "has-a-display"]), 0);
    let out = rig.pty_env(&[("PTY_SESSION", "i2")], &["rename", "--clear"]);
    expect_status(&out, 0);
    assert!(rig.list_entry("i2").unwrap().get("displayName").is_none());
}

// ── lookup by displayName ──

/// node: tests/display-name.test.ts:310
#[test]
fn stats_resolves_a_display_name() {
    let rig = Rig::new();
    run_d(&rig, &["--id", "raw1", "--no-display-name"]);
    expect_status(&rig.pty(&["rename", "raw1", "friendly"]), 0);
    let out = rig.pty(&["stats", "friendly", "--json"]);
    let stats = expect_json(&out);
    assert_eq!(stats["name"], "raw1");
}

/// node: tests/display-name.test.ts:322
#[test]
fn exact_stable_id_wins_over_matching_display_names() {
    let rig = Rig::new();
    run_d(&rig, &["--id", "other", "--name", "shared"]);
    run_d(&rig, &["--id", "shared", "--no-display-name"]);
    let out = rig.pty(&["stats", "shared", "--json"]);
    assert_eq!(expect_json(&out)["name"], "shared");
}

/// node: tests/display-name.test.ts:334
#[test]
fn every_verb_fails_closed_on_an_ambiguous_display_name() {
    let verbs: &[&[&str]] = &[
        &["attach", "shared"],
        &["peek", "--plain", "shared"],
        &["send", "shared", "hello"],
        &["stats", "shared", "--json"],
        &["events", "--recent", "shared"],
        &["restart", "-y", "shared"],
        &["kill", "shared"],
        &["tag", "shared", "role=test"],
        &["tag-multi", "shared", "role=test"],
        &["emit", "shared", "user.test"],
        &["rename", "--show", "shared"],
        &["rename", "--clear", "shared"],
        &["rename", "shared", "renamed"],
        &["rm", "shared"],
    ];
    let rig = Rig::new();
    run_d(&rig, &["--id", "alpha", "--name", "shared"]);
    run_d(&rig, &["--id", "beta", "--name", "shared"]);
    for argv in verbs {
        let out = rig.pty(argv);
        assert_ne!(out.status, 0, "pty {}: expected failure: {}", argv.join(" "), out.summary());
        let err = out.stderr();
        assert!(
            err.contains("Session reference \"shared\" is ambiguous."),
            "pty {}: {}",
            argv.join(" "),
            out.summary()
        );
        expect_contains(&err, "alpha");
        expect_contains(&err, "beta");
    }
    // Every verb failed closed: both sessions are still running and untouched.
    let list = rig.list_json();
    assert_eq!(list.len(), 2, "{list:?}");
    for s in &list {
        assert_eq!(s["status"], "running", "{s:?}");
        assert_eq!(s["displayName"], "shared", "{s:?}");
    }
}

// ── long displayName resolution ──

const LONG_LABEL: &str = "org.cos.orc-payments-platform.orc-checkout-api.worker-authz-service.subworker-db-migrations.verifier-contracts";

/// node: tests/display-name.test.ts:377
#[test]
fn creates_a_110_char_display_name() {
    let rig = Rig::new();
    assert_eq!(LONG_LABEL.len(), 110);
    run_d(&rig, &["--name", LONG_LABEL]);
    let sessions = rig.list_json();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["displayName"], LONG_LABEL);
}

/// node: tests/display-name.test.ts:387
#[test]
fn peek_resolves_a_long_display_name() {
    let rig = Rig::new();
    run_d(&rig, &["--name", LONG_LABEL]);
    let out = rig.pty(&["peek", "--plain", LONG_LABEL]);
    expect_status(&out, 0);
}

/// node: tests/display-name.test.ts:395
#[test]
fn send_resolves_a_long_display_name() {
    let rig = Rig::new();
    run_d(&rig, &["--name", LONG_LABEL]);
    let out = rig.pty(&["send", LONG_LABEL, "hi"]);
    expect_status(&out, 0);
}

/// node: tests/display-name.test.ts:402
#[test]
fn tag_resolves_a_long_display_name() {
    let rig = Rig::new();
    run_d(&rig, &["--name", LONG_LABEL]);
    let out = rig.pty(&["tag", LONG_LABEL, "role=worker"]);
    expect_status(&out, 0);
    let s = rig
        .list_json()
        .into_iter()
        .find(|s| s["displayName"] == LONG_LABEL)
        .expect("listed");
    assert_eq!(s["tags"]["role"], "worker");
}

/// node: tests/display-name.test.ts:411
#[test]
fn events_recent_resolves_a_long_display_name() {
    let rig = Rig::new();
    run_d(&rig, &["--name", LONG_LABEL]);
    let out = rig.pty(&["events", "--recent", LONG_LABEL]);
    expect_status(&out, 0);
}

/// node: tests/display-name.test.ts:418
#[test]
fn kill_resolves_a_long_display_name() {
    let rig = Rig::new();
    run_d(&rig, &["--name", LONG_LABEL]);
    let out = rig.pty(&["kill", LONG_LABEL]);
    expect_status(&out, 0);
    let running: Vec<_> = rig.list_json().into_iter().filter(|s| s["status"] == "running").collect();
    assert!(running.is_empty(), "{running:?}");
}

// ── restart preserves displayName + tags ──

/// node: tests/display-name.test.ts:433
#[test]
fn restart_keeps_display_name_and_tags() {
    let rig = Rig::new();
    run_d(&rig, &["--id", "svc", "--name", "My Service", "--tag", "role=web"]);
    let s = rig.list_entry("svc").expect("svc listed");
    assert_eq!(s["displayName"], "My Service");
    assert_eq!(s["tags"]["role"], "web");

    // PTY_SESSION makes the CLI take the "inside a session, not attaching"
    // branch instead of trying to attach without a tty.
    let out = rig.pty_env(&[("PTY_SESSION", "outer")], &["restart", "-y", "svc"]);
    expect_status(&out, 0);
    expect_contains(&out.stdout(), "restarted");

    let s = rig.list_entry("svc").expect("svc listed after restart");
    assert_eq!(s["displayName"], "My Service");
    assert_eq!(s["tags"]["role"], "web");
}
