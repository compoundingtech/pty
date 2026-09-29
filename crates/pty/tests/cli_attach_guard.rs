mod cli_common;

use std::os::unix::net::UnixListener;
use std::time::{Duration, Instant};

use cli_common::Rig;

#[test]
fn delayed_status_does_not_allow_unforced_attach() {
    let rig = Rig::new();
    // The listener makes the session visible to registry discovery, but it
    // never answers STATUS. The guard must refuse before ATTACH is sent.
    let _listener = UnixListener::bind(rig.path("slow.sock")).unwrap();
    let started = Instant::now();
    let out = rig.run(&["attach", "slow"]);
    assert_eq!(out.code, 1, "{out:?}");
    assert!(out.stderr.contains("cannot verify the current session"), "{out:?}");
    assert!(started.elapsed() >= Duration::from_millis(500));
}
