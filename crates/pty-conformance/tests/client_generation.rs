//! `clientGeneration`: the Rust daemon bumps a counter in `<id>.json`
//! whenever the client facts `pty stats` reports change — a writable client
//! attaches, resizes or leaves — so an observer can stat the record and know
//! when to ask the daemon again. Observing (PEEK, STATUS) never bumps it.
//! The Node daemon has no such counter (docs/decisions/0015).

use pty_conformance::*;
use std::time::Duration;

fn client_generation(rig: &Rig, id: &str) -> Option<u64> {
    rig.meta(id)?.get("clientGeneration")?.as_u64()
}

fn record(rig: &Rig, id: &str) -> Vec<u8> {
    std::fs::read(rig.meta_path(id)).expect("record")
}

/// Wait until the counter moves past `after`, and return the new value.
fn next_generation(rig: &Rig, id: &str, after: Option<u64>, what: &str) -> u64 {
    wait_until(what, || client_generation(rig, id) > after);
    client_generation(rig, id).expect("counter")
}

/// Rust half of `client_changes_write_no_generation_node`.
#[test]
fn attach_resize_and_detach_each_bump_the_generation_rust() {
    if !is_rust() {
        return;
    }
    let rig = Rig::new();
    rig.daemon("cg-life", &["cat"], DaemonOpts::no_display_name());
    assert_eq!(
        client_generation(&rig, "cg-life"),
        None,
        "no client has come yet"
    );

    let mut conn = rig.connect("cg-life");
    conn.attach(24, 80);
    let attached = next_generation(&rig, "cg-life", None, "the attach bump");
    let after_attach = record(&rig, "cg-life");

    conn.resize(30, 100);
    let resized = next_generation(&rig, "cg-life", Some(attached), "the resize bump");
    let after_resize = record(&rig, "cg-life");
    assert_ne!(after_attach, after_resize);

    // A second writable client, then its departure by closing the socket
    // rather than by DETACH: both are client facts.
    let mut other = rig.connect("cg-life");
    other.attach(20, 70);
    let joined = next_generation(&rig, "cg-life", Some(resized), "the second attach");
    drop(other);
    let left = next_generation(&rig, "cg-life", Some(joined), "the dropped client");

    conn.detach();
    next_generation(&rig, "cg-life", Some(left), "the detach bump");
}

/// A RESIZE that repeats the client's size changes no fact and so no record.
#[test]
fn a_resize_to_the_same_size_leaves_the_record_alone() {
    if !is_rust() {
        return;
    }
    let rig = Rig::new();
    rig.daemon("cg-same", &["cat"], DaemonOpts::no_display_name());
    let mut conn = rig.connect("cg-same");
    conn.attach(24, 80);
    next_generation(&rig, "cg-same", None, "the attach bump");
    let before = record(&rig, "cg-same");
    conn.resize(24, 80);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(record(&rig, "cg-same"), before);
}

/// Looking at a session must not change what an observer of its record sees,
/// or every observer would wake every other one.
#[test]
fn peek_and_stats_leave_the_record_alone() {
    if !is_rust() {
        return;
    }
    let rig = Rig::new();
    rig.daemon("cg-look", &["cat"], DaemonOpts::no_display_name());
    let before = record(&rig, "cg-look");

    let out = rig.pty(&["peek", "cg-look"]);
    expect_status(&out, 0);
    let out = rig.pty(&["stats", "cg-look", "--json"]);
    expect_status(&out, 0);
    let mut follower = rig.connect("cg-look");
    follower.peek(false, false);
    std::thread::sleep(Duration::from_millis(200));
    drop(follower);
    std::thread::sleep(Duration::from_millis(300));

    assert_eq!(record(&rig, "cg-look"), before);
}

/// Node half (docs/decisions/0015): a Node daemon stamps `lastAttachAt` on
/// ATTACH but keeps no client counter.
#[test]
fn client_changes_write_no_generation_node() {
    if !is_node() {
        return;
    }
    let rig = Rig::new();
    rig.daemon("cg-life", &["cat"], DaemonOpts::no_display_name());
    let mut conn = rig.connect("cg-life");
    conn.attach(24, 80);
    wait_until("the attach stamp", || {
        rig.meta("cg-life")
            .is_some_and(|m| m.get("lastAttachAt").is_some())
    });
    conn.resize(30, 100);
    conn.detach();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(client_generation(&rig, "cg-life"), None);
}
