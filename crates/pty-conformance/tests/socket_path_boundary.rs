//! Positive counterpart to the socket-path overflow cases: a real daemon can
//! bind and serve the longest combined `<PTY_ROOT>/<id>.sock` path supported
//! by the current platform.

use pty_conformance::*;
use pty_core::protocol::MessageType;
use std::time::Duration;

const SOCKET_PATH_LIMIT: usize = pty_core::registry::SUN_PATH_MAX;

fn operation_budget() -> Duration {
    if unoptimized_binary_under_test() {
        deadline() * 2
    } else {
        deadline()
    }
}

#[test]
fn longest_platform_socket_path_starts_and_serves_a_session() {
    let rig = Rig::new();
    let overhead = rig.root().as_os_str().as_encoded_bytes().len() + 1 + ".sock".len();
    assert!(
        overhead < SOCKET_PATH_LIMIT,
        "test PTY_ROOT is too long: {}",
        rig.root().display()
    );
    let id = "b".repeat(SOCKET_PATH_LIMIT - overhead);
    let socket = rig.socket_path(&id);
    assert_eq!(
        socket.as_os_str().as_encoded_bytes().len(),
        SOCKET_PATH_LIMIT,
        "{}",
        socket.display()
    );

    let daemon = rig.daemon(&id, &["cat"], DaemonOpts::no_display_name().ephemeral());
    let pid = daemon.pid();
    let mut client = rig.connect(&id);
    client.attach(24, 80);
    client
        .wait_for(MessageType::Screen, operation_budget())
        .unwrap_or_else(|| panic!("maximal socket {} did not serve ATTACH", socket.display()));
    client.detach();
    wait_until_for(
        "maximal-path client detach",
        operation_budget(),
        &mut || {
            let _ = client.next_packet(Duration::from_millis(20));
            client.is_eof()
        },
    );
    drop(client);

    let stopped = rig.pty(&["kill", &id]);
    expect_status(&stopped, 0);
    wait_for_process_gone(pid);
    wait_until_for(
        "maximal-path session files to be removed",
        operation_budget(),
        &mut || !rig.meta_path(&id).exists() && !socket.exists() && !rig.pid_path(&id).exists(),
    );
}

#[test]
fn logical_id_longer_than_socket_budget_can_list_attach_query_and_stop() {
    let rig = Rig::new();
    let id = "n".repeat(120);
    let socket = rig.socket_path(&id);
    assert!(socket.as_os_str().len() <= SOCKET_PATH_LIMIT);
    assert!(
        socket
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with('@')
    );
    let daemon = rig.daemon(&id, &["cat"], DaemonOpts::no_display_name().ephemeral());
    let pid = daemon.pid();

    let listed = rig.pty(&["list", "--json"]);
    expect_status(&listed, 0);
    let entries = expect_json(&listed);
    assert!(
        entries
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == id)
    );
    let stats = rig.pty(&["stats", &id, "--json"]);
    expect_status(&stats, 0);
    let mut client = rig.connect(&id);
    client.attach(24, 80);
    assert!(
        client
            .wait_for(MessageType::Screen, operation_budget())
            .is_some()
    );
    client.detach();
    drop(client);

    let stopped = rig.pty(&["kill", &id]);
    expect_status(&stopped, 0);
    wait_for_process_gone(pid);
    wait_until_for("bounded socket cleanup", operation_budget(), &mut || {
        !rig.meta_path(&id).exists() && !socket.exists() && !rig.pid_path(&id).exists()
    });
}

#[cfg(target_os = "macos")]
#[test]
fn darwin_daemon_records_start_identity_without_ps_on_path() {
    let rig = Rig::new();
    let id = "no-ps-on-path";
    let daemon = rig.daemon(
        id,
        &["/bin/cat"],
        DaemonOpts::no_display_name()
            .ephemeral()
            .invoke_env("PATH", "/nonexistent"),
    );
    let pid = daemon.pid();
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(rig.meta_path(id)).unwrap()).unwrap();
    assert!(
        metadata["daemonStartToken"]
            .as_str()
            .is_some_and(|token| token.starts_with("darwin:"))
    );
    let stopped = rig.pty(&["kill", id]);
    expect_status(&stopped, 0);
    wait_for_process_gone(pid);
}

#[test]
fn registry_sidecar_limit_accepts_boundary_and_rejects_overflow() {
    let rig = Rig::new();
    let id = "s".repeat(209);
    let daemon = rig.daemon(&id, &["cat"], DaemonOpts::no_display_name().ephemeral());
    let pid = daemon.pid();
    expect_status(&rig.pty(&["kill", &id]), 0);
    wait_for_process_gone(pid);
    let overflow = "s".repeat(210);
    let out = rig.pty(&["run", "-d", "--id", &overflow, "--", "cat"]);
    expect_failure(&out);
    expect_contains(&out.stderr(), "registry sidecars (max 209 characters)");
    assert!(!rig.meta_path(&overflow).exists());
}
