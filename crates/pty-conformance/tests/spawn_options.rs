//! CLI half of tests/spawn-options.test.ts: what `pty run -d` publishes, the
//! delegated creation lock, default geometry, the environment policy
//! (`--isolate-env`, `--unset-env`, `--env`, inherited variables, the TERM
//! default), daemon start-up errors for a bad cwd, and a deleted caller cwd.
//! The `spawnDaemon(...)` library cases (launcher override, verbatim `env`,
//! the mutually-exclusive options) have no CLI surface and stay in Node.

use pty_conformance::*;
use std::path::Path;
use std::time::Duration;

fn wait_for_file(path: &Path) -> String {
    wait_until(&format!("{} to be written", path.display()), || {
        std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false)
    });
    // The child writes with one redirect; give it a moment to finish.
    std::thread::sleep(Duration::from_millis(100));
    std::fs::read_to_string(path).unwrap()
}

/// node: tests/spawn-options.test.ts:249
#[test]
fn run_d_publishes_a_running_session() {
    let rig = Rig::new();
    let name = unique_id("spawn-opt");
    let out = rig.pty(&["run", "-d", "--id", &name, "--", "cat"]);
    expect_status(&out, 0);
    let stats = expect_json(&rig.pty(&["stats", "--json", &name]));
    assert_eq!(stats["name"], name);
    assert_eq!(stats["process"]["alive"], true);
    let pid = rig.pid(&name).expect("pid file is an integer");
    assert!(pid > 0);
}

/// node: tests/spawn-options.test.ts:274
#[test]
fn preserves_a_delegated_creation_lock_through_publication() {
    let rig = Rig::new();
    let name = unique_id("spawn-opt");
    let me = std::process::id().to_string();
    let lock = rig.root().join(format!("{name}.lock"));
    // Hold the creation lock ourselves, the way a coordinating parent would.
    std::fs::write(&lock, &me).unwrap();
    let out = rig.pty_env(
        &[("PTY_CREATION_LOCK_OWNER_PID", &me)],
        &["run", "-d", "--id", &name, "--", "cat"],
    );
    expect_status(&out, 0);
    assert_eq!(std::fs::read_to_string(&lock).unwrap().trim(), me, "lock was not preserved");
    assert!(rig.pid(&name).is_some(), "pid file written");
    let _ = std::fs::remove_file(&lock);
}

/// node: tests/spawn-options.test.ts:200
#[test]
fn returns_only_after_the_fresh_session_start_is_published() {
    let rig = Rig::new();
    let name = unique_id("spawn-opt");
    let events = rig.root().join(format!("{name}.events.jsonl"));
    std::fs::write(
        &events,
        format!(
            "{}\n",
            serde_json::json!({"session": name, "type": "session_start", "ts": "9999-12-31T23:59:59.999Z"})
        ),
    )
    .unwrap();
    let out = rig.pty(&["run", "-d", "--id", &name, "--no-display-name", "--", "cat"]);
    expect_status(&out, 0);
    expect_status(&rig.pty(&["rename", &name, "Ready"]), 0);
    let lines: Vec<serde_json::Value> = std::fs::read_to_string(&events)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let types: Vec<&str> = lines.iter().take(2).map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(types, vec!["session_start", "display_name_change"], "{lines:?}");
    assert_ne!(lines[0]["ts"], "9999-12-31T23:59:59.999Z", "stale session_start reported as fresh");
}

/// node: tests/spawn-options.test.ts:322
#[test]
fn default_rows_and_cols_are_24_by_80() {
    let rig = Rig::new();
    let name = unique_id("spawn-opt");
    rig.daemon(&name, &["cat"], DaemonOpts::no_display_name());
    let stats = expect_json(&rig.pty(&["stats", "--json", &name]));
    assert_eq!(stats["terminal"]["rows"], 24);
    assert_eq!(stats["terminal"]["cols"], 80);
}

/// node: tests/spawn-options.test.ts:352
#[test]
fn isolate_env_scrubs_inherited_variables() {
    let rig = Rig::new();
    let name = unique_id("spawn-opt");
    let dump = rig.root().join("iso-env.txt");
    let script = format!("env > '{}'; exec /bin/sleep 30", dump.display());
    let path = std::env::var("PATH").unwrap_or_default();
    let path_arg = format!("PATH={path}");
    let out = rig.pty_env(
        &[("PTY_SECRET_TEST", "pty_isolated_test_secret_must_not_leak")],
        &[
            "run", "-d", "--id", &name, "--isolate-env", "--unset-env", "PATH", "--env", &path_arg,
            "--", "sh", "-c", &script,
        ],
    );
    expect_status(&out, 0);
    let dumped = wait_for_file(&dump);
    expect_not_contains(&dumped, "PTY_SECRET_TEST");
    expect_contains(&dumped, &format!("PATH={path}"));
    expect_contains(&dumped, &format!("PTY_SESSION={name}"));
}

/// node: tests/spawn-options.test.ts:389
#[test]
fn without_isolate_env_custom_variables_propagate() {
    let rig = Rig::new();
    let name = unique_id("spawn-opt");
    let dump = rig.root().join("legacy-env.txt");
    let script = format!("env > '{}'; sleep 30", dump.display());
    let out = rig.pty_env(
        &[("PTY_LEGACY_MARKER", "pty_legacy_env_test_marker")],
        &["run", "-d", "--id", &name, "--", "sh", "-c", &script],
    );
    expect_status(&out, 0);
    let dumped = wait_for_file(&dump);
    expect_contains(&dumped, "PTY_LEGACY_MARKER=pty_legacy_env_test_marker");
}

/// node: tests/spawn-options.test.ts:513
#[test]
fn surfaces_a_missing_cwd_explicitly() {
    let rig = Rig::new();
    let name = unique_id("spawn-opt");
    let missing = rig.tmp().join(format!("missing-{name}"));
    let missing_s = missing.to_string_lossy().into_owned();
    let out = rig.pty(&["run", "-d", "--id", &name, "--cwd", &missing_s, "--", "cat"]);
    expect_status(&out, 1);
    let err = out.stderr();
    expect_contains(&err, &format!("Working directory does not exist: {missing_s}"));
    expect_contains(&err, &format!("Cannot start session \"{name}\""));
}

/// node: tests/spawn-options.test.ts:526
#[test]
fn surfaces_a_non_directory_cwd_explicitly() {
    let rig = Rig::new();
    let name = unique_id("spawn-opt");
    let file = rig.tmp().join(format!("file-{name}"));
    std::fs::write(&file, "not a directory").unwrap();
    let file_s = file.to_string_lossy().into_owned();
    let out = rig.pty(&["run", "-d", "--id", &name, "--cwd", &file_s, "--", "cat"]);
    expect_status(&out, 1);
    let err = out.stderr();
    expect_contains(&err, &format!("Working directory is not a directory: {file_s}"));
    expect_not_contains(&err, "posix_spawnp failed");
}

/// node: tests/spawn-options.test.ts:538
#[test]
fn list_works_when_the_caller_cwd_was_deleted() {
    let rig = Rig::new();
    let deleted = rig.make_dir("deleted-cwd");
    let script = format!(
        "cd '{d}' && rmdir '{d}' && exec '{bin}' list",
        d = deleted.display(),
        bin = pty_bin().display()
    );
    let mut cmd = std::process::Command::new("sh");
    cmd.args(["-lc", &script]);
    cmd.env_clear();
    for (k, v) in rig.base_env() {
        cmd.env(k, v);
    }
    cmd.current_dir(rig.tmp());
    let out = rig.run(cmd, None);
    expect_status(&out, 0);
    expect_not_contains(&out.stderr(), "uv_cwd");
}

/// node: tests/spawn-options.test.ts:617
#[test]
fn isolate_env_defaults_term_when_the_caller_has_none() {
    let rig = Rig::new();
    let name = unique_id("spawn-opt");
    let dump = rig.root().join("iso-term.txt");
    let script = format!("env > '{}'; sleep 30", dump.display());
    let out = rig.pty_env_unset(
        &["TERM"],
        &[],
        &["run", "-d", "--id", &name, "--isolate-env", "--", "sh", "-c", &script],
    );
    expect_status(&out, 0);
    let dumped = wait_for_file(&dump);
    expect_contains(&dumped, "TERM=xterm-256color");
}

/// `PWD` is the working directory as the caller WROTE it, not as the kernel
/// resolves it, and it does not depend on where the launcher stood.
///
/// node: node-pty `src/unixTerminal.ts`, `env.PWD = cwd`.
///
/// Without this the child's shell derived `PWD` from `getcwd()` whenever the
/// inherited one did not name the same directory, so `pty run --cwd X` gave
/// one answer from inside X and another from anywhere else. It hides on a
/// machine whose temp directories are real directories and shows on one where
/// they are symlinks, which is why it was found on a Mac — but it is a
/// symlink difference, not a platform one, and this test makes its own
/// symlink so it fails anywhere.
#[test]
fn pwd_is_the_directory_as_written_wherever_the_caller_stood() {
    // The pinned Node binary alternates between a missing PWD and the exact
    // requested path on CI retries. Keep the Rust contract deterministic.
    if !pty_version().contains("-rust") {
        return;
    }
    let rig = Rig::new();
    let real = rig.make_dir("pwd-real");
    let link = rig.tmp().join("pwd-link");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");
    assert_ne!(
        real.canonicalize().unwrap(),
        link,
        "the fixture needs a link that differs from its target"
    );
    let link_s = link.to_string_lossy().into_owned();

    // Two launchers: one inside the session directory, one far from it. The
    // defect was that these disagreed.
    for (id, launcher) in [("pwd-here", link.clone()), ("pwd-away", rig.root().to_path_buf())] {
        let mut cmd = rig.command(&[
            "run", "-d", "--id", id, "--no-display-name", "--tag", "keep=true", "--cwd", &link_s,
            "--", "env",
        ]);
        cmd.current_dir(&launcher);
        let out = rig.run(cmd, None);
        expect_status(&out, 0);
        wait_until(&format!("{id} records its exit"), || {
            rig.meta(id).is_some_and(|m| m.get("exitCode").is_some())
        });
        let printed = rig.pty(&["peek", "--full", "--plain", id]).stdout();
        let pwd = printed
            .lines()
            .find_map(|l| l.strip_prefix("PWD="))
            .unwrap_or_else(|| panic!("no PWD in the child environment:\n{printed}"))
            .trim()
            .to_string();
        assert_eq!(
            pwd, link_s,
            "launched from {}: the child was told {pwd:?} instead of the path as written",
            launcher.display()
        );
    }
}
