//! A large replay followed by a live output burst uses a memory budget,
//! independent of child write granularity or consumer drain timing.
mod daemon_support;
use daemon_support::*;
use pty_core::protocol::MessageType;
use std::path::Path;
use std::time::Duration;

const T: Duration = Duration::from_secs(15);

fn busy_session(root: &Path, name: &str, respawn: bool) -> Daemon {
    let script = format!(
        "i=0; while [ $i -lt 10000 ]; do printf 'history %s xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\\r\\n' $i; i=$((i+1)); done; printf 'READY\\r\\n'; touch {}; read trigger; dd if=/dev/zero bs=16384 count=384 2>/dev/null | tr '\\000' x; printf '\\r\\nAFTER-THE-FLOOD\\r\\n'; touch {}; exec sleep 300",
        root.join("history-ready").display(), root.join("burst-done").display()
    );
    let mut cfg = config(name, "sh", &["-c", &script]);
    cfg["respawn"] = serde_json::json!(respawn);
    cfg["tags"] = serde_json::json!({"keep":"true"});
    Daemon::start(root, cfg)
}

fn catch_up_and_detach(d: &Daemon) {
    assert!(wait_until(T, || d.root.join("history-ready").exists()));
    let mut slow = d.connect();
    slow.attach(40, 120);
    assert!(slow.wait_text("READY", T));
    assert!(slow.screen().unwrap().len() > 100_000, "large initial replay");
    slow.data("burst\n");
    // The producer's file is the barrier: the whole six-MiB burst is now
    // generated. The attacher has deliberately consumed none of it yet.
    assert!(wait_until(T, || d.root.join("burst-done").exists()));
    assert!(slow.wait_for(T, |packets| {
        let bytes: Vec<_> = packets.iter()
            .filter(|packet| matches!(packet.type_, MessageType::Data | MessageType::Screen))
            .flat_map(|packet| packet.payload.iter().copied())
            .collect();
        bytes.windows(b"AFTER-THE-FLOOD".len()).any(|part| part == b"AFTER-THE-FLOOD")
    }), "the daemon dropped live output while the attacher was catching up");
    assert!(!slow.is_closed(), "attachment must remain live after draining");
    slow.detach();
    assert!(slow.wait_closed(T));
    assert!(!process_exited(d.pid), "detach must not end the session daemon");
    let mut status = d.connect();
    status.status();
    assert!(status.wait_status(T).is_some(), "session still answers after detach");
}

#[test]
fn a_busy_session_keeps_a_slow_draining_attacher_connected() {
    skip_without_a_real_machine!();
    let _serial = serial();
    let root = short_root();
    let d = busy_session(&root, &unique_name("flood"), false);
    catch_up_and_detach(&d);
}

#[test]
fn a_recovered_live_session_keeps_a_slow_draining_attacher_connected() {
    skip_without_a_real_machine!();
    let _serial = serial();
    let root = short_root();
    let name = unique_name("recovered");
    let mut cfg = config(&name, "sh", &["-c", "printf 'prior incarnation\\r\\n'; exec sleep 300"]);
    cfg["tags"] = serde_json::json!({"keep":"true"});
    let mut previous = Daemon::start(&root, cfg);
    previous.signal(libc::SIGTERM);
    assert!(previous.wait_exit(T).is_some());
    assert!(previous.meta().is_some(), "existing session record survives shutdown");
    let recovered = busy_session(&root, &name, true);
    assert!(!recovered.events("session_respawn").is_empty());
    catch_up_and_detach(&recovered);
}
