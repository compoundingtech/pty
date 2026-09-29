#![cfg(target_os = "linux")]

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::Duration;

use pty_core::protocol::{encode_attach, encode_resize};

#[test]
fn an_extreme_resize_cannot_stop_status_queries() {
    let root = std::env::temp_dir().join(format!(
        "px-{}",
        pty_testkit::server::random_id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let bin = env!("CARGO_BIN_EXE_pty");
    let started = Command::new(bin)
        .args(["run", "-d", "--id", "large", "--", "sleep", "30"])
        .env("PTY_ROOT", &root)
        .env_remove("PTY_SESSION")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(started.status.success(), "{}", String::from_utf8_lossy(&started.stderr));
    let mut client = UnixStream::connect(root.join("large.sock")).unwrap();
    client.write_all(&encode_attach(24, 80)).unwrap();
    client.write_all(&encode_resize(u16::MAX, 80)).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let responsive = pty_client::query_stats_in_with_timeout(
        &root,
        "large",
        Duration::from_millis(700),
    )
    .is_ok();

    let pid = std::fs::read_to_string(root.join("large.pid"))
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    // SAFETY: this PID came from the private registry created for this test.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    drop(client);
    let _ = std::fs::remove_dir_all(root);
    assert!(responsive, "an extreme size stalled the daemon's status query");
}
