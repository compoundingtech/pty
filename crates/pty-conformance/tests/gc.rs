//! Port of tests/gc.test.ts (the sweep, orphan layout-tag pruning, dry run,
//! and `--print-launchd-plist`) plus the plist half of tests/pty-root.test.ts
//! (per-root Label and log path). Node starts daemons with
//! `node dist/server.js`; here they come from `pty run -d`. The respawn /
//! flapping / abandoned gc steps are dropped (docs/parity.md §12) and have
//! no tests here.

use pty_conformance::*;
use std::time::Duration;

fn start(rig: &Rig, id: &str, tags: &[(&str, &str)]) -> Daemon {
    let mut opts = DaemonOpts::no_display_name();
    for (k, v) in tags {
        opts = opts.tag(k, v);
    }
    rig.daemon(id, &["cat"], opts)
}

/// SIGKILL a daemon so no shutdown code runs and its `<id>.json` is left
/// behind without an exit record (status vanished).
fn kill_daemon_hard(pid: i32) {
    kill_pid(pid, libc::SIGKILL);
    assert!(poll_for(Duration::from_secs(5), || !pid_alive(pid)), "daemon {pid} did not die");
}

/// An unused pid so ESRCH is deterministic (probing downward from 999999).
fn find_dead_pid() -> i32 {
    let mut p = 999_999;
    while p > 900_000 {
        if !pid_alive(p) {
            return p;
        }
        p -= 7;
    }
    panic!("no unused pid found");
}

fn tags(rig: &Rig, id: &str) -> serde_json::Value {
    rig.meta(id).expect("metadata").get("tags").cloned().unwrap_or(serde_json::Value::Null)
}

#[test]
fn removes_only_dead_writer_atomic_temporaries() {
    let rig = Rig::new();
    let dead_pid = find_dead_pid();
    let live_pid = std::process::id();
    let dead = format!("record.json.tmp.{dead_pid}.0123456789abcdef");
    let live = format!("record.json.tmp.{live_pid}.0123456789abcdef");
    let preserved = [
        live,
        "record.json.tmp.0.0123456789abcdef".into(),
        "record.json.tmp.-1.0123456789abcdef".into(),
        "record.json.tmp.+1.0123456789abcdef".into(),
        "record.json.tmp.0001.0123456789abcdef".into(),
        "record.json.tmp.2147483648.0123456789abcdef".into(),
        format!("record.json.tmp.{dead_pid}.0123456789abcde"),
        format!("record.json.tmp.{dead_pid}.0123456789abcdef0"),
        format!("record.json.tmp.{dead_pid}.0123456789abcdeg"),
        format!("record.json.tmp.{dead_pid}.0123456789ABCDEF"),
        format!("record.json.tmp.{dead_pid}.0123456789abcdef.extra"),
        "record.tmp.malformed.json".into(),
        "unrelated-file".into(),
    ];
    let directories = [
        rig.root().to_path_buf(),
        rig.root().join(".activity"),
        rig.root().join(".recovery"),
    ];
    for directory in &directories {
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(&dead), b"incomplete").unwrap();
        for filename in &preserved {
            std::fs::write(directory.join(filename), b"preserve").unwrap();
        }
    }
    let directory_temp = rig.root().join(format!("directory.tmp.{dead_pid}.0123456789abcdef"));
    std::fs::create_dir(&directory_temp).unwrap();
    let symlink_temp = rig.root().join(format!("symlink.tmp.{dead_pid}.0123456789abcdef"));
    std::os::unix::fs::symlink(rig.root().join("unrelated-file"), &symlink_temp).unwrap();

    expect_status(&rig.pty(&["gc", "--dry-run"]), 0);
    for directory in &directories {
        assert!(directory.join(&dead).exists(), "dry run removed a temporary");
    }
    expect_status(&rig.pty(&["gc"]), 0);
    for directory in &directories {
        assert!(!directory.join(&dead).exists(), "dead writer temporary survived");
        for filename in &preserved {
            assert_eq!(std::fs::read(directory.join(filename)).unwrap(), b"preserve");
        }
    }
    assert!(directory_temp.is_dir());
    assert!(symlink_temp.is_symlink());
}

/// node: tests/gc.test.ts:121
#[test]
fn removes_vanished_sessions() {
    let rig = Rig::new();
    let d = start(&rig, "gc1", &[]);
    kill_daemon_hard(d.pid());
    assert!(d.meta_path().exists());
    let out = rig.pty(&["gc"]);
    expect_status(&out, 0);
    expect_contains(&out.stdout(), "Removed: gc1");
    assert!(!d.meta_path().exists());
}

/// node: tests/gc.test.ts:137
#[test]
fn prunes_layout_tags_whose_pid_is_dead() {
    let rig = Rig::new();
    let key = format!(":l{}-abc", find_dead_pid());
    start(&rig, "gc2", &[("role", "web"), (&key, "1")]);
    assert_eq!(tags(&rig, "gc2")[&key], "1");
    let out = rig.pty(&["gc"]);
    expect_status(&out, 0);
    expect_contains(&out.stdout(), &format!("Pruned orphan tags on gc2: #{key}"));
    let t = tags(&rig, "gc2");
    assert!(t.get(&key).is_none(), "{t}");
    assert_eq!(t["role"], "web");
}

/// node: tests/gc.test.ts:162
#[test]
fn keeps_layout_tags_whose_pid_is_alive() {
    let rig = Rig::new();
    let key = format!(":l{}-xyz", std::process::id());
    start(&rig, "gc3", &[(&key, "1")]);
    let out = rig.pty(&["gc"]);
    expect_status(&out, 0);
    expect_not_contains(&out.stdout(), &key);
    assert_eq!(tags(&rig, "gc3")[&key], "1");
}

/// node: tests/gc.test.ts:179
#[test]
fn does_not_prune_non_layout_colon_tags() {
    let rig = Rig::new();
    start(&rig, "gc4", &[(":layout", "grid"), (":other", "x")]);
    let out = rig.pty(&["gc"]);
    expect_status(&out, 0);
    let t = tags(&rig, "gc4");
    assert_eq!(t[":layout"], "grid");
    assert_eq!(t[":other"], "x");
}

/// node: tests/gc.test.ts:198
#[test]
fn reports_nothing_to_clean_up() {
    let rig = Rig::new();
    start(&rig, "gc5", &[]);
    let out = rig.pty(&["gc"]);
    expect_status(&out, 0);
    expect_contains(&out.stdout(), "Nothing to clean up.");
}

/// node: tests/gc.test.ts:208
#[test]
fn dry_run_previews_vanished_removal() {
    let rig = Rig::new();
    let d = start(&rig, "gc6", &[]);
    kill_daemon_hard(d.pid());
    assert!(d.meta_path().exists());
    let dry = rig.pty(&["gc", "--dry-run"]);
    expect_status(&dry, 0);
    expect_contains(&dry.stdout(), "Would remove: gc6");
    expect_contains(&dry.stdout(), "Dry run");
    assert!(d.meta_path().exists());
    let real = rig.pty(&["gc"]);
    expect_status(&real, 0);
    expect_contains(&real.stdout(), "Removed: gc6");
    assert!(!d.meta_path().exists());
}

/// node: tests/gc.test.ts:231
#[test]
fn dry_run_previews_orphan_tag_pruning() {
    let rig = Rig::new();
    let key = format!(":l{}-abc", find_dead_pid());
    start(&rig, "gc7", &[(&key, "1")]);
    assert_eq!(tags(&rig, "gc7")[&key], "1");
    let dry = rig.pty(&["gc", "--dry-run"]);
    expect_status(&dry, 0);
    expect_contains(&dry.stdout(), &format!("Would prune orphan tags on gc7: #{key}"));
    assert_eq!(tags(&rig, "gc7")[&key], "1");
    let real = rig.pty(&["gc"]);
    expect_status(&real, 0);
    expect_contains(&real.stdout(), &format!("Pruned orphan tags on gc7: #{key}"));
    assert!(tags(&rig, "gc7").get(&key).is_none());
}

/// node: tests/gc.test.ts:255
#[test]
fn n_is_an_alias_for_dry_run() {
    let rig = Rig::new();
    let d = start(&rig, "gc8", &[]);
    kill_daemon_hard(d.pid());
    let out = rig.pty(&["gc", "-n"]);
    expect_status(&out, 0);
    expect_contains(&out.stdout(), "Would remove: gc8");
    assert!(d.meta_path().exists());
}

/// node: tests/gc.test.ts:268
#[test]
fn reaps_synthetic_vanished_metadata() {
    let rig = Rig::new();
    write_fake_metadata(rig.root(), "gc9", FakeMeta::created(0));
    let out = rig.pty(&["gc"]);
    expect_status(&out, 0);
    expect_contains(&out.stdout(), "Removed: gc9");
    assert!(!rig.meta_path("gc9").exists());
}

/// node: tests/gc.test.ts:288
#[test]
fn print_launchd_plist_emits_a_plist() {
    let rig = Rig::new();
    let out = rig.pty(&["gc", "--print-launchd-plist"]);
    expect_status(&out, 0);
    let s = out.stdout();
    expect_contains(&s, "<!DOCTYPE plist");
    expect_regex(&s, r"<string>com\.compoundingtech\.pty\.gc(?:\.[A-Za-z0-9._-]+)?</string>");
    expect_contains(&s, "<key>StartInterval</key>");
    expect_contains(&s, "<integer>30</integer>");
    expect_contains(&s, "<key>PTY_ROOT</key>");
}

/// node: tests/gc.test.ts:303
#[test]
fn print_launchd_plist_preserves_the_invoked_binary() {
    let rig = Rig::new();
    let out = rig.pty(&["gc", "--print-launchd-plist"]);
    expect_status(&out, 0);
    expect_contains(&out.stdout(), &format!("<string>{}</string>", pty_bin().display()));
}

/// node: tests/gc.test.ts:314
#[test]
fn print_launchd_plist_interval_flag() {
    let rig = Rig::new();
    let out = rig.pty(&["gc", "--print-launchd-plist", "--interval=15"]);
    expect_status(&out, 0);
    expect_contains(&out.stdout(), "<integer>15</integer>");
}

/// node: tests/gc.test.ts:321
#[test]
fn rejects_zero_and_non_numeric_intervals() {
    let rig = Rig::new();
    expect_failure(&rig.pty(&["gc", "--print-launchd-plist", "--interval=0"]));
    expect_failure(&rig.pty(&["gc", "--print-launchd-plist", "--interval=abc"]));
}

// ── tests/pty-root.test.ts: per-root Label + logPath ──

fn plist_for(rig: &Rig, env: &[(&str, &str)]) -> String {
    let mut extra = vec![("PTY_ROOT_LEGACY_SILENT", "1")];
    extra.extend_from_slice(env);
    let out = rig.pty_clean(&extra, &["gc", "--print-launchd-plist"]);
    expect_status(&out, 0);
    out.stdout()
}

/// node: tests/pty-root.test.ts:158
#[test]
fn default_root_keeps_the_legacy_label() {
    let rig = Rig::new();
    let plist = plist_for(&rig, &[]);
    expect_contains(&plist, "<string>com.compoundingtech.pty.gc</string>");
    expect_not_contains(&plist, "<string>com.compoundingtech.pty.gc.");
}

/// node: tests/pty-root.test.ts:165
#[test]
fn non_default_root_gets_a_suffixed_label() {
    let rig = Rig::new();
    let dir = rig.make_dir("my-network");
    let plist = plist_for(&rig, &[("PTY_ROOT", dir.to_str().unwrap())]);
    expect_contains(&plist, "<string>com.compoundingtech.pty.gc.my-network</string>");
}

/// node: tests/pty-root.test.ts:172
#[test]
fn non_default_root_log_path_lives_inside_the_root() {
    let rig = Rig::new();
    let dir = rig.make_dir("another-net");
    let plist = plist_for(&rig, &[("PTY_ROOT", dir.to_str().unwrap())]);
    expect_contains(&plist, &format!("<string>{}/gc.log</string>", dir.display()));
}

/// node: tests/pty-root.test.ts:179
#[test]
fn emits_pty_root_in_environment_variables() {
    let rig = Rig::new();
    let dir = rig.make_dir("envcheck");
    let plist = plist_for(&rig, &[("PTY_ROOT", dir.to_str().unwrap())]);
    expect_contains(&plist, "<key>PTY_ROOT</key>");
    expect_not_contains(&plist, "<key>PTY_SESSION_DIR</key>");
}

/// node: tests/pty-root.test.ts:188
#[test]
fn sanitizes_a_pathological_basename_into_the_label() {
    let rig = Rig::new();
    let dir = rig.make_dir("weird name with spaces");
    let plist = plist_for(&rig, &[("PTY_ROOT", dir.to_str().unwrap())]);
    expect_contains(&plist, "<string>com.compoundingtech.pty.gc.weird-name-with-spaces</string>");
}
