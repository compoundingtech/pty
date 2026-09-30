//! Port of the pty project's `tests/env-isolation.test.ts` (the `buildSpawnEnv`
//! portion). Verifies the spawn environment scrubs the harness's own
//! pty-internal context so a test running inside a pty session can't leak the
//! real live session dir into spawned children.

use pty_testkit::build_spawn_env;

fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn always_scrubs_pty_session_and_server_config() {
    let env = build_spawn_env(
        &kv(&[
            ("PTY_SESSION", "example.pty"),
            ("PTY_SERVER_CONFIG", "{}"),
            ("HOME", "/h"),
        ]),
        &[],
    );
    assert!(!env.contains_key("PTY_SESSION"));
    assert!(!env.contains_key("PTY_SERVER_CONFIG"));
    assert_eq!(env.get("HOME").map(String::as_str), Some("/h")); // unrelated vars pass through
}

#[test]
fn scrubs_ambient_root_and_dir_when_caller_didnt_set_them() {
    let env = build_spawn_env(
        &kv(&[("PTY_ROOT", "/real/root"), ("PTY_SESSION_DIR", "/real/dir")]),
        &[],
    );
    assert!(!env.contains_key("PTY_ROOT"));
    assert!(!env.contains_key("PTY_SESSION_DIR"));
}

#[test]
fn ambient_root_does_not_override_explicit_session_dir() {
    let env = build_spawn_env(
        &kv(&[("PTY_ROOT", "/real/root"), ("HOME", "/h")]),
        &kv(&[("PTY_SESSION_DIR", "/tmp/isolated")]),
    );
    assert!(!env.contains_key("PTY_ROOT"));
    assert_eq!(
        env.get("PTY_SESSION_DIR").map(String::as_str),
        Some("/tmp/isolated")
    );
}

#[test]
fn keeps_explicit_pty_root_override() {
    let env = build_spawn_env(
        &kv(&[("PTY_ROOT", "/ambient/root")]),
        &kv(&[("PTY_ROOT", "/wanted/root")]),
    );
    assert_eq!(env.get("PTY_ROOT").map(String::as_str), Some("/wanted/root"));
}

#[test]
fn keeps_explicit_session_dir() {
    let env = build_spawn_env(&[], &kv(&[("PTY_SESSION_DIR", "/wanted/dir")]));
    assert_eq!(
        env.get("PTY_SESSION_DIR").map(String::as_str),
        Some("/wanted/dir")
    );
}
