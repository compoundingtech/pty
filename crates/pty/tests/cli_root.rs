//! `--root`, the `PTY_SESSION_DIR` notices, and the root-length backstop.
//!
//! node: tests/pty-root.test.ts, tests/gc-flap-clear-badge-root-len.test.ts

mod cli_common;

use std::process::Command;

use cli_common::{Rig, pty_bin};

/// A command with an environment built from scratch (PATH + HOME only).
fn scrubbed(args: &[&str], env: &[(&str, &str)]) -> cli_common::Out {
    let mut c = Command::new(pty_bin());
    c.args(args)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", std::env::var("HOME").unwrap_or_default());
    for (k, v) in env {
        c.env(k, v);
    }
    let out = c.output().unwrap();
    cli_common::Out {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code().unwrap_or(-1),
    }
}

#[test]
fn root_report_sources_share_resolution_and_keep_native_default_independent() {
    let rig = Rig::new();
    let home = rig.scratch.join("empty-home");
    std::fs::create_dir(&home).unwrap();
    let home = home.to_str().unwrap();
    let baseline = scrubbed(&["root", "--json"], &[("HOME", home)]);
    assert_eq!(baseline.code, 0, "{}", baseline.stderr);
    let baseline = baseline.json();
    assert_eq!(baseline["effective"]["source"], "default");
    assert_eq!(baseline["effective"]["path"], baseline["nativeDefault"]["path"]);
    let native = baseline["nativeDefault"].clone();
    assert!(native["path"].as_str().unwrap().starts_with(&format!("{home}/.local/state/pty/h-")));
    let env_root = rig.scratch.join("env-root");
    let legacy = rig.scratch.join("legacy-root");
    let flag = rig.scratch.join("flag-root/../alias");
    let env_root = env_root.to_str().unwrap();
    let legacy = legacy.to_str().unwrap();
    let flag = flag.to_str().unwrap();
    for (args, env, source, path) in [
        (vec!["root", "--json"], vec![("PTY_ROOT", env_root)], "PTY_ROOT", env_root),
        (vec!["root", "--json"], vec![("PTY_SESSION_DIR", legacy)], "PTY_SESSION_DIR", legacy),
        (vec!["root", "--json"], vec![("PTY_ROOT", env_root), ("PTY_SESSION_DIR", legacy)], "PTY_ROOT", env_root),
        (vec!["--root", flag, "root", "--json"], vec![("PTY_ROOT", env_root)], "flag", flag),
        (vec!["root", "--json", "--root", flag], vec![("PTY_SESSION_DIR", legacy)], "flag", flag),
    ] {
        let mut env = env;
        env.push(("HOME", home));
        let out = scrubbed(&args, &env);
        assert_eq!(out.code, 0, "{}", out.stderr);
        let report = out.json();
        assert_eq!(report["effective"], serde_json::json!({"path": path, "source": source}));
        assert_eq!(report["nativeDefault"], native);
    }
    let empty = scrubbed(&["root", "--json"], &[("HOME", home), ("PTY_ROOT", ""), ("PTY_SESSION_DIR", "")]);
    assert_eq!(empty.json(), baseline);
    assert!(std::fs::read_dir(home).unwrap().next().is_none());
    assert!(!rig.scratch.join("env-root").exists());
    assert!(!rig.scratch.join("legacy-root").exists());
    assert!(!rig.scratch.join("flag-root").exists());
}

#[test]
fn root_report_keeps_legacy_notices_on_stderr_and_has_two_plain_lines() {
    let rig = Rig::new();
    let path = rig.root.to_str().unwrap();
    let out = scrubbed(&["root", "--json"], &[("PTY_SESSION_DIR", path)]);
    assert_eq!(out.code, 0);
    assert_eq!(out.json()["effective"]["source"], "PTY_SESSION_DIR");
    assert!(out.stderr.contains("PTY_SESSION_DIR is deprecated"));
    let quiet = scrubbed(&["root", "--json"], &[("PTY_SESSION_DIR", path), ("PTY_ROOT_LEGACY_SILENT", "1")]);
    assert!(quiet.stderr.is_empty());
    assert_eq!(quiet.json(), out.json());
    let plain = scrubbed(&["root"], &[("PTY_ROOT", path)]);
    assert_eq!(plain.code, 0);
    assert_eq!(plain.stdout.lines().collect::<Vec<_>>(), vec![
        format!("Effective: {path} (PTY_ROOT)"),
        format!("Native default: {}", out.json()["nativeDefault"]["path"].as_str().unwrap()),
    ]);
}

#[test]
fn root_report_is_exempt_from_socket_path_length_guard() {
    let long_root = format!("/tmp/{}", "long-root".repeat(30));
    for args in [
        vec!["root", "--json"],
        vec!["--root", long_root.as_str(), "root", "--json"],
    ] {
        let out = scrubbed(&args, &[("PTY_ROOT", &long_root)]);
        assert_eq!(out.code, 0, "{}", out.stderr);
        assert_eq!(out.json()["effective"]["path"], long_root);
        assert!(!out.stderr.contains("too long"));
    }
    let guarded = scrubbed(&["list", "--json"], &[("PTY_ROOT", &long_root)]);
    assert_eq!(guarded.code, 1);
    assert!(guarded.stderr.contains("too long"));
    let help = scrubbed(&["root", "--help"], &[("PTY_ROOT", &long_root)]);
    assert_eq!(help.code, 0);
    assert_eq!(help.stdout, include_str!("fixtures/help/root.txt"));
    let bad = scrubbed(&["root", "--unexpected"], &[]);
    assert_eq!(bad.code, 1);
}

/// node: tests/pty-root.test.ts:37-89, 233-257
#[test]
fn legacy_root_notices() {
    let rig = Rig::new();
    let a = rig.root.to_string_lossy().into_owned();
    let b = rig.scratch.to_string_lossy().into_owned();
    let out = scrubbed(
        &["list", "--json"],
        &[("PTY_ROOT", &a), ("PTY_SESSION_DIR", &b), ("PTY_ROOT_LEGACY_SILENT", "1")],
    );
    assert_eq!(out.code, 0);
    assert_eq!(out.stdout.trim(), "[]");
    assert_eq!(out.stderr, "");

    let out = scrubbed(&["list", "--json"], &[("PTY_SESSION_DIR", &b)]);
    assert_eq!(out.code, 0);
    assert_eq!(
        out.stderr,
        "pty: PTY_SESSION_DIR is deprecated; use PTY_ROOT (same shape, canonical name).\n"
    );

    let out = scrubbed(&["list", "--json"], &[("PTY_ROOT", &a)]);
    assert!(!out.stderr.contains("deprecated"));

    let out = scrubbed(
        &["list", "--json"],
        &[("PTY_SESSION_DIR", &b), ("PTY_ROOT_LEGACY_SILENT", "1")],
    );
    assert!(!out.stderr.contains("deprecated"));

    let out = scrubbed(&["list", "--json"], &[("PTY_ROOT", &a), ("PTY_SESSION_DIR", &b)]);
    assert_eq!(
        out.stderr,
        format!(
            "pty: both PTY_ROOT and PTY_SESSION_DIR are set — using PTY_ROOT ({a}); PTY_SESSION_DIR ({b}) is ignored (deprecated). For isolation, set PTY_ROOT.\n"
        )
    );
    assert_eq!(out.stderr.matches("both PTY_ROOT").count(), 1);
}

/// node: tests/pty-root.test.ts:93-142
#[test]
fn root_flag_pins_the_registry() {
    let rig = Rig::new();
    let flag_root = rig.scratch.join("flag-root");
    std::fs::create_dir_all(&flag_root).unwrap();
    // A planted record in the env root must not leak into the flag root.
    rig.write_meta("leak", serde_json::json!({"pid": 999999}));
    let out = rig.run(&["--root", flag_root.to_str().unwrap(), "list", "--json"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.stdout.trim(), "[]");
    // Any position works.
    let out = rig.run(&["list", "--json", "--root", flag_root.to_str().unwrap()]);
    assert_eq!(out.stdout.trim(), "[]");

    for args in [&["--root"][..], &["--root", "--json", "list"][..]] {
        let out = rig.run(args);
        assert_eq!(out.code, 1, "{args:?}");
        assert_eq!(
            out.stderr,
            "pty: --root requires a path (e.g. pty --root /var/lib/pty-eval list)\n"
        );
    }
}

#[test]
fn root_flag_after_command_separator_is_passed_to_the_child() {
    let rig = Rig::new();
    let result = rig.scratch.join("child-argv.txt");
    let other_root = rig.scratch.join("other-root");
    let script = format!(
        "printf '<%s>' \"$@\" > '{}'; exec sleep 30",
        result.display()
    );
    let other_root = other_root.to_str().unwrap();
    let out = rig.run(&[
        "run", "-d", "--id", "argv-root", "--", "/bin/sh", "-c", &script,
        "sh", "--root", other_root,
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    cli_common::wait_until("child arguments", || result.exists());
    assert_eq!(
        std::fs::read_to_string(&result).unwrap(),
        format!("<--root><{other_root}>")
    );
    assert!(rig.path("argv-root.json").exists());
    assert!(!rig.scratch.join("other-root").exists());
}

/// node: tests/gc-flap-clear-badge-root-len.test.ts:163-230
#[test]
fn root_length_backstop() {
    let socket_path_limit = pty_core::registry::SUN_PATH_MAX;
    let usable_root = socket_path_limit - (1 + 8 + 5);
    let long_root = format!("/tmp/{}", "a".repeat(95));
    let out = scrubbed(&["list"], &[("PTY_ROOT", &long_root)]);
    assert_ne!(out.code, 0);
    assert_eq!(
        out.stderr,
        format!(
            "pty: PTY_ROOT is too long — 100 bytes; must be ≤ {usable_root} bytes for the socket path to fit the {socket_path_limit}-byte kernel limit.\n  root: {long_root}\n  Shorten the root (or use `pty --root <shorter-path>` for a one-off).\n"
        )
    );

    // Fires before the command switch: an unknown command is never reached.
    let root105 = format!("/tmp/{}", "b".repeat(100));
    let out = scrubbed(&["definitely-not-a-real-subcommand"], &[("PTY_ROOT", &root105)]);
    assert!(out.stderr.contains("PTY_ROOT is too long"));
    assert!(!out.stderr.contains("Unknown command"));

    // The longest root that leaves room for `/xxxxxxxx.sock` is accepted.
    let fitting_root = format!("/tmp/{}", "c".repeat(usable_root - 5));
    std::fs::create_dir_all(&fitting_root).unwrap();
    let out = scrubbed(
        &["list", "--json"],
        &[
            ("PTY_ROOT", &fitting_root),
            ("PTY_ROOT_LEGACY_SILENT", "1"),
        ],
    );
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.stdout.trim(), "[]");
    let _ = std::fs::remove_dir(&fitting_root);

    // `--root <short>` overrides an over-long env root before the check.
    let rig = Rig::new();
    let out = scrubbed(
        &["--root", rig.root.to_str().unwrap(), "list", "--json"],
        &[("PTY_ROOT", &long_root), ("PTY_ROOT_LEGACY_SILENT", "1")],
    );
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.stdout.trim(), "[]");
}
