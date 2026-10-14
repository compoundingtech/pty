#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

struct Fixture {
    child: Child,
    root: PathBuf,
}
impl Fixture {
    fn new(child: Child) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "pty-stop-identity-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        Self { child, root }
    }
    fn record(&self, pid: i32, token: Option<String>, boot: Option<String>) -> UnixListener {
        let socket = UnixListener::bind(self.root.join("owner.sock")).unwrap();
        std::fs::write(self.root.join("owner.pid"), pid.to_string()).unwrap();
        // JSON keeps this test source compilable on the unfixed base too.
        std::fs::write(
            self.root.join("owner.json"),
            serde_json::to_vec(&serde_json::json!({
                "generation": "original", "daemonPid": pid,
                "daemonStartToken": token, "daemonBootId": boot
            }))
            .unwrap(),
        )
        .unwrap();
        socket
    }
    fn refused(&mut self) {
        let result = pty_client::stop_in(&self.root, "owner", Some("original"));
        assert!(
            result.is_err(),
            "unverified owner must refuse stop: {result:?}"
        );
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "stop signalled an unrelated owner"
        );
        assert!(
            self.root.join("owner.sock").exists(),
            "refusal must keep evidence"
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn child() -> Child {
    Command::new("sleep").arg("60").spawn().unwrap()
}
fn boot() -> String {
    format!(
        "linux:{}",
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .unwrap()
            .trim()
    )
}
fn token(pid: i32) -> String {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    format!(
        "linux:{}",
        stat[stat.rfind(')').unwrap() + 2..]
            .split_whitespace()
            .nth(19)
            .unwrap()
    )
}

#[test]
fn reused_pid_token_mismatch_refuses_stop() {
    let mut fixture = Fixture::new(child());
    let pid = fixture.child.id() as i32;
    let _socket = fixture.record(pid, Some("linux:0".into()), Some(boot()));
    fixture.refused();
}

#[test]
fn boot_mismatch_refuses_stop() {
    let mut fixture = Fixture::new(child());
    let pid = fixture.child.id() as i32;
    let _socket = fixture.record(pid, Some(token(pid)), Some("previous-boot".into()));
    fixture.refused();
}

#[test]
fn missing_identity_refuses_stop() {
    for missing_boot in [false, true] {
        let mut fixture = Fixture::new(child());
        let pid = fixture.child.id() as i32;
        let _socket = fixture.record(
            pid,
            (!missing_boot).then(|| token(pid)),
            missing_boot.then(boot),
        );
        fixture.refused();
    }
}

#[test]
fn matching_owner_remains_stoppable() {
    let fixture = Fixture::new(child());
    let pid = fixture.child.id() as i32;
    let _socket = fixture.record(pid, Some(token(pid)), Some(boot()));
    let stopped = pty_client::stop_in(&fixture.root, "owner", Some("original")).unwrap();
    assert!(
        stopped.verified_empty(),
        "matching owner must stop completely: {stopped:?}"
    );
}

#[test]
fn thread_fixture() {
    if std::env::var_os("PTY_IDENTITY_THREAD_CHILD").is_none() {
        return;
    }
    let (send, recv) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        // SAFETY: gettid has no arguments or side effects.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) };
        send.send(tid).unwrap();
        std::thread::sleep(Duration::from_secs(60));
    });
    println!("IDENTITY_THREAD={}", recv.recv().unwrap());
    worker.join().unwrap();
}

#[test]
fn reused_pid_is_nonleader_thread_refuses_stop() {
    let process = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "thread_fixture", "--nocapture"])
        .env("PTY_IDENTITY_THREAD_CHILD", "1")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut fixture = Fixture::new(process);
    let stdout = fixture.child.stdout.take().unwrap();
    let tid = BufReader::new(stdout)
        .lines()
        .find_map(|line| {
            line.unwrap()
                .strip_prefix("IDENTITY_THREAD=")
                .map(|value| value.parse::<i32>().unwrap())
        })
        .expect("child publishes its worker thread ID");
    assert_ne!(tid, fixture.child.id() as i32);
    let _socket = fixture.record(tid, Some(token(tid)), Some(boot()));
    fixture.refused();
}

#[test]
fn unreadable_identity_child() {
    if std::env::var_os("PTY_IDENTITY_UNREADABLE_CHILD").is_none() {
        return;
    }
    let mut fixture = Fixture::new(child());
    let pid = fixture.child.id() as i32;
    let _socket = fixture.record(pid, Some(token(pid)), Some(boot()));
    fixture.refused();
}

#[test]
fn unreadable_boot_identity_refuses_stop() {
    let mut process = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "unreadable_identity_child", "--nocapture"])
        .env("PTY_IDENTITY_UNREADABLE_CHILD", "1")
        .env("PTY_TEST_BOOT_ID", "unavailable")
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = process.try_wait().unwrap() {
            assert!(
                status.success(),
                "unreadable boot identity must dispatch no signal"
            );
            break;
        }
        if Instant::now() >= deadline {
            let _ = process.kill();
            let _ = process.wait();
            panic!("unreadable identity probe timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
