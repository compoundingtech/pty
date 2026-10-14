//! Port of tests/restart-launch-parity.test.ts: every restart-relevant
//! `pty run` setting (`--env`, `--unset-env`, `--isolate-env`, `-e`, `--cwd`,
//! tags, display name) is persisted and replayed by `run -a` and `restart`,
//! and a `pty.toml` session keeps its launch fields across a restart.
//! Every CLI call runs with `PTY_SESSION=outer-test-session` so `restart`
//! reports instead of attaching.

use pty_conformance::*;
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

const OUTER: (&str, &str) = ("PTY_SESSION", "outer-test-session");

fn cli(rig: &Rig, args: &[&str], extra: &[(&str, &str)]) -> Out {
    let mut env = vec![OUTER];
    env.extend_from_slice(extra);
    rig.pty_env(&env, args)
}

fn wait_for_content(file: &Path) -> String {
    let _ = poll_for(Duration::from_secs(5), || {
        std::fs::read_to_string(file).map(|s| !s.is_empty()).unwrap_or(false)
    });
    std::fs::read_to_string(file).unwrap_or_default()
}

fn restart_shape(m: &Value) -> Value {
    json!({
        "command": m["command"],
        "args": m["args"],
        "displayCommand": m["displayCommand"],
        "cwd": m["cwd"],
        "rows": m["rows"],
        "cols": m["cols"],
        "ephemeral": m["ephemeral"],
        "tags": m["tags"],
        "displayName": m["displayName"],
        "isolateEnv": m["isolateEnv"],
        "extraEnv": m["extraEnv"],
        "unsetEnv": m["unsetEnv"],
    })
}

/// node: tests/restart-launch-parity.test.ts:73
#[test]
fn run_a_preserves_env_removals_when_recreating_an_exited_session() {
    let rig = Rig::new();
    let output = rig.tmp().join("run-a-unset-child.txt");
    let name = "run-a-unset";
    let created = cli(
        &rig,
        &[
            "run", "-d", "--id", name, "--no-display-name", "--tag", "keep=true", "--unset-env",
            "NO_COLOR", "--", "true",
        ],
        &[("NO_COLOR", "ambient-create")],
    );
    expect_status(&created, 0);
    wait_until_for("exit metadata", Duration::from_secs(5), &mut || {
        rig.meta(name).map(|m| m["exitedAt"].is_string()).unwrap_or(false)
    });

    let script = format!(
        "if [ \"${{NO_COLOR+x}}\" = x ]; then printf set; else printf unset; fi > '{}'; exec sleep 300",
        output.display()
    );
    let recreated = cli(
        &rig,
        &["run", "-a", "-d", "--id", name, "--no-display-name", "--", "sh", "-c", &script],
        &[("NO_COLOR", "ambient-recreate")],
    );
    expect_status(&recreated, 0);
    assert_eq!(wait_for_content(&output), "unset");
    assert_eq!(rig.meta(name).unwrap()["unsetEnv"], json!(["NO_COLOR"]));
}

/// node: tests/restart-launch-parity.test.ts:106
#[test]
fn restart_preserves_inherited_env_removals() {
    let rig = Rig::new();
    let output = rig.tmp().join("unset-child.txt");
    let name = "unset-parity";
    let recorder = format!(
        "printf '%s|%s' \"${{NO_COLOR+x}}\" \"$ASSIGNMENT_WINS\" > '{}'; exec sleep 300",
        output.display()
    );
    let created = cli(
        &rig,
        &[
            "run", "-d", "--id", name, "--no-display-name", "--tag", "keep=true", "--unset-env",
            "NO_COLOR", "--env", "ASSIGNMENT_WINS=explicit", "--unset-env", "ASSIGNMENT_WINS", "--",
            "sh", "-c", &recorder,
        ],
        &[("NO_COLOR", "1"), ("ASSIGNMENT_WINS", "ambient-create")],
    );
    expect_status(&created, 0);
    assert_eq!(wait_for_content(&output), "|explicit");

    let before = rig.meta(name).unwrap();
    assert_eq!(before["unsetEnv"], json!(["NO_COLOR", "ASSIGNMENT_WINS"]));
    assert_eq!(before["extraEnv"], json!({"ASSIGNMENT_WINS": "explicit"}));

    let _ = std::fs::remove_file(&output);
    let restarted = cli(
        &rig,
        &["restart", "-y", name],
        &[("NO_COLOR", "1"), ("ASSIGNMENT_WINS", "ambient-restarter")],
    );
    expect_status(&restarted, 0);
    assert_eq!(wait_for_content(&output), "|explicit");
    let after = rig.meta(name).unwrap();
    assert_eq!(restart_shape(&after), restart_shape(&before));
}

/// node: tests/restart-launch-parity.test.ts:141
#[test]
fn persists_repeatable_env_and_every_restart_relevant_setting() {
    let rig = Rig::new();
    let cwd = rig.make_dir("run-cwd");
    let cwd_s = cwd.to_string_lossy().into_owned();
    let output = rig.tmp().join("run-child.txt");
    let name = "launch-parity";
    let recorder = format!(
        "printf '%s|%s|%s|%s' \"$ST_AGENT\" \"$CATALOG\" \"$PTY_SESSION\" \"$PWD\" > '{}'; exec sleep 300",
        output.display()
    );
    let created = cli(
        &rig,
        &[
            "run", "-d", "-e", "--id", name, "--name", "Launch Parity", "--tag", "keep=true", "--tag",
            "role=service", "--cwd", &cwd_s, "--isolate-env", "--env", "ST_AGENT=managed-first",
            "--env", "CATALOG=/managed/catalog", "--env", "PTY_SESSION=must-not-win", "--env",
            "ST_AGENT=managed-final", "--", "sh", "-c", &recorder,
        ],
        &[("ST_AGENT", "ambient-create"), ("CATALOG", "/ambient/create")],
    );
    expect_status(&created, 0);
    assert_eq!(
        wait_for_content(&output),
        format!("managed-final|/managed/catalog|{name}|{cwd_s}")
    );

    let before = rig.meta(name).unwrap();
    assert_eq!(before["ephemeral"], true, "{before}");
    assert_eq!(before["isolateEnv"], true, "{before}");
    assert_eq!(
        before["extraEnv"],
        json!({"ST_AGENT": "managed-final", "CATALOG": "/managed/catalog", "PTY_SESSION": "must-not-win"})
    );
    assert_eq!(before["tags"]["keep"], "true");
    assert_eq!(before["tags"]["role"], "service");
    assert_eq!(before["displayName"], "Launch Parity");
    assert_eq!(before["cwd"], cwd_s);
    assert!(before["rows"].as_i64().unwrap() > 0, "{before}");
    assert!(before["cols"].as_i64().unwrap() > 0, "{before}");

    let _ = std::fs::remove_file(&output);
    let restarted = cli(
        &rig,
        &["restart", "-y", name],
        &[("ST_AGENT", "ambient-restarter"), ("CATALOG", "/ambient/restarter")],
    );
    expect_status(&restarted, 0);
    expect_contains(&restarted.stdout(), "restarted");
    assert_eq!(
        wait_for_content(&output),
        format!("managed-final|/managed/catalog|{name}|{cwd_s}")
    );
    let after = rig.meta(name).unwrap();
    assert_eq!(restart_shape(&after), restart_shape(&before));
}

/// node: tests/restart-launch-parity.test.ts:190
#[test]
fn keeps_every_ptyfile_launch_field_across_restart() {
    let rig = Rig::new();
    let project = rig.make_dir("toml-project");
    let cwd = project.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd_s = cwd.to_string_lossy().into_owned();
    let output = rig.tmp().join("toml-child.txt");
    let name = "toml-parity";
    std::fs::write(
        project.join("pty.toml"),
        format!(
            r#"
prefix = "ignored-by-override"

[sessions.worker]
id = "{name}"
display_name = "TOML Worker"
command = """printf '%s|%s|%s' "$TASK_VALUE" "$PTY_SESSION" "$PWD" > "{out}"; exec sleep 300"""
cwd = "work"

[sessions.worker.tags]
keep = "true"
role = "worker"

[sessions.worker.env]
TASK_VALUE = "from-toml"
PTY_SESSION = "must-not-win"
"#,
            out = output.display()
        ),
    )
    .unwrap();

    let project_s = project.to_string_lossy().into_owned();
    let created = cli(&rig, &["up", &project_s], &[("TASK_VALUE", "ambient-create")]);
    expect_status(&created, 0);
    assert_eq!(wait_for_content(&output), format!("from-toml|{name}|{cwd_s}"));

    let before = rig.meta(name).unwrap();
    assert_eq!(before["displayName"], "TOML Worker");
    assert!(before["displayCommand"].as_str().unwrap().contains("TASK_VALUE"), "{before}");
    assert_eq!(before["cwd"], cwd_s);
    assert_eq!(before["tags"]["keep"], "true");
    assert_eq!(before["tags"]["role"], "worker");
    assert_eq!(before["tags"]["ptyfile.session"], "worker");
    assert_eq!(
        before["extraEnv"],
        json!({"TASK_VALUE": "from-toml", "PTY_SESSION": "must-not-win"})
    );

    let _ = std::fs::remove_file(&output);
    let restarted = cli(&rig, &["restart", "-y", name], &[("TASK_VALUE", "ambient-restarter")]);
    expect_status(&restarted, 0);
    assert_eq!(wait_for_content(&output), format!("from-toml|{name}|{cwd_s}"));
    let after = rig.meta(name).unwrap();
    assert_eq!(restart_shape(&after), restart_shape(&before));
}
