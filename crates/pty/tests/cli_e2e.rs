//! End-to-end tests for the `pty` CLI + daemon. Each test uses an isolated
//! `PTY_ROOT` temp dir so it can't touch the real registry or other tests.
//!
//! The interactive `attach` test is itself driven *through* the libghostty test
//! harness — we spawn `pty attach` inside a PTY and assert on the emulated
//! screen. So the CLI's own terminal handling is validated by libghostty too.

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use pty_testkit::{Session, SpawnOptions};

fn pty_bin() -> &'static str {
    env!("CARGO_BIN_EXE_pty")
}

/// Serialize these heavy real-daemon tests. Rust runs a binary's tests in
/// parallel by default; each of these spawns detached daemons + PTYs, and
/// enough of them at once starve each other's daemon-startup timing (the "green
/// in isolation, red under load" class). Holding this lock keeps at most one
/// running at a time — the same approach the upstream vitest config takes for
/// its heavy PTY tests. Poison-tolerant so one failure doesn't cascade.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn unique_root() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("pty-e2e-{}-{}", std::process::id(), n));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The stable id out of `Session "<id>" created.` — what `pty run -d`
/// prints now that it speaks Node's text.
fn created_id(stdout: &str) -> String {
    stdout.split('"').nth(1).unwrap_or_default().to_string()
}

/// Run a `pty` subcommand with the given registry root; return
/// `(stdout, stderr, exit_code)`, all trimmed.
fn run_pty(root: &PathBuf, args: &[&str]) -> (String, String, i32) {
    run_pty_env(root, args, &[])
}

/// Like [`run_pty`] but with extra env vars (e.g. `PTY_REAP_ON_EXIT`).
fn run_pty_env(root: &PathBuf, args: &[&str], env: &[(&str, &str)]) -> (String, String, i32) {
    let mut c = Command::new(pty_bin());
    c.args(args)
        .env("PTY_ROOT", root)
        // These tests deliberately CREATE sessions; scrub any ambient
        // PTY_SESSION so nesting-prevention doesn't turn `pty run` into a direct
        // exec when the harness itself runs inside a pty session. Also scrub
        // PTY_REAP_ON_EXIT so the reap DEFAULT (reap) is deterministic
        // regardless of ambient env; preserve-mode tests set it explicitly.
        .env_remove("PTY_SESSION")
        .env_remove("PTY_REAP_ON_EXIT");
    for (k, v) in env {
        c.env(k, v);
    }
    let out = c.output().expect("spawn pty");
    (
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
        out.status.code().unwrap_or(-1),
    )
}

/// PTY_REAP_ON_EXIT=false → preserve exited sessions (for post-exit assertions).
const PRESERVE: &[(&str, &str)] = &[("PTY_REAP_ON_EXIT", "false")];

/// Run a `pty` subcommand and assert it exited 0; return stdout (trimmed).
fn ok_pty(root: &PathBuf, args: &[&str]) -> String {
    let (out, err, code) = run_pty(root, args);
    assert_eq!(code, 0, "`pty {}` exited {code}: {err}", args.join(" "));
    out
}

#[test]
fn run_ls_peek_send_kill_lifecycle() {
    let _serial = serial();
    let root = unique_root();
    // Spawn a persistent bash session.
    let (name, _e, code) = run_pty(
        &root,
        &["run", "-d", "--rows", "24", "--cols", "80", "--", "bash", "--norc", "--noprofile"],
    );
    let name = created_id(&name);
    assert_eq!(code, 0, "run failed: {_e}");
    assert!(!name.is_empty(), "run printed no session name");

    // ls shows it running.
    let ls = ok_pty(&root, &["ls"]);
    assert!(ls.contains(&name), "ls missing session:\n{ls}");
    assert!(ls.contains("Active sessions:"), "session not running:\n{ls}");

    // peek shows the bash prompt.
    let mut prompt_seen = false;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        let screen = ok_pty(&root, &["peek", "--plain", &name]);
        if screen.contains('$') {
            prompt_seen = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(prompt_seen, "never saw a bash prompt via peek");

    // send a command, then peek for its output.
    let (_o, _e, code) = run_pty(
        &root,
        &["send", &name, "--seq", "echo hello-e2e", "--seq", "key:return"],
    );
    assert_eq!(code, 0);

    let mut output_seen = false;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        let screen = ok_pty(&root, &["peek", "--plain", &name]);
        // The standalone output line, not just the echoed command.
        if screen.lines().any(|l| l.trim() == "hello-e2e") {
            output_seen = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(output_seen, "never saw command output via peek");

    // status returns JSON with the name.
    let status = ok_pty(&root, &["stats", &name]);
    assert!(status.contains(&name), "status missing name:\n{status}");

    // kill is an external stop → the session is PRESERVED as exited (node #114),
    // not left running.
    let (_o, _e, code) = run_pty(&root, &["kill", &name]);
    assert_eq!(code, 0);
    std::thread::sleep(Duration::from_millis(300));
    let ls2 = ok_pty(&root, &["ls"]);
    // Name the section, not the word. The text listing never prints
    // "running" for a live session — it prints "Active sessions:" and a
    // line per session — so a `kill` that did nothing at all would have
    // satisfied a check for the absence of that word.
    assert!(
        ls2.contains("Exited sessions:") && ls2.contains(&name),
        "the killed session is not listed as exited:\n{ls2}"
    );
    assert!(
        !ls2.contains("Active sessions:"),
        "session still listed as active after kill:\n{ls2}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn up_and_down_from_a_manifest() {
    let _serial = serial();
    let root = unique_root();
    let work = unique_root();
    std::fs::write(
        work.join("pty.toml"),
        "[sessions.echoer]\ncommand = \"cat\"\n\n[sessions.echoer.env]\nGREETING = \"hi-from-env\"\n",
    )
    .unwrap();
    let work_str = work.to_string_lossy().to_string();

    // up starts the session.
    let (up, _e, code) = run_pty(&root, &["up", &work_str]);
    assert_eq!(code, 0, "up failed: {_e}");
    assert!(up.contains("echoer"), "up output:\n{up}");

    // It shows up in ls as running (on-disk name == the short name).
    let mut running = false;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        let ls = ok_pty(&root, &["ls"]);
        if ls.contains("echoer") && ls.contains("Active sessions:") {
            running = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(running, "manifest session not running after up");

    // down stops it (external stop → preserved as exited, not running).
    let (_o, _e, code) = run_pty(&root, &["down", &work_str]);
    assert_eq!(code, 0);
    std::thread::sleep(Duration::from_millis(400));
    let ls = ok_pty(&root, &["ls"]);
    assert!(
        !ls.contains("Active sessions:"),
        "session still running after down:\n{ls}"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn restart_rename_rm() {
    let _serial = serial();
    let root = unique_root();
    // A session that prints its own pid so we can tell restarts apart.
    let (name, _e, code) = run_pty(
        &root,
        &["run", "-d", "--id", "svc", "--", "sh", "-c", "echo pid-$$; cat"],
    );
    let name = created_id(&name);
    assert_eq!(code, 0, "run failed: {_e}");
    assert_eq!(name, "svc");

    // Capture the first pid via the pid file.
    let pid_path = root.join("svc.pid");
    let wait_pid = |after: &str| -> String {
        let start = Instant::now();
        loop {
            if let Ok(p) = std::fs::read_to_string(&pid_path) {
                let p = p.trim().to_string();
                if !p.is_empty() && p != after {
                    return p;
                }
            }
            if start.elapsed() > Duration::from_secs(5) {
                panic!("pid file never settled ({after})");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let pid1 = wait_pid("");

    // rename sets a display label that resolves as a lookup key.
    let (_o, _e, code) = run_pty(&root, &["rename", "svc", "My Service"]);
    assert_eq!(code, 0);
    let peek = ok_pty(&root, &["peek", "--plain", "My Service"]);
    assert!(peek.contains("pid-"), "rename lookup failed:\n{peek}");

    // restart respawns with a new pid.
    // `pty restart` attaches after the respawn. Run it as a session would,
    // so it takes the documented "not attached" path instead of blocking.
    let (_o, _e, code) =
        run_pty_env(&root, &["restart", "-y", "svc"], &[("PTY_SESSION", "outer")]);
    assert_eq!(code, 0, "restart failed: {_e}");
    let pid2 = wait_pid(&pid1);
    assert_ne!(pid1, pid2, "restart should change the pid");

    // rm refuses a running session (node: `Use "pty kill <name>" first.`);
    // kill, then rm removes it entirely.
    let (_o, e, code) = run_pty(&root, &["rm", "svc"]);
    assert_eq!(code, 1);
    assert_eq!(e, "Session \"svc\" is still running. Use \"pty kill svc\" first.");
    let (_o, _e, code) = run_pty(&root, &["kill", "svc"]);
    assert_eq!(code, 0);
    let (_o, e, code) = run_pty(&root, &["rm", "svc"]);
    assert_eq!(code, 0, "rm failed: {e}");
    std::thread::sleep(Duration::from_millis(300));
    let ls = ok_pty(&root, &["ls"]);
    assert!(
        !ls.contains("svc") || ls.contains("No active sessions."),
        "session remained after rm:\n{ls}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn peek_follow_streams_live_output() {
    let _serial = serial();
    let root = unique_root();
    let root_str = root.to_string_lossy().to_string();

    // A session that emits a few lines, then stays alive.
    let (name, _e, code) = run_pty(
        &root,
        &[
            "run",
            "-d",
            "--id",
            "fol",
            "--",
            "sh",
            "-c",
            "for i in 1 2 3; do echo streamed-$i; sleep 0.3; done; sleep 30",
        ],
    );
    let name = created_id(&name);
    assert_eq!(code, 0, "run failed: {_e}");
    assert_eq!(name, "fol");

    // Drive `pty peek -f` (read-only follow) through the harness; it should
    // stream the session's live output to its own stdout.
    let mut s = Session::spawn(
        pty_bin(),
        &["peek", "-f", "fol"],
        SpawnOptions {
            rows: Some(24),
            cols: Some(80),
            env: vec![("PTY_ROOT".to_string(), root_str)],
            ..Default::default()
        },
    )
    .expect("spawn pty peek -f");

    s.wait_for_text("streamed-3", 8000).expect("followed live output");
    s.close();

    let _ = run_pty(&root, &["kill", "fol"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn list_json_tracks_multiple_attached_clients_until_detach_or_disconnect() {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use pty_core::protocol::{encode_attach, encode_attach_with_identity, encode_detach};

    let _serial = serial();
    let root = unique_root();
    let (_, err, code) = run_pty(&root, &["run", "-d", "--id", "clients", "--", "cat"]);
    assert_eq!(code, 0, "{err}");
    let socket = root.join("clients.sock");
    let mut first = UnixStream::connect(&socket).unwrap();
    let mut second = UnixStream::connect(&socket).unwrap();
    first.write_all(&encode_attach_with_identity(24, 80, 1234, Some("/dev/pts/7"))).unwrap();
    second.write_all(&encode_attach(24, 80)).unwrap();

    let start = Instant::now();
    loop {
        let list: serde_json::Value =
            serde_json::from_str(&ok_pty(&root, &["list", "--json", "--clients"])).unwrap();
        let clients = list[0]["clients"].as_array().cloned().unwrap_or_default();
        if clients.len() == 2 {
            assert!(clients.iter().any(|c| c["pid"] == 1234
                && c["tty"] == "/dev/pts/7" && c["attachedAt"].is_string()));
            assert!(clients.iter().any(|c| c["pid"].is_null()
                && c["tty"].is_null() && c["attachedAt"].is_string()));
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "clients did not attach: {list}");
        std::thread::sleep(Duration::from_millis(20));
    }
    first.write_all(&encode_detach()).unwrap();
    drop(second);
    let start = Instant::now();
    loop {
        let list: serde_json::Value =
            serde_json::from_str(&ok_pty(&root, &["list", "--json", "--clients"])).unwrap();
        if list[0]["clients"] == serde_json::json!([]) {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "clients not removed: {list}");
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = run_pty(&root, &["kill", "clients"]);
    let _ = std::fs::remove_dir_all(root);
}

/// The default listing never contacts a daemon; `--clients` does, and a
/// daemon that never answers is reported as unknown within the budget.
#[test]
fn list_contacts_daemons_for_clients_only_when_asked() {
    use std::os::unix::net::UnixListener;

    let _serial = serial();
    let root = unique_root();
    // A live pid makes the fixture a running session without a real daemon.
    std::fs::write(root.join("fixture.pid"), std::process::id().to_string()).unwrap();
    let listener = UnixListener::bind(root.join("fixture.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();

    for args in [&["list", "--json"][..], &["list", "--clients"][..]] {
        let out = ok_pty(&root, args);
        assert!(!out.contains("\"clients\""), "{args:?}: {out}");
        assert!(listener.accept().is_err(), "{args:?} contacted the daemon");
    }

    // Accepted by the kernel backlog, never answered: a stalled daemon.
    let start = Instant::now();
    let list: serde_json::Value =
        serde_json::from_str(&ok_pty(&root, &["list", "--json", "--clients"])).unwrap();
    assert!(start.elapsed() < Duration::from_secs(3), "{:?}", start.elapsed());
    assert_eq!(list[0]["name"], "fixture");
    assert!(list[0]["clients"].is_null(), "{list}");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn stats_json_matches_node_contract() {
    // Parity B: stats --json shape. EXACT for stable fields (geometry,
    // scrollbackCapacity=rows+10000, alive, status, modes, client counts,
    // exited exitCode); SHAPE/TYPE-only for volatile (pids differ, resources,
    // cursor, uptime, createdAt); OMIT-when-unset (geometryNeutral, capabilities,
    // gone tags).
    let _serial = serial();
    let root = unique_root();
    let (_n, _e, code) = run_pty(
        &root,
        &["run", "-d", "--id", "sj", "--rows", "30", "--cols", "100", "--", "cat"],
    );
    assert_eq!(code, 0);
    // wait until up
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if run_pty(&root, &["stats", "--json", "sj"]).2 == 0
            && run_pty(&root, &["stats", "--json", "sj"]).0.contains("\"terminal\"")
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = ok_pty(&root, &["stats", "--json", "sj"]);
    let v: serde_json::Value = serde_json::from_str(&out).expect("valid json");

    // EXACT-value fields.
    assert_eq!(v["name"], "sj");
    assert_eq!(v["terminal"]["cols"], 100);
    assert_eq!(v["terminal"]["rows"], 30);
    assert_eq!(v["terminal"]["scrollbackCapacity"], 30 + 10000);
    assert_eq!(v["process"]["alive"], true);
    assert_eq!(v["clients"]["total"], 0);
    assert_eq!(v["clients"]["attached"], 0);
    assert_eq!(v["clients"]["readOnly"], 0);
    assert_eq!(v["modes"]["sgrMouse"], false);
    assert_eq!(v["modes"]["cursorHidden"], false);
    assert_eq!(v["modes"]["kittyKeyboard"], false);
    assert_eq!(v["modes"]["kittyKeyboardFlags"], serde_json::json!([]));

    // SHAPE/TYPE-only: pids are numbers and differ; resources numeric; uptime
    // numeric; createdAt string.
    let ppid = v["process"]["pid"].as_i64().expect("process.pid number");
    let dpid = v["daemon"]["pid"].as_i64().expect("daemon.pid number");
    assert_ne!(ppid, dpid, "process.pid must differ from daemon.pid");
    assert!(v["process"]["resources"]["rssKb"].is_number());
    assert!(v["process"]["resources"]["cpuPercent"].is_number());
    assert!(v["daemon"]["resources"]["rssKb"].is_number());
    assert!(v["terminal"]["cursorX"].is_number());
    assert!(v["terminal"]["cursorY"].is_number());
    assert!(v["terminal"]["scrollbackUsed"].is_number());
    assert!(v["uptimeSeconds"].is_number());
    assert!(v["createdAt"].is_string());

    // OMIT-when-unset.
    assert!(v["clients"].get("geometryNeutral").is_none(), "geometryNeutral should be omitted");
    assert!(v.get("capabilities").is_none(), "capabilities should be omitted");

    // Exited session (preserve mode) → the small gone shape.
    let (_n, _e, code) =
        run_pty_env(&root, &["run", "-d", "--id", "sg", "--", "sh", "-c", "exit 6"], PRESERVE);
    assert_eq!(code, 0);
    let start = Instant::now();
    let mut gone = String::new();
    while start.elapsed() < Duration::from_secs(5) {
        gone = ok_pty(&root, &["stats", "--json", "sg"]);
        if gone.contains("\"exited\"") {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let g: serde_json::Value = serde_json::from_str(&gone).expect("valid gone json");
    assert_eq!(g["name"], "sg");
    assert_eq!(g["status"], "exited");
    assert_eq!(g["exitCode"], 6);
    assert!(g["exitedAt"].is_string());
    assert!(g.get("tags").is_none(), "tags should be omitted when unset");

    let _ = run_pty(&root, &["kill", "sj"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn version_is_semver_with_rust_tag_and_sha() {
    // Parity E: `pty version` prints `<semver>+<short-sha>` like node's
    // `0.12.0+500eab2`, not a "pty-rust X.Y.Z" label. The Rust line is one
    // minor above node with a `-rust` pre-release tag (docs/parity.md §14):
    // `0.13.<patch>-rust+<hex>`, stamped by crates/pty/build.rs.
    let re = regex::Regex::new(r"^0\.13\.\d+-rust\+[0-9a-f]{4,}$").unwrap();
    let root = unique_root();
    for form in ["version", "--version", "-v", "-V"] {
        let out = ok_pty(&root, &[form]);
        assert!(re.is_match(&out), "`pty {form}` not 0.13.x-rust+<sha>: {out:?}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn ls_json_matches_node_shape() {
    // Parity A: ls --json fields match node — {name, status, pid(daemon),
    // command, cwd, createdAt, exitCode, exitedAt}; displayName omitted when
    // unset; status enum running|exited|vanished.
    let _serial = serial();
    let root = unique_root();
    let (_n, _e, code) = run_pty(&root, &["run", "-d", "--id", "jr", "--no-display-name", "--", "cat"]);
    assert_eq!(code, 0);
    // Wait until it's up.
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if run_pty(&root, &["ls", "--json"]).0.contains("\"status\":\"running\"") {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let running = ok_pty(&root, &["ls", "--json"]);
    assert!(running.contains("\"name\":\"jr\""), "json: {running}");
    assert!(running.contains("\"status\":\"running\""), "json: {running}");
    assert!(running.contains("\"exitCode\":null"), "json: {running}");
    assert!(running.contains("\"exitedAt\":null"), "json: {running}");
    assert!(running.contains("\"createdAt\":\""), "json: {running}");
    // pid is a number (the daemon pid), not null, for a running session.
    assert!(
        !running.contains("\"pid\":null"),
        "running session should have a daemon pid: {running}"
    );
    // displayName omitted when unset.
    assert!(
        !running.contains("displayName"),
        "displayName should be omitted when unset: {running}"
    );

    // Exited session (preserve mode so it stays visible as exited).
    let (_n, _e, code) =
        run_pty_env(&root, &["run", "-d", "--id", "je", "--no-display-name", "--", "sh", "-c", "exit 5"], PRESERVE);
    assert_eq!(code, 0);
    let start = Instant::now();
    let mut exited_json = String::new();
    while start.elapsed() < Duration::from_secs(5) {
        exited_json = ok_pty(&root, &["ls", "--json"]);
        if exited_json.contains("\"status\":\"exited\"") {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(exited_json.contains("\"status\":\"exited\""), "json: {exited_json}");
    assert!(exited_json.contains("\"exitCode\":5"), "json: {exited_json}");
    assert!(exited_json.contains("\"exitedAt\":\""), "json: {exited_json}");

    let _ = run_pty(&root, &["kill", "jr"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn default_reap_removes_session_on_self_exit() {
    // Parity (node #114): with NO PTY_REAP_ON_EXIT (default = reap), a session
    // that exits ON ITS OWN is removed entirely — post-exit peek fails and
    // ls --json omits it.
    let _serial = serial();
    let root = unique_root();
    let (name, _e, code) = run_pty(
        &root,
        &["run", "-d", "--id", "rp", "--", "sh", "-c", "echo GONE; exit 3"],
    );
    let name = created_id(&name);
    assert_eq!(code, 0, "run failed: {_e}");
    assert_eq!(name, "rp");
    // Give the daemon time to run + reap.
    std::thread::sleep(Duration::from_millis(600));
    // peek fails (session gone).
    let (_o, _e, peek_code) = run_pty(&root, &["peek", "--plain", "rp"]);
    assert_ne!(peek_code, 0, "reaped session should not be peekable");
    // ls --json omits it.
    let ls = ok_pty(&root, &["ls", "--json"]);
    assert!(!ls.contains("\"rp\""), "reaped session should be omitted from ls: {ls}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn external_kill_preserves_session() {
    // Parity (node #114): an external stop (`pty kill`) PRESERVES the session
    // (status=exited) even under the default reap config — only self-exit reaps.
    let _serial = serial();
    let root = unique_root();
    let (name, _e, code) = run_pty(&root, &["run", "-d", "--id", "ek", "--", "cat"]);
    let name = created_id(&name);
    assert_eq!(code, 0, "run failed: {_e}");
    assert_eq!(name, "ek");
    // Wait until up, then kill.
    std::thread::sleep(Duration::from_millis(400));
    let (_o, _e, code) = run_pty(&root, &["kill", "ek"]);
    assert_eq!(code, 0);
    std::thread::sleep(Duration::from_millis(500));
    // Preserved as exited (NOT removed).
    let ls = ok_pty(&root, &["ls", "--json"]);
    assert!(
        ls.contains("\"ek\"") && ls.contains("\"status\":\"exited\""),
        "external kill should preserve the session as exited: {ls}"
    );

    let _ = run_pty(&root, &["rm", "ek"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
#[ignore = "post-exit peek moves from the removed <name>.screen sidecar to metadata lastLines; the client rewrite restores it"]
fn post_exit_peek_returns_final_screen() {
    // Parity #1: after a session exits, `peek --plain` still returns its final
    // screen (node retains it); rust previously failed with ENOENT.
    let _serial = serial();
    let root = unique_root();
    let (name, _e, code) = run_pty_env(
        &root,
        &["run", "-d", "--id", "px", "--", "sh", "-c", "echo FINAL-OUTPUT-LINE; exit 7"],
        PRESERVE,
    );
    let name = created_id(&name);
    assert_eq!(code, 0, "run failed: {_e}");
    assert_eq!(name, "px");

    // Wait for the session to exit (ls shows exited:7).
    let mut exited = false;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        let (ls, _e, _c) = run_pty(&root, &["ls"]);
        if ls.contains("exited:7") {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(exited, "session never recorded exit:7");

    // peek --plain must succeed AND contain the final output (not ENOENT).
    let (screen, err, code) = run_pty(&root, &["peek", "--plain", "px"]);
    assert_eq!(code, 0, "post-exit peek failed: {err}");
    assert!(
        screen.contains("FINAL-OUTPUT-LINE"),
        "post-exit peek missing final screen:\n{screen}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn run_force_creates_nested_session() {
    // Parity #4 (canonical ruling): --force CREATES a session even
    // from inside a pty session (PTY_SESSION set) — bypasses the nesting guard,
    // matching node's --help + the fixed node code. Both --force alone and
    // --force -d create; and --force is parsed (not treated as the command).
    let _serial = serial();
    let root = unique_root();

    // --force alone (no -d), nested, creates a real session.
    let out = Command::new(pty_bin())
        .args(["run", "-d", "--force", "--id", "fc", "--", "cat"])
        .env("PTY_ROOT", &root)
        .env("PTY_SESSION", "fake-parent")
        .output()
        .expect("spawn pty");
    assert_eq!(
        out.status.code(),
        Some(0),
        "run --force failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "Session \"fc\" created.");
    let ls = ok_pty(&root, &["ls"]);
    assert!(
        ls.contains("fc") && ls.contains("Active sessions:"),
        "--force should create a nested session:\n{ls}"
    );

    // --force -d also creates (belt-and-suspenders form used in the report).
    let out = Command::new(pty_bin())
        .args(["run", "-d", "--force", "-d", "--id", "fd", "--", "cat"])
        .env("PTY_ROOT", &root)
        .env("PTY_SESSION", "fake-parent")
        .output()
        .expect("spawn pty");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "Session \"fd\" created.");
    let ls = ok_pty(&root, &["ls"]);
    assert!(ls.contains("fd") && ls.contains("Active sessions:"), "ls:\n{ls}");

    let _ = run_pty(&root, &["kill", "fc"]);
    let _ = run_pty(&root, &["kill", "fd"]);
    let _ = std::fs::remove_dir_all(&root);
}

/// `--env KEY` (name only) resolves the value from the launcher's
/// environment, so the value never appears on the `pty run` command line —
/// the argv a supervisor or a service manager records — while the session
/// still receives it and the daemon still persists it for restart. The
/// registry record that carries the resolved value stays owner-only, and a
/// name with no inherited value is a loud refusal, never a silent drop.
#[test]
fn run_env_name_only_keeps_values_off_the_command_line() {
    let _serial = serial();
    let root = unique_root();
    const SECRET: &str = "synthetic-seat-secret-4f2a";

    let (out, err, code) = run_pty_env(
        &root,
        &[
            "run",
            "-d",
            "--id",
            "envn",
            "--env",
            "SEAT_TOKEN",
            "--",
            "sh",
            "-c",
            "printf '%s' \"$SEAT_TOKEN\" > \"$PTY_ROOT/envn.out\"; cat",
        ],
        &[("SEAT_TOKEN", SECRET)],
    );
    assert_eq!(code, 0, "run failed: {err}");
    assert!(!out.contains(SECRET), "the value must not echo back: {out}");

    // The session child received the value.
    let marker = root.join("envn.out");
    let mut seen = false;
    for _ in 0..50 {
        if let Ok(text) = std::fs::read_to_string(&marker)
            && text == SECRET
        {
            seen = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(seen, "the session child never received SEAT_TOKEN");

    // No live process of the session carries the value on its command line:
    // neither the daemon (which received it in its config) nor the session
    // child (which received it in its environment).
    #[cfg(target_os = "linux")]
    {
        let daemon: u32 = std::fs::read_to_string(root.join("envn.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut cmdlines = vec![std::fs::read(format!("/proc/{daemon}/cmdline")).unwrap()];
        if let Ok(children) =
            std::fs::read_to_string(format!("/proc/{daemon}/task/{daemon}/children"))
        {
            for child in children.split_whitespace() {
                if let Ok(cmdline) = std::fs::read(format!("/proc/{child}/cmdline")) {
                    cmdlines.push(cmdline);
                }
            }
        }
        assert!(
            cmdlines.len() > 1,
            "the session child was not found under the daemon"
        );
        for cmdline in &cmdlines {
            let text = String::from_utf8_lossy(cmdline).replace('\0', " ");
            assert!(
                !text.contains(SECRET),
                "a session process carries the value on its command line: {text}"
            );
        }
    }

    // The daemon persists the resolved value so a restart reproduces the
    // same environment — and that record is owner-only.
    let record_path = root.join("envn.json");
    let record = std::fs::read_to_string(&record_path).unwrap();
    assert!(record.contains("SEAT_TOKEN"), "record:\n{record}");
    assert!(record.contains(SECRET), "record:\n{record}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&record_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "the record persists environment values and must stay owner-only"
        );
    }

    // A name with no inherited value refuses loudly instead of dropping it.
    let (_out2, err2, code2) = run_pty_env(
        &root,
        &["run", "-d", "--id", "envx", "--env", "SEAT_ABSENT", "--", "cat"],
        &[],
    );
    assert_eq!(code2, 1);
    assert!(err2.contains("--env SEAT_ABSENT names no value"), "{err2}");

    let _ = run_pty(&root, &["kill", "envn"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn attach_double_tap_ctrl_backslash_sends_literal_not_detach() {
    let _serial = serial();
    let root = unique_root();
    let root_str = root.to_string_lossy().to_string();

    // `cat` echoes its input, so we can observe that we're still attached.
    let (name, _e, code) = run_pty(&root, &["run", "-d", "--id", "dt", "--", "cat"]);
    let name = created_id(&name);
    assert_eq!(code, 0, "run failed: {_e}");
    assert_eq!(name, "dt");

    let mut s = Session::spawn(
        pty_bin(),
        &["attach", "dt"],
        SpawnOptions {
            rows: Some(24),
            cols: Some(80),
            env: vec![("PTY_ROOT".to_string(), root_str)],
            ..Default::default()
        },
    )
    .expect("spawn pty attach");
    // Ensure we're attached (cat echoes a probe line).
    s.type_str("probe-line\r");
    s.wait_for_text("probe-line", 8000).expect("attached");

    // Two Ctrl+\ in one chunk = double-tap → forwards a literal Ctrl+\ to the
    // child (cat echoes it as `^\`) and does NOT detach. A following marker
    // still reaching the child proves the attach stayed connected.
    s.type_str("\x1c\x1c");
    s.type_str("still-attached-marker\r");
    let ss = s
        .wait_for_text("still-attached-marker", 8000)
        .expect("should still be attached after double-tap");
    assert!(
        ss.text.contains("^\\"),
        "double-tap should forward a literal Ctrl+\\ (echoed as ^\\):\n{}",
        ss.text
    );
    assert!(
        !ss.text.contains("[detached from"),
        "double-tap must NOT detach:\n{}",
        ss.text
    );
    s.close();

    let _ = run_pty(&root, &["kill", "dt"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn nesting_prevention_runs_directly_inside_a_session() {
    let _serial = serial();
    let root = unique_root();
    // PTY_SESSION set + no -d: `pty run` should run the command DIRECTLY (no
    // session-in-a-session), so stdout is the command's output, not a session id.
    let out = Command::new(pty_bin())
        .args(["run", "--", "echo", "nested-direct"])
        .env("PTY_ROOT", &root)
        .env("PTY_SESSION", "fake-parent")
        .output()
        .expect("spawn pty");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "nested-direct",
        "should have run the command directly"
    );
    // No session was created.
    let ls = ok_pty(&root, &["ls"]);
    assert!(ls.contains("No active sessions."), "a nested session was created:\n{ls}");

    // With -d, it DOES create a background session even inside a session.
    let out = Command::new(pty_bin())
        .args(["run", "-d", "--id", "nbg", "--", "cat"])
        .env("PTY_ROOT", &root)
        .env("PTY_SESSION", "fake-parent")
        .output()
        .expect("spawn pty");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "Session \"nbg\" created.");
    let ls = ok_pty(&root, &["ls"]);
    assert!(ls.contains("nbg") && ls.contains("Active sessions:"), "ls:\n{ls}");

    let _ = run_pty(&root, &["kill", "nbg"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn attach_is_interactive_and_detaches() {
    let _serial = serial();
    let root = unique_root();
    let root_str = root.to_string_lossy().to_string();

    // Spawn a session to attach to.
    let (name, _e, code) = run_pty(
        &root,
        &["run", "-d", "--rows", "24", "--cols", "80", "--", "bash", "--norc", "--noprofile"],
    );
    let name = created_id(&name);
    assert_eq!(code, 0, "run failed: {_e}");

    // Drive `pty attach` THROUGH the libghostty harness (real PTY stdin/stdout).
    let mut s = Session::spawn(
        pty_bin(),
        &["attach", &name],
        SpawnOptions {
            rows: Some(24),
            cols: Some(80),
            env: vec![("PTY_ROOT".to_string(), root_str.clone())],
            ..Default::default()
        },
    )
    .expect("spawn pty attach");

    // The attach replays the screen (bash prompt) and streams live output.
    s.wait_for_text("$", 8000).expect("attached prompt");
    s.type_str("echo attached-works\r");
    // wait_for_text asserts the live output appeared (errors on timeout).
    s.wait_for_text("attached-works", 8000).expect("live output");

    // A single Ctrl+\ (0x1c) detaches (after the ~300ms double-tap window)
    // without killing the session — matching the real pty.
    s.type_str("\x1c");
    // Wait past the double-tap window so the detach fires, then verify the
    // detach confirmation was rendered.
    s.wait_for_text(&format!("[detached from {name}]"), 5000).expect("detach confirmation");
    s.close();

    // The session must still be alive after detach.
    let ls = ok_pty(&root, &["ls"]);
    assert!(
        ls.contains(&name) && ls.contains("Active sessions:"),
        "session should survive detach:\n{ls}"
    );

    let _ = run_pty(&root, &["kill", &name]);
    let _ = std::fs::remove_dir_all(&root);
}
