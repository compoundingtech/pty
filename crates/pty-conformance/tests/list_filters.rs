//! Port of tests/list-filters.test.ts: the vanished status, `list` as a pure
//! read, `--status`, `--older-than`/`--newer-than`, `--summary`, and the sort
//! order. Daemon-less records are written straight into the root so the age
//! filters need no wall-clock time.

use pty_conformance::*;

fn names(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect()
}

/// node: tests/list-filters.test.ts:124
#[test]
fn infers_vanished_without_exit_fields_or_a_live_daemon() {
    let rig = Rig::new();
    let name = unique_id("lf");
    write_fake_metadata(rig.root(), &name, FakeMeta::created(0));
    let found = rig.list_entry(&name).expect("listed");
    assert_eq!(found["status"], "vanished");
    assert_eq!(found["exitCode"], serde_json::Value::Null, "{found}");
    assert_eq!(found["exitedAt"], serde_json::Value::Null, "{found}");
}

/// node: tests/list-filters.test.ts:138
#[test]
fn text_output_buckets_vanished_sessions() {
    let rig = Rig::new();
    let name = unique_id("lf");
    write_fake_metadata(rig.root(), &name, FakeMeta::created(0));
    let out = rig.pty(&["list"]);
    expect_status(&out, 0);
    let stdout = out.stdout();
    expect_contains(&stdout, "Vanished sessions");
    expect_contains(&stdout, &name);
}

/// node: tests/list-filters.test.ts:149
#[test]
fn cleanly_exited_sessions_stay_exited() {
    let rig = Rig::new();
    let name = unique_id("lf");
    rig.daemon(&name, &["true"], DaemonOpts::keep());
    rig.wait_for_exit(&name);
    let found = rig.list_entry(&name).expect("listed");
    assert_eq!(found["status"], "exited");
    assert_eq!(found["exitCode"], 0);
    assert!(found["exitedAt"].is_string(), "{found}");
}

// ── listSessions is observational ──

/// node: tests/list-filters.test.ts:172
#[test]
fn keeps_a_session_whose_socket_is_missing_but_pid_is_alive() {
    let rig = Rig::new();
    let name = unique_id("lf");
    // The test process itself stands in for a live daemon.
    std::fs::write(rig.pid_path(&name), std::process::id().to_string()).unwrap();
    // Older than 24 h, no socket: both former cleanup triggers.
    write_fake_metadata(
        rig.root(),
        &name,
        FakeMeta::created(-48 * 3600).identity(std::process::id() as i32),
    );
    let found = rig.list_entry(&name).expect("listed");
    assert_eq!(found["status"], "running", "pid is alive: {found}");
    assert!(rig.meta_path(&name).exists(), "metadata must survive a list");
}

/// node: tests/list-filters.test.ts:200
#[test]
fn reports_old_dead_metadata_without_deleting_it() {
    let rig = Rig::new();
    let name = unique_id("lf");
    std::fs::write(rig.pid_path(&name), "2147483647").unwrap();
    write_fake_metadata(rig.root(), &name, FakeMeta::created(-48 * 3600));
    let found = rig.list_entry(&name).expect("listed");
    assert_eq!(found["status"], "vanished");
    assert!(rig.pid_path(&name).exists());
    assert!(rig.meta_path(&name).exists());
}

// ── --status ──

/// node: tests/list-filters.test.ts:220
#[test]
fn status_filters_to_a_single_status() {
    let rig = Rig::new();
    let live = unique_id("lf");
    rig.daemon(&live, &["cat"], DaemonOpts::no_display_name());
    let gone = unique_id("lf");
    write_fake_metadata(rig.root(), &gone, FakeMeta::created(0).exited(0, 0));
    let lost = unique_id("lf");
    write_fake_metadata(rig.root(), &lost, FakeMeta::created(0));

    let only_running = expect_json(&rig.pty(&["list", "--json", "--status", "running"]));
    assert_eq!(names(&only_running), vec![live.clone()]);
    let only_exited = expect_json(&rig.pty(&["list", "--json", "--status", "exited"]));
    assert_eq!(names(&only_exited), vec![gone.clone()]);
    let only_vanished = expect_json(&rig.pty(&["list", "--json", "--status", "vanished"]));
    assert_eq!(names(&only_vanished), vec![lost.clone()]);
}

/// node: tests/list-filters.test.ts:245
#[test]
fn rejects_an_invalid_status_value() {
    let rig = Rig::new();
    let out = rig.pty(&["list", "--status", "bogus"]);
    expect_failure(&out);
    expect_contains(&out.stderr(), "--status expects");
}

// ── --older-than / --newer-than ──

/// node: tests/list-filters.test.ts:254
#[test]
fn older_than_filters_out_recent_sessions() {
    let rig = Rig::new();
    let old = unique_id("lfold");
    let recent = unique_id("lfnew");
    write_fake_metadata(rig.root(), &old, FakeMeta::created(-2 * 3600));
    write_fake_metadata(rig.root(), &recent, FakeMeta::created(0));
    let out = expect_json(&rig.pty(&["list", "--json", "--older-than", "1h"]));
    assert_eq!(names(&out), vec![old]);
}

/// node: tests/list-filters.test.ts:267
#[test]
fn newer_than_filters_out_old_sessions() {
    let rig = Rig::new();
    let old = unique_id("lfold");
    let recent = unique_id("lfnew");
    write_fake_metadata(rig.root(), &old, FakeMeta::created(-2 * 3600));
    write_fake_metadata(rig.root(), &recent, FakeMeta::created(0));
    let out = expect_json(&rig.pty(&["list", "--json", "--newer-than", "1h"]));
    assert_eq!(names(&out), vec![recent]);
}

/// node: tests/list-filters.test.ts:281
#[test]
fn rejects_a_malformed_duration() {
    let rig = Rig::new();
    let out = rig.pty(&["list", "--older-than", "1week"]);
    expect_failure(&out);
    expect_contains(&out.stderr(), "duration");
}

/// node: tests/list-filters.test.ts:288
#[test]
fn age_filters_compose_with_filter_tag() {
    let rig = Rig::new();
    let old_match = unique_id("lfm");
    let old_skip = unique_id("lfs");
    write_fake_metadata(rig.root(), &old_match, FakeMeta::created(-2 * 3600).tag("env", "prod"));
    write_fake_metadata(rig.root(), &old_skip, FakeMeta::created(-2 * 3600).tag("env", "dev"));
    let out = expect_json(&rig.pty(&[
        "list", "--json", "--older-than", "1h", "--filter-tag", "env=prod",
    ]));
    assert_eq!(names(&out), vec![old_match]);
}

// ── --summary ──

/// node: tests/list-filters.test.ts:305
#[test]
fn summary_text_emits_counts_and_oldest_newest() {
    let rig = Rig::new();
    let old_name = unique_id("lfold");
    let recent_name = unique_id("lfnew");
    write_fake_metadata(rig.root(), &old_name, FakeMeta::created(-2 * 3600).exited(-60, 0));
    write_fake_metadata(rig.root(), &recent_name, FakeMeta::created(0));
    let out = rig.pty(&["list", "--summary"]);
    expect_status(&out, 0);
    let stdout = out.stdout();
    expect_contains(&stdout, "2 sessions");
    expect_contains(&stdout, "1 exited");
    expect_contains(&stdout, "1 vanished");
    expect_contains(&stdout, &format!("oldest: {old_name}"));
    expect_contains(&stdout, &format!("newest: {recent_name}"));
}

/// node: tests/list-filters.test.ts:325
#[test]
fn summary_json_is_structured() {
    let rig = Rig::new();
    let only = unique_id("lf");
    write_fake_metadata(rig.root(), &only, FakeMeta::created(-5 * 60));
    let payload = expect_json(&rig.pty(&["list", "--json", "--summary"]));
    assert_eq!(payload["total"], 1, "{payload}");
    assert_eq!(payload["byStatus"]["vanished"], 1, "{payload}");
    assert_eq!(payload["byStatus"]["exited"], 0, "{payload}");
    assert_eq!(payload["byStatus"]["running"], 0, "{payload}");
    assert_eq!(payload["oldest"]["name"], only, "{payload}");
    assert_eq!(payload["oldest"]["status"], "vanished", "{payload}");
    assert!(payload["oldest"]["ageSeconds"].as_f64().unwrap() >= 295.0, "{payload}");
    assert_eq!(payload["newest"]["name"], only, "{payload}");
}

/// node: tests/list-filters.test.ts:343
#[test]
fn summary_respects_status_filter() {
    let rig = Rig::new();
    let exited = unique_id("lfx");
    let lost = unique_id("lfl");
    write_fake_metadata(rig.root(), &exited, FakeMeta::created(-60).exited(0, 0));
    write_fake_metadata(rig.root(), &lost, FakeMeta::created(0));
    let payload = expect_json(&rig.pty(&["list", "--json", "--summary", "--status", "vanished"]));
    assert_eq!(payload["total"], 1, "{payload}");
    assert_eq!(payload["byStatus"]["vanished"], 1, "{payload}");
    assert_eq!(payload["byStatus"]["exited"], 0, "{payload}");
    assert_eq!(payload["oldest"]["name"], lost, "{payload}");
}

/// node: tests/list-filters.test.ts:361
#[test]
fn summary_reports_no_matching_sessions() {
    let rig = Rig::new();
    write_fake_metadata(rig.root(), &unique_id("lf"), FakeMeta::created(0));
    let out = rig.pty(&["list", "--summary", "--status", "running"]);
    expect_contains(&out.stdout(), "No matching sessions.");
}

// ── sort order ──

fn sort_keys(v: &[serde_json::Value]) -> Vec<String> {
    v.iter()
        .map(|s| s["displayName"].as_str().or(s["name"].as_str()).unwrap().to_string())
        .collect()
}

/// node: tests/list-filters.test.ts:391
#[test]
fn json_output_is_sorted_by_display_name_then_name() {
    let rig = Rig::new();
    write_fake_metadata(rig.root(), "zzz-raw", FakeMeta::created(0));
    write_fake_metadata(rig.root(), "aaa-raw", FakeMeta::created(0).display_name("mmm-friendly"));
    write_fake_metadata(rig.root(), "mmm-raw", FakeMeta::created(0).display_name("bbb-friendly"));
    write_fake_metadata(rig.root(), "bbb-raw", FakeMeta::created(0));
    let sessions = rig.list_json();
    assert_eq!(sort_keys(&sessions), vec!["bbb-friendly", "bbb-raw", "mmm-friendly", "zzz-raw"]);
}

/// node: tests/list-filters.test.ts:411
#[test]
fn text_output_renders_buckets_in_sorted_order() {
    let rig = Rig::new();
    write_fake_metadata(rig.root(), "z1", FakeMeta::created(0));
    write_fake_metadata(rig.root(), "a1", FakeMeta::created(0));
    write_fake_metadata(rig.root(), "m1", FakeMeta::created(0));
    let out = rig.pty(&["list"]);
    expect_status(&out, 0);
    let stdout = out.stdout();
    let ia = stdout.find("a1").expect("a1 listed");
    let im = stdout.find("m1").expect("m1 listed");
    let iz = stdout.find("z1").expect("z1 listed");
    assert!(im > ia && iz > im, "{stdout}");
}

/// node: tests/list-filters.test.ts:428
#[test]
fn display_name_beats_the_stable_id_when_sorting() {
    let rig = Rig::new();
    write_fake_metadata(rig.root(), "aaa", FakeMeta::created(0).display_name("zebra"));
    write_fake_metadata(rig.root(), "mmm", FakeMeta::created(0));
    let sessions = rig.list_json();
    assert_eq!(sort_keys(&sessions), vec!["mmm", "zebra"]);
}
