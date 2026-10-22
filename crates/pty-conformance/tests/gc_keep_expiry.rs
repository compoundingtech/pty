//! Port of tests/gc-keep-expiry.test.ts: `keep=true` buys a DEAD session a
//! bounded retention window against `pty gc`, not immortality. Agents tag a
//! session they are debugging right now and never come back to untag it, so
//! an unbounded exemption turns the registry into an append-only log.
//!
//! The policy pinned here: exempt while the session has been dead for less
//! than `--keep-max-age` (default 7d), swept and reported separately once
//! past it, `0` sweeps the whole backlog, and a RUNNING keep session is never
//! a candidate no matter what the flag says.
//!
//! Records are written straight into the root rather than spawned: the policy
//! is a comparison against `exitedAt`/`createdAt`, so a fabricated record is
//! both exact about age and free of daemon-startup waits.

use pty_conformance::*;

const DAY: i64 = 24 * 60 * 60;

/// Fake policy records still need process ownership evidence. A known absent
/// PID plus a retained token models an exited daemon; no evidence is Unknown.
fn bind_fixture_identity(rig: &Rig, name: &str, pid: i32) {
    let token = pty_core::registry::read_process_start_token(pid).unwrap_or_else(|| {
        assert!(!pid_alive(pid), "a live fixture needs a readable identity");
        "fixture:dead".to_string()
    });
    let mut metadata = rig.meta(name).expect("fixture metadata");
    metadata["daemonPid"] = serde_json::json!(pid);
    metadata["daemonStartToken"] = serde_json::json!(token);
    metadata["daemonBootId"] = serde_json::json!(pty_core::registry::read_boot_id().unwrap());
    std::fs::write(rig.meta_path(name), metadata.to_string()).unwrap();
}
/// An exited session, dead for `dead_for_secs`, tagged `keep=true`.
fn write_exited_keep(rig: &Rig, name: &str, dead_for_secs: i64) {
    write_fake_metadata(
        rig.root(),
        name,
        FakeMeta::created(-dead_for_secs)
            .exited(-dead_for_secs, 0)
            .tag("keep", "true"),
    );
    bind_fixture_identity(rig, name, i32::MAX);
}

/// node: tests/gc-keep-expiry.test.ts:101
#[test]
fn keeps_an_exited_keep_session_younger_than_the_default_window() {
    let rig = Rig::new();
    let name = unique_id("gke");
    write_exited_keep(&rig, &name, 2 * DAY);

    let out = rig.pty(&["gc"]);
    expect_status(&out, 0);
    let stdout = out.stdout();
    expect_contains(&stdout, &format!("Kept (keep tag): {name}"));
    expect_not_contains(&stdout, "keep expired");
    assert!(rig.meta_path(&name).exists());
}

/// node: tests/gc-keep-expiry.test.ts:113
#[test]
fn sweeps_an_expired_keep_session_apart_from_the_plain_sweep() {
    let rig = Rig::new();
    let expired = unique_id("gke");
    let stale = unique_id("gke");
    write_exited_keep(&rig, &expired, 30 * DAY);
    // A same-age session WITHOUT the tag: proves the two buckets stay
    // distinct rather than one absorbing the other.
    write_fake_metadata(
        rig.root(),
        &stale,
        FakeMeta::created(-30 * DAY).exited(-30 * DAY, 0),
    );
    bind_fixture_identity(&rig, &stale, i32::MAX);

    let out = rig.pty(&["gc"]);
    expect_status(&out, 0);
    let stdout = out.stdout();
    expect_contains(
        &stdout,
        &format!("Removed (keep expired after 7d): {expired}"),
    );
    expect_contains(&stdout, &format!("Removed: {stale}"));
    expect_contains(&stdout, "1 stale session");
    expect_contains(&stdout, "1 keep-expired session");
    assert!(!rig.meta_path(&expired).exists());
    assert!(!rig.meta_path(&stale).exists());
}

/// node: tests/gc-keep-expiry.test.ts:132
#[test]
fn honours_a_custom_window_in_both_flag_spellings() {
    let rig = Rig::new();
    let spaced = unique_id("gke");
    let equals = unique_id("gke");
    write_exited_keep(&rig, &spaced, 2 * 3600);
    write_exited_keep(&rig, &equals, 2 * 3600);

    // 3h window: both sessions are 2h dead, so both survive.
    let kept = rig.pty(&["gc", "--keep-max-age", "3h"]);
    expect_status(&kept, 0);
    let stdout = kept.stdout();
    expect_contains(&stdout, &format!("Kept (keep tag): {spaced}"));
    expect_contains(&stdout, &format!("Kept (keep tag): {equals}"));
    assert!(rig.meta_path(&spaced).exists());

    // 1h window: both are past it.
    let swept = rig.pty(&["gc", "--keep-max-age=1h"]);
    expect_status(&swept, 0);
    let stdout = swept.stdout();
    expect_contains(
        &stdout,
        &format!("Removed (keep expired after 1h): {spaced}"),
    );
    expect_contains(
        &stdout,
        &format!("Removed (keep expired after 1h): {equals}"),
    );
    assert!(!rig.meta_path(&spaced).exists());
    assert!(!rig.meta_path(&equals).exists());
}

/// node: tests/gc-keep-expiry.test.ts:155
#[test]
fn a_zero_window_sweeps_a_keep_session_that_just_exited() {
    let rig = Rig::new();
    let name = unique_id("gke");
    write_exited_keep(&rig, &name, 0);

    let out = rig.pty(&["gc", "--keep-max-age", "0"]);
    expect_status(&out, 0);
    expect_contains(
        &out.stdout(),
        &format!("Removed (keep expired after 0s): {name}"),
    );
    assert!(!rig.meta_path(&name).exists());
}

/// node: tests/gc-keep-expiry.test.ts:166
#[test]
fn anchors_on_created_at_when_there_is_no_exit_record() {
    let rig = Rig::new();
    let name = unique_id("gke");
    // A vanished session (SIGKILLed daemon) never wrote `exitedAt`, so its
    // age comes from `createdAt` — the same anchor precedence `pty list
    // --older-than` uses.
    write_fake_metadata(
        rig.root(),
        &name,
        FakeMeta::created(-30 * DAY).tag("keep", "true"),
    );
    bind_fixture_identity(&rig, &name, i32::MAX);

    let out = rig.pty(&["gc"]);
    expect_status(&out, 0);
    expect_contains(
        &out.stdout(),
        &format!("Removed (keep expired after 7d): {name}"),
    );
    assert!(!rig.meta_path(&name).exists());
}

/// node: tests/gc-keep-expiry.test.ts:188
#[test]
fn never_sweeps_a_running_keep_session_even_at_zero() {
    let rig = Rig::new();
    let name = unique_id("gke");
    // The test process stands in for a live daemon with its exact recorded
    // start token. Age alone must not make a proven live session a candidate.
    std::fs::write(rig.pid_path(&name), std::process::id().to_string()).unwrap();
    write_fake_metadata(
        rig.root(),
        &name,
        FakeMeta::created(-30 * DAY).tag("keep", "true"),
    );
    bind_fixture_identity(&rig, &name, std::process::id() as i32);
    assert_eq!(rig.list_entry(&name).expect("listed")["status"], "running");

    let out = rig.pty(&["gc", "--keep-max-age", "0"]);
    expect_status(&out, 0);
    expect_not_contains(&out.stdout(), &name);
    assert!(rig.meta_path(&name).exists());
    assert_eq!(rig.list_entry(&name).expect("listed")["status"], "running");
}

/// node: tests/gc-keep-expiry.test.ts:214
#[test]
fn dry_run_previews_keep_expiry_without_removing_anything() {
    let rig = Rig::new();
    let name = unique_id("gke");
    write_exited_keep(&rig, &name, 30 * DAY);

    let dry = rig.pty(&["gc", "--dry-run"]);
    expect_status(&dry, 0);
    let stdout = dry.stdout();
    expect_contains(
        &stdout,
        &format!("Would remove (keep expired after 7d): {name}"),
    );
    expect_contains(&stdout, "1 keep-expired session");
    expect_contains(&stdout, "Dry run");
    assert!(rig.meta_path(&name).exists());

    // A zero-window dry run is equally non-mutating.
    let dry_zero = rig.pty(&["gc", "-n", "--keep-max-age", "0"]);
    expect_status(&dry_zero, 0);
    expect_contains(
        &dry_zero.stdout(),
        &format!("Would remove (keep expired after 0s): {name}"),
    );
    assert!(rig.meta_path(&name).exists());

    // And the real pass then actually removes it.
    let real = rig.pty(&["gc"]);
    expect_status(&real, 0);
    expect_contains(
        &real.stdout(),
        &format!("Removed (keep expired after 7d): {name}"),
    );
    assert!(!rig.meta_path(&name).exists());
}

/// node: tests/gc-keep-expiry.test.ts:239
#[test]
fn rejects_a_unit_less_non_zero_window() {
    let rig = Rig::new();
    let bare = rig.pty(&["gc", "--keep-max-age", "7"]);
    expect_failure(&bare);
    expect_contains(
        &bare.stderr(),
        "--keep-max-age expects a duration like 12h, 7d, or 0",
    );

    let junk = rig.pty(&["gc", "--keep-max-age=soon"]);
    expect_failure(&junk);
    expect_contains(
        &junk.stderr(),
        "--keep-max-age expects a duration like 12h, 7d, or 0",
    );
}
