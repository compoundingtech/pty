//! Port of tests/remote-reconnect.test.ts: the `attach --remote` reconnect
//! path. Reconnect fires only on a LOUD close of the fabric tunnel, never on
//! a stall. The killable tunnel is the bridge in `remote_support`: `drop`
//! severs every live tunnel while the listener stays up (a fresh dial then
//! re-attaches by identity and the daemon replays its screen); `block` makes
//! new dials fail at the transport level (an outage) until `unblock`.
//!
//! Node puts its killable proxy in front of a persistent `remote-serve
//! --socket` daemon; that form is dropped (docs/parity.md §12), so the
//! bridge spawns `pty remote-serve --stdio` per tunnel instead. The
//! observable contract is the same: a dropped tunnel is a loud close.

mod remote_support;

use pty_conformance::*;
use remote_support::*;
use std::time::Duration;

/// The Node `beforeAll`: a remote `cat` shell that prints RECONNECT_READY.
fn remote_with_shell(rig: &Rig) -> Bridge {
    let srv = rig.make_root();
    let r = remote_run(rig, &srv, &["--id", "rshell", "--", "sh", "-c", "echo RECONNECT_READY; cat"]);
    expect_status(&r, 0);
    wait_remote_socket(&srv, "rshell");
    Bridge::start(rig, &srv)
}

fn attach_remote(rig: &Rig, id: &str) -> TtyProc {
    rig.pty_tty_raw(&[], &[], &["attach", "--remote", "testpeer", id], 24, 80)
}

/// node: tests/remote-reconnect.test.ts:142
#[test]
fn bridge_tunnels_attach_remote_and_drop_severs_it() {
    let rig = Rig::new();
    let bridge = remote_with_shell(&rig);
    let mut t = attach_remote(&rig, "rshell");
    assert!(t.wait_for_text("RECONNECT_READY", Duration::from_secs(8)), "{:?}", t.output_str());
    t.write(b"PRE_DROP\r");
    assert!(t.wait_for_text("PRE_DROP", Duration::from_secs(8)), "{:?}", t.output_str());
    assert!(bridge.active_count() > 0, "a live tunnel should exist");
    bridge.drop_tunnels();
    assert_eq!(bridge.active_count(), 0, "tunnel should be gone at once");
    t.kill();
}

/// node: tests/remote-reconnect.test.ts:164
#[test]
fn attach_remote_survives_a_tunnel_drop_and_resumes() {
    let rig = Rig::new();
    let bridge = remote_with_shell(&rig);
    let mut t = attach_remote(&rig, "rshell");
    assert!(t.wait_for_text("RECONNECT_READY", Duration::from_secs(8)), "{:?}", t.output_str());
    t.write(b"BEFORE_DROP\r");
    assert!(t.wait_for_text("BEFORE_DROP", Duration::from_secs(8)), "{:?}", t.output_str());

    bridge.drop_tunnels();
    // attach must not exit: it re-dials a fresh tunnel, re-attaches, and the
    // daemon replays the screen; the remote cat persisted, so input echoes
    // again only if the reconnect succeeded.
    std::thread::sleep(Duration::from_secs(3));
    assert!(t.try_exit().is_none(), "attach exited after the drop: {:?}", t.output_str());
    t.write(b"AFTER_RECONNECT\r");
    assert!(t.wait_for_text("AFTER_RECONNECT", Duration::from_secs(12)), "{:?}", t.output_str());
    let out = t.output_str();
    expect_contains(&out, "[reconnecting… — Ctrl-\\ or Ctrl-C to stop]");
    t.kill();
}

/// node: tests/remote-reconnect.test.ts:188
#[test]
fn attach_remote_survives_a_long_outage_and_reconnects_when_the_peer_returns() {
    let rig = Rig::new();
    let bridge = remote_with_shell(&rig);
    let mut t = attach_remote(&rig, "rshell");
    assert!(t.wait_for_text("RECONNECT_READY", Duration::from_secs(8)), "{:?}", t.output_str());

    // Peer unreachable and the live tunnel drops: every re-dial fails, and
    // attach keeps retrying through several backoff cycles.
    bridge.block();
    bridge.drop_tunnels();
    assert!(t.wait_for_text("reconnecting", Duration::from_secs(8)), "{:?}", t.output_str());
    std::thread::sleep(Duration::from_secs(6));
    assert!(t.try_exit().is_none(), "attach gave up during the outage: {:?}", t.output_str());

    // Peer back: the next retry lands and the session resumes.
    bridge.unblock();
    std::thread::sleep(Duration::from_secs(6));
    t.write(b"AFTER_OUTAGE\r");
    assert!(t.wait_for_text("AFTER_OUTAGE", Duration::from_secs(12)), "{:?}", t.output_str());
    t.kill();
}

/// node: tests/remote-reconnect.test.ts:215
/// PTY.TRL-R11 (2026-10-08) excludes transport failures from cross-runtime parity.
#[test]
fn attach_remote_reports_the_runtime_transport_failure_contract() {
    let rig = Rig::new();
    let bridge = remote_with_shell(&rig);
    let sid = unique_id("gone-");
    let r = remote_run(&rig, &bridge.srv_root, &["--id", &sid, "--", "sh", "-c", "echo GONE_READY; cat"]);
    expect_status(&r, 0);
    wait_remote_socket(&bridge.srv_root, &sid);
    std::thread::sleep(Duration::from_millis(400));

    let mut t = attach_remote(&rig, &sid);
    assert!(t.wait_for_text("GONE_READY", Duration::from_secs(8)), "{:?}", t.output_str());
    // Kill the remote session, then drop the tunnel: the re-dial reaches the
    // host but the route is refused (session gone) — a clean give-up, not an
    // endless reconnect.
    let k = rig.pty_env(&[("PTY_ROOT", &bridge.srv_root.to_string_lossy())], &["kill", &sid]);
    expect_status(&k, 0);
    std::thread::sleep(Duration::from_millis(400));
    bridge.drop_tunnels();
    let (header, status) = if is_rust() {
        (format!("[connection lost to {sid}]"), 1)
    } else {
        (format!("[{sid} session ended]"), 0)
    };
    assert!(t.wait_for_text(&header, Duration::from_secs(12)), "{:?}", t.output_str());
    let code = t.wait_exit(Duration::from_secs(5));
    let out = t.output_str();
    expect_contains(&out, &header);
    assert_eq!(code, Some(status), "transport failure contract: {out:?}");
}
