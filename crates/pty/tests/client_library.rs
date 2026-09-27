//! `pty-client`'s rooted operations against real daemons. The binary only
//! starts the sessions; everything after that goes through the library, on
//! the rig's registry root, never through `$PTY_ROOT`.
//!
//! The test process's own `$PTY_ROOT` is left as it is. Every session has a
//! name no other registry holds, so an operation that ignored its root would
//! find nothing to act on rather than something real.

mod cli_common;

use cli_common::{Rig, wait_until};
use pty_client::list::ListOptions;
use pty_client::{Delivery, PeekScreenOptions, SendOptions, SignalError};
use pty_core::registry::SessionStatus;

fn unique(label: &str) -> String {
    format!("lib-{label}-{}", std::process::id())
}

fn status(rig: &Rig, id: &str) -> Option<SessionStatus> {
    pty_client::list::list(&rig.root, &ListOptions::default())
        .into_iter()
        .find(|s| s.info.name == id)
        .map(|s| s.info.status)
}

/// Start `cat` under `id`, keeping its record after it exits.
fn spawn_kept_cat(rig: &Rig, id: &str) {
    let out = rig.run_env(
        &["run", "-d", "--id", id, "--", "cat"],
        &[("PTY_REAP_ON_EXIT", "false")],
    );
    assert_eq!(out.code, 0, "run -d {id}: {}", out.stderr);
    wait_until("socket up", || {
        std::os::unix::net::UnixStream::connect(rig.path(&format!("{id}.sock"))).is_ok()
    });
}

#[test]
fn a_private_root_is_listed_written_read_stopped_and_removed_through_the_library() {
    let rig = Rig::new();
    let id = unique("drive");
    rig.spawn_cat(&id, &[]);
    let root = rig.root.as_path();

    assert_eq!(status(&rig, &id), Some(SessionStatus::Running));

    pty_client::send_in(
        root,
        &id,
        &[b"through-the-library\r".as_slice()],
        SendOptions::default(),
    )
    .expect("send");
    let plain = PeekScreenOptions {
        plain: true,
        full: false,
    };
    wait_until("the echo on the screen", || {
        pty_client::peek_screen_in(root, &id, plain)
            .is_ok_and(|screen| screen.contains("through-the-library"))
    });

    let stats = pty_client::query_stats_in(root, &id).expect("stats");
    assert!(stats.process.alive, "{stats:?}");

    let stopped = pty_client::stop_in(root, &id).expect("stop");
    assert!(stopped.verified_empty(), "{stopped:?}");
    assert_ne!(status(&rig, &id), Some(SessionStatus::Running));
    assert!(
        !rig.exists(&format!("{id}.sock")),
        "the socket is cleaned up"
    );

    pty_client::remove_in(root, &id).expect("remove");
    assert_eq!(status(&rig, &id), None);
    assert!(!rig.exists(&format!("{id}.json")), "the record is gone");
}

#[test]
fn stopping_or_removing_what_is_not_there_says_so() {
    let rig = Rig::new();
    let id = unique("absent");
    assert_eq!(
        pty_client::stop_in(&rig.root, &id).unwrap_err().to_string(),
        format!("Session \"{id}\" not found.")
    );
    assert_eq!(
        pty_client::remove_in(&rig.root, &id)
            .unwrap_err()
            .to_string(),
        format!("Session \"{id}\" not found.")
    );
}

#[test]
fn a_running_session_is_not_removed() {
    let rig = Rig::new();
    let id = unique("running");
    rig.spawn_cat(&id, &[]);
    assert_eq!(
        pty_client::remove_in(&rig.root, &id)
            .unwrap_err()
            .to_string(),
        format!("Session \"{id}\" is still running. Use \"pty kill {id}\" first.")
    );
    assert_eq!(status(&rig, &id), Some(SessionStatus::Running));
}

/// The signal reaches the program, not the daemon: `cat` dies of SIGTERM
/// itself (exit code 143). A signalled daemon would have stopped the session
/// the external way, hanging the child up (129) instead.
#[test]
fn a_signal_reaches_the_program_and_not_its_daemon() {
    let rig = Rig::new();
    let id = unique("signal");
    spawn_kept_cat(&rig, &id);
    let root = rig.root.as_path();
    let generation = pty_core::registry::read_metadata_in(root, &id)
        .and_then(|m| m.generation)
        .expect("a generation");

    let refused =
        pty_client::signal_in(root, &id, libc::SIGTERM, Some("not-this-generation")).unwrap_err();
    assert!(
        matches!(refused, SignalError::GenerationChanged { .. }),
        "{refused:?}"
    );
    let stats = pty_client::query_stats_in(root, &id).expect("stats");
    assert!(stats.process.alive, "a refused signal is not sent");

    let sent = pty_client::signal_in(root, &id, libc::SIGTERM, Some(&generation)).expect("signal");
    assert_eq!(Some(sent.pid), stats.process.pid);
    assert_eq!(sent.delivery, Delivery::Group);

    wait_until("the session to exit", || {
        status(&rig, &id) == Some(SessionStatus::Exited)
    });
    let exit_code = pty_core::registry::read_metadata_in(root, &id).and_then(|m| m.exit_code);
    assert_eq!(exit_code, Some(143), "cat died of the SIGTERM itself");
}

#[test]
fn a_session_that_is_not_running_is_not_signalled() {
    let rig = Rig::new();
    let id = unique("exited");
    spawn_kept_cat(&rig, &id);
    pty_client::stop_in(&rig.root, &id).expect("stop");
    let refused = pty_client::signal_in(&rig.root, &id, libc::SIGTERM, None).unwrap_err();
    assert!(
        matches!(refused, SignalError::NotRunning { .. }),
        "{refused:?}"
    );
}
