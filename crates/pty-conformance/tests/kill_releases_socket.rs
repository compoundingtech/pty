//! Port of tests/kill-releases-socket-command.test.ts and the
//! `bin/pty-kill-releases-socket-test` script it runs (the script is dropped
//! as a shipped binary, docs/parity.md §12; the case lives here instead).
//! A session's command launches a 3-deep tree (launcher -> middle -> socket
//! owner); the owner binds a Unix socket, ignores SIGHUP and SIGTERM, and
//! writes a ready file. `pty kill` must return only once the owner is dead
//! and the socket released, so `pty run -a -d --id` with the same tree
//! starts a replacement owner on the same path.

use pty_conformance::*;
use std::path::{Path, PathBuf};
use std::time::Duration;

const OWNER_PY: &str = r#"
import os, signal, socket, sys
sock_path, ready, pidfile = sys.argv[1:4]
signal.signal(signal.SIGHUP, signal.SIG_IGN)
signal.signal(signal.SIGTERM, signal.SIG_IGN)
s = socket.socket(socket.AF_UNIX)
try:
    s.bind(sock_path)
except OSError:
    probe = socket.socket(socket.AF_UNIX)
    try:
        probe.connect(sock_path)
        sys.stderr.write("socket owner failed: live owner still accepts connections\n")
        sys.exit(73)
    except OSError:
        pass
    os.unlink(sock_path)
    s.bind(sock_path)
s.listen(1)
with open(pidfile, "w") as f:
    f.write(f"{os.getpid()}\n")
with open(ready, "w") as f:
    f.write("ready\n")
while True:
    signal.pause()
"#;

struct Tree {
    launcher: PathBuf,
}

fn install_tree(rig: &Rig) -> Tree {
    use std::os::unix::fs::PermissionsExt;
    let dir = rig.tmp().join("tree");
    std::fs::create_dir_all(&dir).unwrap();
    let owner = dir.join("owner.py");
    std::fs::write(&owner, OWNER_PY).unwrap();
    let middle = dir.join("middle.sh");
    std::fs::write(
        &middle,
        format!(
            "#!/bin/sh\npython3 '{}' \"$@\" </dev/null >/dev/null 2>&1 &\nwhile :; do sleep 3600; done\n",
            owner.display()
        ),
    )
    .unwrap();
    let launcher = dir.join("launcher.sh");
    std::fs::write(
        &launcher,
        format!(
            "#!/bin/sh\nsh '{}' \"$@\" </dev/null >/dev/null 2>&1 &\nwhile :; do sleep 3600; done\n",
            middle.display()
        ),
    )
    .unwrap();
    for p in [&middle, &launcher] {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    Tree { launcher }
}

fn wait_ready(ready: &Path, pidfile: &Path) -> i32 {
    assert!(
        poll_for(Duration::from_secs(5), || ready.exists()),
        "timeout waiting for {}",
        ready.display()
    );
    read_pid_file(pidfile).expect("owner pid")
}

/// node: tests/kill-releases-socket-command.test.ts:11
#[test]
fn kill_releases_the_owned_socket_and_a_replacement_starts() {
    let rig = Rig::new();
    let tree = install_tree(&rig);
    let sock = rig.tmp().join("owned.sock");
    let first_ready = rig.tmp().join("first.ready");
    let first_pid = rig.tmp().join("first.pid");
    let second_ready = rig.tmp().join("second.ready");
    let second_pid = rig.tmp().join("second.pid");
    let launcher = tree.launcher.to_str().unwrap().to_string();
    let sock_s = sock.to_str().unwrap().to_string();

    let first = rig.daemon(
        "kill-socket",
        &["sh", &launcher, &sock_s, first_ready.to_str().unwrap(), first_pid.to_str().unwrap()],
        DaemonOpts::no_display_name(),
    );
    let first_daemon = first.pid();
    let first_owner = wait_ready(&first_ready, &first_pid);
    assert!(pid_alive(first_owner));
    assert!(sock.exists());

    let killed = rig.pty(&["kill", "kill-socket"]);
    expect_status(&killed, 0);
    assert!(process_exited(first_daemon), "daemon survived kill");
    assert!(process_exited(first_owner), "socket owner {first_owner} survived kill (it ignores HUP/TERM)");

    let second = rig.pty(&[
        "run",
        "-a",
        "-d",
        "--id",
        "kill-socket",
        "--no-display-name",
        "--",
        "sh",
        &launcher,
        &sock_s,
        second_ready.to_str().unwrap(),
        second_pid.to_str().unwrap(),
    ]);
    expect_status(&second, 0);
    let second_owner = wait_ready(&second_ready, &second_pid);
    assert!(pid_alive(second_owner));
    assert_ne!(second_owner, first_owner);
    // The replacement owner bound the same path (a live first owner would
    // have made it exit 73 before writing its ready file).
    assert!(sock.exists());

    expect_status(&rig.pty(&["kill", "kill-socket"]), 0);
    assert!(process_exited(second_owner), "second owner {second_owner} survived kill");
}
