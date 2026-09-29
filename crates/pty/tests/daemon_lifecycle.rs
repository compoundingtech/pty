//! Publication, exit evidence, reap vs preserve, external kills, the
//! shutdown backstop, the spawner watchdog, the child environment, and the
//! spawn path (`pty run`) with its readiness rule.
//!
//! node: tests/exit-signal.test.ts, tests/exit-event-race.test.ts,
//! tests/shutdown-backstop.test.ts, tests/spawner-pid-watchdog.test.ts,
//! bin/pty-kill-releases-socket-test, tests/process-title.test.ts,
//! tests/restart-launch-parity.test.ts, tests/exit-reap.test.ts

mod daemon_support;

use std::time::{Duration, Instant};

use daemon_support::*;
use pty_core::protocol::{
    LifecycleCompareAndSetRequest, LifecycleCompareAndSetResult, MessageType::*,
    encode_lifecycle_compare_and_set_request,
};
use serde_json::json;

const T: Duration = Duration::from_secs(8);
const PRESERVE: &[(&str, &str)] = &[("PTY_REAP_ON_EXIT", "false")];

fn wait_exited(d: &Daemon) -> serde_json::Value {
    assert!(
        wait_until(T, || d.meta().map(|m| m["exitedAt"].is_string()).unwrap_or(false)),
        "session {} never recorded an exit",
        d.name
    );
    d.meta().unwrap()
}

/// node: src/server.ts:630-682; tests/events.test.ts:525-543
#[test]
fn publication_order_and_shapes() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let name = unique_name("pub");
    // A stale events file is truncated at start.
    std::fs::write(
        root.join(format!("{name}.events.jsonl")),
        "{\"session\":\"x\",\"type\":\"bell\",\"ts\":\"2020-01-01T00:00:00.000Z\"}\n",
    )
    .unwrap();
    let mut cfg = config(&name, "sleep", &["30"]);
    cfg["tags"] = json!({"team": "a", "keep": "true"});
    cfg["displayName"] = json!("Sleeper");
    cfg["ephemeral"] = json!(false);
    let d = Daemon::start(&root, cfg);
    assert!(wait_until(T, || !d.events("session_start").is_empty()));

    use std::os::unix::fs::PermissionsExt;
    let sock = std::fs::metadata(d.socket_path()).unwrap();
    assert_eq!(sock.permissions().mode() & 0o777, 0o600);
    let pid: i32 = std::fs::read_to_string(root.join(format!("{name}.pid")))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(pid, d.pid);

    let m = d.meta().unwrap();
    let keys: Vec<&str> = m.as_object().unwrap().keys().map(String::as_str).collect();
    assert!(
        keys == ["generation", "daemonPid", "command", "args", "displayCommand", "cwd", "rows",
                 "cols", "ephemeral", "createdAt", "tags", "displayName"]
            || keys == ["generation", "daemonPid", "daemonStartToken", "command", "args",
                        "displayCommand", "cwd", "rows", "cols", "ephemeral", "createdAt", "tags",
                        "displayName"],
        "unexpected metadata publication shape: {keys:?}"
    );
    assert_eq!(m["generation"].as_str().unwrap().len(), 32);
    assert_eq!(m["daemonPid"], d.pid);
    assert_eq!(m["ephemeral"], false);
    assert_eq!(m["tags"], json!({"team": "a", "keep": "true"}));

    let events = read_events(&root, &name);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["type"], "session_start");
    assert_eq!(events[0]["session"], name);
    assert_eq!(events[0]["tags"], json!({"team": "a", "keep": "true"}));
    assert!(events[0]["ts"].as_str().unwrap() >= m["createdAt"].as_str().unwrap());
    // Linux names its daemon; macOS does not yet. Pinned both ways rather
    // than skipped, so implementing it there turns this green instead of
    // leaving a gap nobody is watching. See `set_process_title`.
    let named = process_name(d.pid);
    if cfg!(target_os = "linux") {
        assert_eq!(named, Some("pty-daemon".to_string()));
    } else {
        assert_ne!(
            named,
            Some("pty-daemon".to_string()),
            "the daemon is named here now — implement it and make this the \
             assertion for every platform"
        );
    }
}

/// The owner record is published before the child starts.
///
/// compoundingtech/pty#180: the daemon spawned its child before publishing,
/// so an attached child's first action could run with `<name>.pid`
/// unpublished and the metadata missing. The child's first action here is a
/// marker file; the moment it appears, the owner sidecar, the metadata, and
/// the `session_start` line the spawner waits for must already be on disk.
#[test]
fn owner_record_is_published_before_the_child_starts() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let name = unique_name("pubfirst");
    let marker = root.join(format!("{name}.started"));
    let cfg = config(
        &name,
        "sh",
        &[
            "-c",
            &format!("touch '{}'; exec sleep 30", marker.display()),
        ],
    );
    // Spawn without waiting: the child's marker is the readiness signal.
    let d = Daemon::spawn(&root, cfg, &[]);
    assert!(
        wait_until(T, || marker.exists()),
        "child of session {name} never started"
    );
    let pid: i32 = std::fs::read_to_string(root.join(format!("{name}.pid")))
        .expect("owner sidecar published before the child starts")
        .trim()
        .parse()
        .unwrap();
    assert_eq!(pid, d.pid);
    let m = d
        .meta()
        .expect("metadata published before the child starts");
    assert_eq!(m["daemonPid"], d.pid);
    assert!(
        !d.events("session_start").is_empty(),
        "session_start published before the child starts"
    );
}

/// A complete owner record is not launcher readiness: starting the PTY child
/// can still fail after publication. The daemon must close its readiness pipe
/// without acknowledging success in that case.
#[test]
fn failed_child_spawn_never_signals_launcher_ready() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let name = unique_name("spawnfail");
    let cfg = config(&name, "/bin/sh", &["\0"]);

    let mut fds = [0i32; 2];
    // SAFETY: pipe(2) fills two new descriptors owned by this test.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let (read_fd, write_fd) = (fds[0], fds[1]);
    // SAFETY: this is the sole owner of the read descriptor.
    let mut ready = unsafe {
        use std::os::fd::FromRawFd;
        std::fs::File::from_raw_fd(read_fd)
    };

    let mut child = std::process::Command::new(pty_bin())
        .arg("__daemon")
        .env("PTY_ROOT", &root)
        .env("PTY_SERVER_CONFIG", cfg.to_string())
        .env("PTY_DAEMON_READY_FD", write_fd.to_string())
        .env_remove("PTY_SESSION")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn daemon");
    // SAFETY: the child inherited its copy and this process no longer writes.
    unsafe {
        libc::close(write_fd);
    }
    let status = child.wait().expect("failed daemon exits");
    assert!(!status.success());

    let mut signal = Vec::new();
    use std::io::Read;
    ready.read_to_end(&mut signal).unwrap();
    assert!(signal.is_empty(), "failed child spawn reported launcher readiness");
    assert!(read_meta(&root, &name).is_some(), "pre-spawn metadata was published");
    assert_eq!(events_of_type(&root, &name, "session_start").len(), 1);
}

/// What `ps` and `top` call a process, read the way each machine offers it.
///
/// **This used to read `/proc` and nothing else, so on a Mac it panicked with
/// a bare "No such file or directory" — and gating it there would have hidden
/// a real gap rather than found one.** The daemon was not named on macOS at
/// all: `ps -o comm=` gave the whole path of the binary where the Node tool
/// gives `pty-daemon`. Measured 2026-09-02, which is when the macOS branch of
/// `set_process_title` came to exist.
///
/// `None` when the machine will not say, which is a different answer from a
/// name that is wrong.
fn process_name(pid: i32) -> Option<String> {
    if cfg!(target_os = "linux") {
        return std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|s| s.trim().to_string());
    }
    let out = std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() {
        return None;
    }
    // `ps -o comm=` prints a path on some machines and a bare name on
    // others; the question is what the process is CALLED either way.
    Some(
        name.rsplit('/')
            .next()
            .unwrap_or(&name)
            .to_string(),
    )
}

/// node: tests/exit-signal.test.ts:49-71
#[test]
fn a_sigkilled_child_is_recorded_as_137_with_signal_9() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let d = Daemon::start_env(
        &root,
        config(&unique_name("sig"), "sh", &["-c", "exec sleep 300"]),
        PRESERVE,
    );
    let leaf = d.child_pid();
    unsafe {
        libc::kill(leaf, libc::SIGKILL);
    }
    let meta = wait_exited(&d);
    assert_eq!(meta["exitCode"], 137);
    let exits = d.events("session_exit");
    assert_eq!(exits.len(), 1);
    assert_eq!(exits[0]["exitCode"], 137);
    assert_eq!(exits[0]["signal"], 9);
}

/// node: tests/exit-signal.test.ts:74-87; tests/integration.test.ts:1252-1269
#[test]
fn a_clean_exit_keeps_the_raw_code_and_last_lines() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let d = Daemon::start_env(
        &root,
        config(&unique_name("sig"), "sh", &["-c", "echo one; echo two; exit 5"]),
        PRESERVE,
    );
    let meta = wait_exited(&d);
    assert_eq!(meta["exitCode"], 5);
    assert!(meta.get("signal").is_none());
    assert_eq!(meta["lastLines"], json!(["one", "two"]));
    let exits = d.events("session_exit");
    assert_eq!(exits.len(), 1);
    assert_eq!(exits[0]["exitCode"], 5);
    assert!(exits[0].get("signal").is_none());
    // The exit write appends these four, in this order. `lastOutputAtMs` is
    // last because the child printed and exited inside the one-second
    // debounce, so the exit write carried the stamp rather than a timer.
    let keys: Vec<&str> = meta.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        &keys[keys.len() - 4..],
        ["exitCode", "exitedAt", "lastLines", "lastOutputAtMs"]
    );
}

/// node: tests/exit-event-race.test.ts:82-136
#[test]
fn exactly_one_session_exit_and_start_after_a_natural_exit() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let mut cfg = config(&unique_name("race"), "true", &[]);
    cfg["tags"] = json!({"keep": "true"});
    let mut d = Daemon::spawn(&root, cfg, &[]);
    let code = d.wait_exit(T).expect("daemon exited");
    assert_eq!(code, 0);
    assert_eq!(d.events("session_exit").len(), 1);
    assert!(d.events("session_exit")[0]["exitCode"].is_number());
    assert_eq!(d.events("session_start").len(), 1);
    // keep=true preserves everything but the socket and pid.
    assert!(d.meta().is_some());
    assert!(!d.socket_path().exists());
    assert!(!root.join(format!("{}.pid", d.name)).exists());
}

/// node: tests/exit-event-race.test.ts:108-125
#[test]
fn sigterm_records_one_session_exit_and_preserves_the_session() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let mut d = Daemon::start(&root, config(&unique_name("race"), "/bin/sh", &["-c", "sleep 30"]));
    std::thread::sleep(Duration::from_millis(200));
    let child = d.child_pid();
    d.signal(libc::SIGTERM);
    let code = d.wait_exit(T).expect("daemon exited");
    assert_eq!(code, 0);
    assert_eq!(d.events("session_exit").len(), 1);
    assert!(wait_dead(child, Duration::from_secs(1)));
    let m = d.meta().expect("metadata preserved on an external kill");
    assert_eq!(m["exitCode"], 129);
    assert!(!d.socket_path().exists());
}

/// node: tests/exit-reap.test.ts:670-932
#[test]
fn reap_and_preserve_decisions() {
    skip_without_a_real_machine!();
    let _s = serial();
    // Default: reaped on exit (no files left).
    let root = short_root();
    let mut d = Daemon::spawn(&root, config(&unique_name("reap"), "sh", &["-c", "exit 3"]), &[]);
    assert_eq!(d.wait_exit(T), Some(3));
    let left: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().map(|e| e.file_name()).collect();
    assert!(left.is_empty(), "{left:?}");

    // PTY_REAP_ON_EXIT=false preserves.
    let root = short_root();
    let mut d = Daemon::spawn(&root, config(&unique_name("reap"), "sh", &["-c", "exit 3"]), PRESERVE);
    assert_eq!(d.wait_exit(T), Some(3));
    assert_eq!(d.meta().unwrap()["exitCode"], 3);
    assert!(root.join(format!("{}.events.jsonl", d.name)).exists());

    // ephemeral reaps under preserve, even on an external kill.
    let root = short_root();
    let mut cfg = config(&unique_name("reap"), "sleep", &["30"]);
    cfg["ephemeral"] = json!(true);
    let mut d = Daemon::start_env(&root, cfg, PRESERVE);
    d.signal(libc::SIGTERM);
    assert_eq!(d.wait_exit(T), Some(0));
    assert!(d.meta().is_none());

    // keep applied while running preserves; a `keep=false` reaps.
    let root = short_root();
    let d0 = Daemon::start(&root, config(&unique_name("reap"), "sh", &["-c", "sleep 0.5; exit 0"]));
    let mut m = d0.meta().unwrap();
    m["tags"] = json!({"keep": "yes"});
    std::fs::write(root.join(format!("{}.json", d0.name)), serde_json::to_string_pretty(&m).unwrap()).unwrap();
    let mut d = d0;
    assert_eq!(d.wait_exit(T), Some(0));
    assert_eq!(d.meta().unwrap()["exitCode"], 0);

    // A replacement's generation on disk means: not ours to reap.
    let root = short_root();
    let d0 = Daemon::start(&root, config(&unique_name("reap"), "sh", &["-c", "sleep 0.5; exit 0"]));
    let mut m = d0.meta().unwrap();
    m["generation"] = json!("ffffffffffffffffffffffffffffffff");
    std::fs::write(root.join(format!("{}.json", d0.name)), serde_json::to_string_pretty(&m).unwrap()).unwrap();
    let mut d = d0;
    assert_eq!(d.wait_exit(T), Some(0));
    assert!(d.meta().is_some());
    assert!(d.meta().unwrap().get("exitCode").is_none(), "foreign generation must not be touched");
}

/// node: tests/shutdown-backstop.test.ts:80-122
#[test]
fn shutdown_backstop_force_exits_and_reaps_a_frozen_child() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let frozen = script(&root, "frozen.sh", "#!/bin/bash\ntrap '' HUP TERM\nwhile :; do sleep 1; done\n");
    let mut d = Daemon::start_env(
        &root,
        config(&unique_name("back"), frozen.to_str().unwrap(), &[]),
        &[("PTY_SHUTDOWN_DEADLINE_MS", "300")],
    );
    std::thread::sleep(Duration::from_millis(200));
    let child = d.child_pid();
    let started = Instant::now();
    d.signal(libc::SIGTERM);
    assert!(d.wait_exit(Duration::from_secs(4)).is_some(), "daemon still alive");
    assert!(wait_dead(child, Duration::from_secs(4)), "child still alive");
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(!d.socket_path().exists());

    // With the default deadline a SIGTERM still finishes within 3 s.
    let root = short_root();
    let frozen = script(&root, "frozen.sh", "#!/bin/bash\ntrap '' HUP TERM\nwhile :; do sleep 1; done\n");
    let mut d = Daemon::start(&root, config(&unique_name("back"), frozen.to_str().unwrap(), &[]));
    std::thread::sleep(Duration::from_millis(200));
    let child = d.child_pid();
    let started = Instant::now();
    d.signal(libc::SIGTERM);
    assert!(d.wait_exit(Duration::from_secs(3)).is_some(), "daemon still alive");
    assert!(wait_dead(child, Duration::from_secs(1)));
    assert!(started.elapsed() < Duration::from_secs(3));
}

/// node: tests/spawner-pid-watchdog.test.ts:93-193
#[test]
fn spawner_pid_watchdog() {
    skip_without_a_real_machine!();
    let _s = serial();
    // The spawner dies → the daemon shuts down within the 5 s poll.
    let mut spawner = std::process::Command::new("sleep")
        .arg("1000")
        .spawn()
        .unwrap();
    let root = short_root();
    let mut d = Daemon::start_env(
        &root,
        config(&unique_name("wd"), "sleep", &["3600"]),
        &[("PTY_SPAWNER_PID", &spawner.id().to_string())],
    );
    assert!(d.alive());
    spawner.kill().unwrap();
    spawner.wait().unwrap();
    assert!(d.wait_exit(Duration::from_secs(12)).is_some(), "daemon outlived its spawner");
    // An external stop: metadata is kept.
    assert!(d.meta().is_some());

    // Already dead at boot → exits before serving.
    let dead = std::process::Command::new("true").spawn().unwrap().wait_with_output().unwrap();
    let _ = dead;
    let dead_pid = {
        let mut c = std::process::Command::new("true").spawn().unwrap();
        let pid = c.id();
        c.wait().unwrap();
        pid
    };
    let root = short_root();
    let mut d = Daemon::spawn(
        &root,
        config(&unique_name("wd"), "sleep", &["3600"]),
        &[("PTY_SPAWNER_PID", &dead_pid.to_string())],
    );
    assert!(d.wait_exit(Duration::from_secs(8)).is_some());

    // An invalid value disables the watchdog.
    let root = short_root();
    let mut d = Daemon::start_env(
        &root,
        config(&unique_name("wd"), "sleep", &["3600"]),
        &[("PTY_SPAWNER_PID", "not-a-pid")],
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(d.alive());
}

/// node: tests/restart-launch-parity.test.ts:106-189; tests/nesting.test.ts:106-117;
/// tests/exec.test.ts:91-98
#[test]
fn child_environment_policy() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let out = root.join("env.txt");
    let dump = format!("env > {}; sleep 30", out.display());
    let mut cfg = config(&unique_name("env"), "sh", &["-c", &dump]);
    cfg["cwd"] = json!(root.to_string_lossy());
    cfg["unsetEnv"] = json!(["NO_COLOR", "ASSIGNMENT_WINS"]);
    cfg["extraEnv"] = json!({"ASSIGNMENT_WINS": "explicit", "PTY_SESSION": "spoof"});
    let d = Daemon::start_env(
        &root,
        cfg,
        &[("NO_COLOR", "1"), ("SECRET_TOKEN", "s3"), ("TERM", ""), ("ASSIGNMENT_WINS", "inherited")],
    );
    assert!(wait_until(T, || out.exists()));
    std::thread::sleep(Duration::from_millis(100));
    let env = std::fs::read_to_string(&out).unwrap();
    let get = |k: &str| env.lines().find_map(|l| l.strip_prefix(&format!("{k}=")).map(str::to_string));
    assert_eq!(get("PTY_SESSION").as_deref(), Some(d.name.as_str()));
    assert_eq!(get("PTY_SESSION_GENERATION"), d.meta().unwrap()["generation"].as_str().map(str::to_string));
    assert_eq!(get("TERM").as_deref(), Some("xterm-256color"));
    assert_eq!(get("ASSIGNMENT_WINS").as_deref(), Some("explicit"));
    assert_eq!(get("NO_COLOR"), None);
    assert_eq!(get("SECRET_TOKEN").as_deref(), Some("s3"));
    assert_eq!(get("PWD").as_deref(), Some(root.to_str().unwrap()));
    assert_eq!(get("PTY_SERVER_CONFIG"), None);
    let m = d.meta().unwrap();
    assert_eq!(m["unsetEnv"], json!(["NO_COLOR", "ASSIGNMENT_WINS"]));
    assert_eq!(m["extraEnv"], json!({"ASSIGNMENT_WINS": "explicit", "PTY_SESSION": "spoof"}));
    drop(d);

    // isolateEnv keeps only the allow-list.
    let root = short_root();
    let out = root.join("env.txt");
    let dump = format!("env > {}; sleep 30", out.display());
    let mut cfg = config(&unique_name("env"), "sh", &["-c", &dump]);
    cfg["isolateEnv"] = json!(true);
    let d = Daemon::start_env(&root, cfg, &[("SECRET_TOKEN", "s3"), ("LC_ALL", "C")]);
    assert!(wait_until(T, || out.exists()));
    std::thread::sleep(Duration::from_millis(100));
    let env = std::fs::read_to_string(&out).unwrap();
    assert!(!env.contains("SECRET_TOKEN="), "{env}");
    assert!(env.contains("LC_ALL=C"));
    assert!(env.contains("PATH="));
    assert_eq!(d.meta().unwrap()["isolateEnv"], true);
}

/// node: src/server.ts:236-260, 524-528
#[test]
fn invalid_cwd_aborts_the_daemon_with_nodes_text() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let name = unique_name("cwd");
    let mut cfg = config(&name, "sleep", &["30"]);
    cfg["cwd"] = json!("/no/such/dir/anywhere");
    let out = std::process::Command::new(pty_bin())
        .arg("__daemon")
        .env("PTY_ROOT", &root)
        .env("PTY_SERVER_CONFIG", cfg.to_string())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        format!(
            "Working directory does not exist: /no/such/dir/anywhere\nCannot start session \"{name}\" for command \"sleep\"."
        )
    );
    assert!(!root.join(format!("{name}.sock")).exists());

    let out = std::process::Command::new(pty_bin())
        .arg("__daemon")
        .env("PTY_ROOT", &root)
        .env_remove("PTY_SERVER_CONFIG")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "PTY_SERVER_CONFIG env var required"
    );
}

/// node: tests/events.test.ts:414-523
#[test]
fn terminal_events_reach_the_log() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let d = Daemon::start(
        &root,
        config(
            &unique_name("ev"),
            "sh",
            &["-c", "printf '\\a'; sleep 0.05; printf '\\033]0;first\\007'; sleep 0.05; printf '\\033]0;first\\007'; sleep 0.05; printf '\\033]0;second\\007'; sleep 0.05; printf '\\033]9;hello\\007'; sleep 30"],
        ),
    );
    assert!(wait_until(T, || !d.events("notification").is_empty()));
    let events = read_events(&root, &d.name);
    let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        ["session_start", "bell", "title_change", "title_change", "notification"]
    );
    assert_eq!(events[2]["value"], "first");
    assert_eq!(events[3]["value"], "second");
    assert_eq!(events[4]["body"], "hello");
    assert_eq!(events[4]["source"], "osc9");
}

/// A 3-deep tree whose leaf ignores HUP and TERM is dead before `pty kill`
/// returns, and the same id can be started again right away.
///
/// node: bin/pty-kill-releases-socket-test:42-152
#[test]
fn kill_terminates_a_deep_tree_and_releases_the_name() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let tree = script(
        &root,
        "tree.sh",
        "#!/bin/sh\n\
         case \"$1\" in\n\
           launcher) \"$0\" middle \"$2\" \"$3\" & sleep 1000 ;;\n\
           middle) \"$0\" leaf \"$2\" \"$3\" & sleep 1000 ;;\n\
           leaf) trap '' HUP TERM; echo $$ > \"$2\"; echo ready > \"$3\"; while :; do sleep 1; done ;;\n\
         esac\n",
    );
    let name = unique_name("tree");
    let (first_pid, first_ready) = (root.join("first.pid"), root.join("first.ready"));
    let (_o, e, code) = run_pty(
        &root,
        &["run", "-d", "--id", &name, "--", tree.to_str().unwrap(), "launcher",
          first_pid.to_str().unwrap(), first_ready.to_str().unwrap()],
        &[],
    );
    assert_eq!(code, 0, "{e}");
    assert!(wait_until(T, || first_ready.exists()));
    let leaf: i32 = std::fs::read_to_string(&first_pid).unwrap().trim().parse().unwrap();
    assert!(pid_alive(leaf));

    let (_o, e, code) = run_pty(&root, &["kill", &name], &[]);
    assert_eq!(code, 0, "{e}");
    assert!(process_exited(leaf), "leaf {leaf} survived `pty kill`");
    assert!(!root.join(format!("{name}.sock")).exists());

    let (second_pid, second_ready) = (root.join("second.pid"), root.join("second.ready"));
    let (_o, e, code) = run_pty(
        &root,
        &["run", "-d", "--id", &name, "--", tree.to_str().unwrap(), "launcher",
          second_pid.to_str().unwrap(), second_ready.to_str().unwrap()],
        &[],
    );
    assert_eq!(code, 0, "{e}");
    assert!(wait_until(T, || second_ready.exists()));
    let leaf2: i32 = std::fs::read_to_string(&second_pid).unwrap().trim().parse().unwrap();
    assert_ne!(leaf, leaf2);
    let _ = run_pty(&root, &["kill", &name], &[]);
    assert!(wait_dead(leaf2, Duration::from_secs(4)));
}

/// `pty run` returns only once the daemon has published: `daemonPid` is
/// the new process and `session_start` is on disk — also for an id whose
/// preserved record already carries an exit code.
///
/// node: src/spawn.ts:225-236; tests/recovery.test.ts:335-370
#[test]
fn run_waits_for_the_publication_of_the_replacement() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let name = unique_name("ready");
    let (_o, e, code) = run_pty(&root, &["run", "-d", "--id", &name, "--", "sh", "-c", "exit 3"], PRESERVE);
    assert_eq!(code, 0, "{e}");
    let m = read_meta(&root, &name).unwrap();
    let pid = m["daemonPid"].as_i64().unwrap() as i32;
    assert!(wait_until(T, || read_meta(&root, &name).unwrap()["exitCode"] == 3));
    assert!(wait_dead(pid, T));
    let old_generation = m["generation"].clone();

    let (_o, e, code) = run_pty(&root, &["run", "-d", "--id", &name, "--", "sleep", "30"], PRESERVE);
    assert_eq!(code, 0, "{e}");
    let m = read_meta(&root, &name).unwrap();
    let new_pid = m["daemonPid"].as_i64().unwrap() as i32;
    assert!(pid_alive(new_pid));
    assert_ne!(new_pid, pid);
    assert_ne!(m["generation"], old_generation);
    assert!(m.get("exitCode").is_none(), "{m}");
    let pidfile: i32 = std::fs::read_to_string(root.join(format!("{name}.pid"))).unwrap().trim().parse().unwrap();
    assert_eq!(pidfile, new_pid);
    let starts = events_of_type(&root, &name, "session_start");
    assert_eq!(starts.len(), 1);
    assert!(starts[0]["ts"].as_str().unwrap() >= m["createdAt"].as_str().unwrap());
    assert_eq!(m["command"].as_str().map(|c| c.ends_with("/sleep")), Some(true));
    let _ = run_pty(&root, &["kill", &name], &[]);
}

/// node: src/spawn.ts:217-223
#[test]
fn run_reports_an_immediately_exiting_daemon_with_its_stderr() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let name = unique_name("early");
    let (_o, e, code) = run_pty(
        &root,
        &["run", "-d", "--id", &name, "--cwd", "/no/such/dir/here", "--", "sleep", "30"],
        &[],
    );
    assert_ne!(code, 0);
    assert!(e.contains("Daemon process exited immediately (code 1)."), "{e}");
    assert!(e.contains("Working directory does not exist: /no/such/dir/here"), "{e}");
    let (_o, e, code) = run_pty(&root, &["run", "-d", "--id", &name, "--", "no-such-command-xyz"], &[]);
    assert_ne!(code, 0);
    assert!(e.contains("Command not found: no-such-command-xyz"), "{e}");
}

/// The Node client's machine stream against this daemon:
/// `[GEOMETRY, SCREEN, ..., EXIT]`.
///
/// node: tests/attach-stream.test.ts:126-181
#[test]
fn node_attach_stream_against_the_rust_daemon() {
    skip_without_a_real_machine!();
    let _s = serial();
    let Some(node_pty) = node_pty() else {
        eprintln!("skipped: the Node pty is not on PATH");
        return;
    };
    let root = short_root();
    let name = unique_name("node");
    let (_o, e, code) = run_pty(
        &root,
        &["run", "-d", "--id", &name, "--", "sh", "-c", "echo LAUNCHER_READY; sleep 0.5; echo tail; exit 3"],
        &[],
    );
    assert_eq!(code, 0, "{e}");
    let out_path = root.join("stream.bin");
    let out_file = std::fs::File::create(&out_path).unwrap();
    use std::os::unix::io::AsRawFd;
    use std::os::unix::process::CommandExt;
    let fd = out_file.as_raw_fd();
    let mut cmd = std::process::Command::new(node_pty);
    cmd.args(["attach", "--attach-stream-fd-v1", "3", &name])
        .env("PTY_ROOT", &root)
        .env_remove("PTY_SESSION")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    unsafe {
        cmd.pre_exec(move || {
            if fd == 3 {
                // Already 3: only clear CLOEXEC so it survives the exec.
                libc::fcntl(3, libc::F_SETFD, 0);
            } else if libc::dup2(fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let out = cmd.output().unwrap();
    assert_eq!(out.status.code(), Some(3), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty());
    assert!(out.stderr.is_empty(), "{}", String::from_utf8_lossy(&out.stderr));
    let bytes = std::fs::read(&out_path).unwrap();
    let mut reader = pty_core::protocol::PacketReader::new();
    let packets = reader.feed(&bytes).unwrap();
    let types: Vec<_> = packets.iter().map(|p| p.type_).collect();
    assert_eq!(&types[..2], &[Geometry, Screen], "{types:?}");
    assert_eq!(types.last(), Some(&Exit), "{types:?}");
    assert!(types[2..types.len() - 1].iter().all(|t| *t == Data), "{types:?}");
    assert!(String::from_utf8_lossy(&packets[1].payload).contains("LAUNCHER_READY"));
    assert_eq!(pty_core::protocol::decode_exit(&packets.last().unwrap().payload), 3);
}

#[test]
fn untimed_startup_lifecycle_uses_real_generation_and_equal_clients_can_cas() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let name = unique_name("startup-untimed");
    let mut cfg = config(&name, "/bin/sh", &["-c", "sleep 30"]);
    cfg["startupLease"] = json!({"lifecycleTag": "run.lifecycle"});
    let mut daemon = Daemon::start(&root, cfg);
    assert!(wait_until(T, || {
        daemon
            .meta()
            .and_then(|metadata| metadata["tags"]["run.lifecycle"].as_str().map(str::to_string))
            .is_some()
    }));
    let metadata = daemon.meta().unwrap();
    let generation = metadata["generation"].as_str().unwrap().to_string();
    let starting = metadata["tags"]["run.lifecycle"]
        .as_str()
        .unwrap()
        .to_string();
    let starting_json: serde_json::Value = serde_json::from_str(&starting).unwrap();
    assert_eq!(starting_json["_tag"], "starting");
    assert_eq!(starting_json["generation"], generation);
    assert!(starting_json["bootId"].is_string());
    assert!(starting_json.get("deadlineMonotonicNs").is_none());

    let mut first = daemon.connect();
    first.send(&encode_lifecycle_compare_and_set_request(
        &LifecycleCompareAndSetRequest {
            expected_generation: "replacement".to_string(),
            tag: "run.lifecycle".to_string(),
            expected_value: starting.clone(),
            value: "intermediate".to_string(),
        },
    ));
    assert!(first.wait_type(LifecycleCas, T));
    assert_eq!(
        serde_json::from_slice::<LifecycleCompareAndSetResult>(
            &first
                .packets
                .iter()
                .find(|packet| packet.type_ == LifecycleCas)
                .unwrap()
                .payload
        )
        .unwrap(),
        LifecycleCompareAndSetResult::GenerationMismatch
    );
    first.clear();
    first.send(&encode_lifecycle_compare_and_set_request(
        &LifecycleCompareAndSetRequest {
            expected_generation: generation.clone(),
            tag: "run.lifecycle".to_string(),
            expected_value: starting,
            value: "intermediate".to_string(),
        },
    ));
    assert!(first.wait_type(LifecycleCas, T));
    assert!(matches!(
        serde_json::from_slice::<LifecycleCompareAndSetResult>(
            &first
                .packets
                .iter()
                .find(|packet| packet.type_ == LifecycleCas)
                .unwrap()
                .payload
        )
        .unwrap(),
        LifecycleCompareAndSetResult::Changed { .. }
    ));

    let ready = json!({"_tag":"ready","generation":generation}).to_string();
    let mut second = daemon.connect();
    second.send(&encode_lifecycle_compare_and_set_request(
        &LifecycleCompareAndSetRequest {
            expected_generation: generation.clone(),
            tag: "run.lifecycle".to_string(),
            expected_value: "intermediate".to_string(),
            value: ready.clone(),
        },
    ));
    assert!(second.wait_type(LifecycleCas, T));
    assert_eq!(
        serde_json::from_slice::<LifecycleCompareAndSetResult>(
            &second
                .packets
                .iter()
                .find(|packet| packet.type_ == LifecycleCas)
                .unwrap()
                .payload
        )
        .unwrap(),
        LifecycleCompareAndSetResult::Changed {
            value: ready.clone()
        }
    );
    assert_eq!(
        daemon.meta().unwrap()["tags"]["run.lifecycle"],
        ready
    );
    daemon.signal(libc::SIGTERM);
    assert!(daemon.wait_exit(T).is_some());
}

#[test]
fn timed_startup_lifecycle_publishes_deadline_then_expires_terminally() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let name = unique_name("startup-timed");
    let mut cfg = config(&name, "/bin/sh", &["-c", "sleep 30"]);
    cfg["startupLease"] =
        json!({"timeoutMs": 300, "lifecycleTag": "run.lifecycle"});
    let mut daemon = Daemon::start(&root, cfg);
    assert!(wait_until(T, || {
        daemon.meta().is_some_and(|metadata| {
            metadata["tags"]["run.lifecycle"]
                .as_str()
                .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
                .is_some_and(|value| value["deadlineMonotonicNs"].is_string())
        })
    }));
    assert_eq!(daemon.wait_exit(T), Some(124));
    let metadata = daemon.meta().unwrap();
    let terminal: serde_json::Value =
        serde_json::from_str(metadata["tags"]["run.lifecycle"].as_str().unwrap()).unwrap();
    assert_eq!(terminal["_tag"], "terminal");
    assert_eq!(terminal["generation"], metadata["generation"]);
    assert_eq!(terminal["cause"], "deadline");
}

// --- PR #34 review findings: startup-lease retention and deadline containment.

fn lifecycle_value(daemon: &Daemon) -> serde_json::Value {
    let metadata = daemon
        .meta()
        .expect("the session's metadata was not retained");
    serde_json::from_str(
        metadata["tags"]["run.lifecycle"]
            .as_str()
            .expect("no lifecycle tag"),
    )
    .unwrap()
}

/// `pty evidence snapshot --id <name>` against the daemon's registry.
fn evidence_snapshot(root: &std::path::Path, name: &str) -> serde_json::Value {
    let output = std::process::Command::new(pty_bin())
        .args(["evidence", "snapshot", "--id", name])
        .env("PTY_ROOT", root)
        .env_remove("PTY_SESSION")
        .output()
        .expect("run pty evidence snapshot");
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "evidence snapshot printed {:?} / {:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// PR #34: a naturally exiting startup-lease child under the default reap
/// policy (`PTY_REAP_ON_EXIT` unset) deleted the metadata and events that
/// carry its terminal lifecycle and exit evidence. Both must survive the
/// daemon, ephemeral or not, until the generation-fenced removal.
#[test]
fn a_natural_exit_keeps_the_startup_lease_lifecycle_and_evidence() {
    skip_without_a_real_machine!();
    let _s = serial();
    for ephemeral in [false, true] {
        let root = short_root();
        let name = unique_name("retain");
        let mut cfg = config(&name, "/bin/sh", &["-c", "exit 7"]);
        cfg["startupLease"] = json!({"lifecycleTag": "run.lifecycle"});
        cfg["ephemeral"] = json!(ephemeral);
        let mut daemon = Daemon::spawn(&root, cfg, &[]);
        assert_eq!(daemon.wait_exit(T), Some(7), "ephemeral={ephemeral}");

        let metadata = daemon
            .meta()
            .unwrap_or_else(|| panic!("ephemeral={ephemeral}: the session was reaped"));
        assert_eq!(metadata["exitCode"], 7);
        let terminal = lifecycle_value(&daemon);
        assert_eq!(terminal["_tag"], "terminal");
        assert_eq!(terminal["cause"], "exit");
        assert_eq!(terminal["generation"], metadata["generation"]);
        assert!(root.join(format!("{name}.events.jsonl")).exists());

        let evidence = evidence_snapshot(&root, &name);
        assert_eq!(
            evidence["_tag"], "snapshot",
            "ephemeral={ephemeral}: {evidence}"
        );
        assert_eq!(evidence["snapshot"]["exitCode"], 7);
        assert_eq!(evidence["snapshot"]["generation"], metadata["generation"]);
    }
}

/// The same retention on the exit a signal causes, and the unchanged default
/// for a session without a lease.
#[test]
fn only_startup_lease_generations_skip_exit_reaping() {
    skip_without_a_real_machine!();
    let _s = serial();
    let root = short_root();
    let mut daemon = Daemon::spawn(
        &root,
        config(&unique_name("plain"), "/bin/sh", &["-c", "exit 7"]),
        &[],
    );
    assert_eq!(daemon.wait_exit(T), Some(7));
    assert!(
        daemon.meta().is_none(),
        "a session without a lease is still reaped by default"
    );
}

/// A child that forks one descendant into its own process group, which
/// ignores the hangup the kernel sends it when the child dies, and one into a
/// new session, outside the child's group. Returns the script and where each
/// writes its pid.
fn tree_script(
    dir: &std::path::Path,
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let in_group = dir.join("in-group.pid");
    let other_session = dir.join("other-session.pid");
    let body = format!(
        "#!/bin/sh\ntrap '' HUP\nsleep 300 &\necho $! > {}\nsetsid sleep 300 &\necho $! > {}\nwait\n",
        in_group.display(),
        other_session.display()
    );
    (script(dir, "tree.sh", &body), in_group, other_session)
}

fn read_pid(path: &std::path::Path) -> i32 {
    assert!(wait_until(T, || std::fs::read_to_string(path)
        .is_ok_and(|s| s.trim().parse::<i32>().is_ok())));
    std::fs::read_to_string(path)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn kill_if_alive(pid: i32) {
    if pid_alive(pid) {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

/// Start a timed startup lease over the descendant tree and return the
/// daemon and both descendants' pids once they exist.
fn timed_tree(timeout_ms: u64, env: &[(&str, &str)]) -> (Daemon, i32, i32, i32) {
    let root = short_root();
    let pids = short_root();
    let (tree, in_group, other_session) = tree_script(&pids);
    let mut cfg = config(&unique_name("deadline"), tree.to_str().unwrap(), &[]);
    cfg["startupLease"] = json!({"timeoutMs": timeout_ms, "lifecycleTag": "run.lifecycle"});
    let daemon = Daemon::start_env(&root, cfg, env);
    let child = daemon.child_pid();
    (daemon, child, read_pid(&in_group), read_pid(&other_session))
}

/// PR #34: with the whole tree observed, the deadline is `deadline`, and the
/// teardown reaches every descendant, including one outside the group.
#[test]
fn a_complete_observation_records_deadline_and_reaches_the_whole_tree() {
    skip_without_a_real_machine!();
    let _s = serial();
    let (mut daemon, child, in_group, other_session) = timed_tree(1_500, &[]);
    assert_eq!(daemon.wait_exit(T), Some(124));
    assert_eq!(lifecycle_value(&daemon)["cause"], "deadline");
    for (pid, what) in [
        (child, "child"),
        (in_group, "in-group descendant"),
        (other_session, "out-of-group descendant"),
    ] {
        assert!(
            wait_dead(pid, Duration::from_secs(3)),
            "the {what} survived the deadline"
        );
    }
}

/// PR #34: an unreadable process table proves nothing, so the terminal cause
/// is `teardown-unavailable`, never `deadline`, and the process-group
/// fallback still reaches the child's group.
#[test]
fn an_unreadable_process_table_records_teardown_unavailable() {
    skip_without_a_real_machine!();
    let _s = serial();
    let (mut daemon, child, in_group, other_session) =
        timed_tree(1_500, &[("PTY_TEST_PROCESS_TABLE", "unreadable")]);
    assert_eq!(daemon.wait_exit(T), Some(124));
    let terminal = lifecycle_value(&daemon);
    kill_if_alive(other_session);
    assert_eq!(terminal["cause"], "teardown-unavailable");
    assert!(
        wait_dead(child, Duration::from_secs(3)),
        "the child survived"
    );
    assert!(
        wait_dead(in_group, Duration::from_secs(3)),
        "the group fallback missed the child's group"
    );
    assert!(daemon.meta().is_some(), "the evidence was not retained");
}

/// PR #34: a table that skipped rows is not complete either, and every
/// descendant it did observe is still signalled by its exact identity: the
/// one in another session is out of the group fallback's reach.
#[test]
fn an_incomplete_process_table_records_teardown_unavailable_and_signals_what_it_saw() {
    skip_without_a_real_machine!();
    let _s = serial();
    let (mut daemon, child, in_group, other_session) =
        timed_tree(1_500, &[("PTY_TEST_PROCESS_TABLE", "incomplete")]);
    assert_eq!(daemon.wait_exit(T), Some(124));
    let terminal = lifecycle_value(&daemon);
    let survived = !wait_dead(other_session, Duration::from_secs(3));
    kill_if_alive(other_session);
    assert_eq!(terminal["cause"], "teardown-unavailable");
    assert!(
        !survived,
        "an observed descendant outside the group was never signalled"
    );
    assert!(
        wait_dead(child, Duration::from_secs(3)),
        "the child survived"
    );
    assert!(
        wait_dead(in_group, Duration::from_secs(3)),
        "the in-group descendant survived"
    );
}

/// The lifecycle CAS that finds the deadline expired settles it the same way
/// as the timer: `deadline` only on a complete observation.
#[test]
fn a_lifecycle_cas_after_the_deadline_chooses_its_cause_from_the_observation() {
    skip_without_a_real_machine!();
    let _s = serial();
    for (table, cause) in [
        (None, "deadline"),
        (Some("unreadable"), "teardown-unavailable"),
    ] {
        let mut env = vec![("PTY_TEST_NO_STARTUP_TIMER", "1")];
        if let Some(table) = table {
            env.push(("PTY_TEST_PROCESS_TABLE", table));
        }
        let (mut daemon, child, in_group, other_session) = timed_tree(200, &env);
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            daemon.alive(),
            "the startup timer ran although the test disabled it"
        );
        let metadata = daemon.meta().unwrap();
        let generation = metadata["generation"].as_str().unwrap().to_string();
        let starting = metadata["tags"]["run.lifecycle"]
            .as_str()
            .unwrap()
            .to_string();
        let mut client = daemon.connect();
        client.send(&encode_lifecycle_compare_and_set_request(
            &LifecycleCompareAndSetRequest {
                expected_generation: generation,
                tag: "run.lifecycle".to_string(),
                expected_value: starting,
                value: "ready".to_string(),
            },
        ));
        assert!(client.wait_type(LifecycleCas, T));
        let result: LifecycleCompareAndSetResult = serde_json::from_slice(
            &client
                .packets
                .iter()
                .find(|packet| packet.type_ == LifecycleCas)
                .unwrap()
                .payload,
        )
        .unwrap();
        let LifecycleCompareAndSetResult::DeadlineExpired { value } = result else {
            panic!("expected DeadlineExpired, got {result:?}");
        };
        let value: serde_json::Value = serde_json::from_str(&value).unwrap();
        assert_eq!(value["cause"], cause, "table={table:?}");
        assert_eq!(daemon.wait_exit(T), Some(124));
        assert_eq!(lifecycle_value(&daemon)["cause"], cause);
        kill_if_alive(other_session);
        assert!(wait_dead(child, Duration::from_secs(3)));
        assert!(wait_dead(in_group, Duration::from_secs(3)));
    }
}
