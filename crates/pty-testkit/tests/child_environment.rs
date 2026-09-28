//! What a session's child starts with, and what the runtime keeps to itself.
//!
//! A program in a session should see the terminal and the login session it
//! really runs under: the identity of the terminal that renders it, the
//! desktop, agent and locale variables of the person who started it, the
//! working directory and argument vector it was asked for, and a controlling
//! terminal it can open. It should not see the runtime's own bookkeeping, or
//! identity left behind by a terminal that is no longer in front of it.
//!
//! The runtime's side of the same contract: its sockets and state are owner
//! only, machine local, and fit the kernel's socket path limit; a start that
//! cannot happen says why; and a session name means exactly that name.
//!
//! Tests marked `#[ignore = "fails on main: ..."]` reproduce a behaviour that
//! does not hold yet. Run one with `-- --ignored <name>` to see the failure.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use pty_core::registry::{ListOptions, SessionInfo, SessionStatus, list_sessions_in};
use pty_testkit::{ServerOptions, Session, SpawnOptions};

/// Point the testkit at the `pty` built from this workspace.
fn use_local_pty() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // CARGO_BIN_EXE_ is only set for the crate that owns the binary, so
        // find it beside this test binary instead.
        let mut dir = std::env::current_exe().expect("test binary path");
        dir.pop(); // deps/
        dir.pop(); // debug/
        let bin = dir.join("pty");
        if bin.exists() {
            // SAFETY: set once, before this test reads it, and never changed
            // afterwards.
            unsafe { std::env::set_var("PTY_BIN", &bin) };
        }
    });
}

fn pty_bin() -> String {
    use_local_pty();
    pty_testkit::server::pty_bin()
}

/// Variables that would make a `pty` started by a test think it runs inside
/// somebody's session, or bind it to somebody else's registry or lifetime.
/// The harness that runs these tests may itself be inside a session.
const AMBIENT_RUNTIME_KEYS: &[&str] = &[
    "PTY_ROOT",
    "PTY_SESSION",
    "PTY_SESSION_GENERATION",
    "PTY_SESSION_DIR",
    "PTY_SERVER_CONFIG",
    "PTY_CREATION_LOCK_OWNER_PID",
    "PTY_SPAWNER_PID",
    "PTY_REAP_ON_EXIT",
    "TMUX",
    "TMUX_PANE",
];

/// `pty` with no ambient session context, rooted at `root`.
fn pty_at(root: &Path) -> Command {
    let mut cmd = Command::new(pty_bin());
    for key in AMBIENT_RUNTIME_KEYS {
        cmd.env_remove(key);
    }
    cmd.env("PTY_ROOT", root);
    cmd.stdin(Stdio::null());
    cmd
}

fn text(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn run_ok(mut cmd: Command) -> Output {
    let out = cmd.output().expect("run pty");
    let (so, se) = text(&out);
    assert!(out.status.success(), "pty failed: {:?}\nstdout: {so}\nstderr: {se}", out.status);
    out
}

fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Wait for a file a child writes with `> x.tmp && mv x.tmp x`.
fn wait_for_file(path: &Path, timeout: Duration) -> String {
    assert!(
        wait_until(timeout, || path.exists()),
        "{} never appeared",
        path.display()
    );
    std::fs::read_to_string(path).expect("read the child's file")
}

fn env_value<'a>(dump: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}=");
    dump.lines().find_map(|l| l.strip_prefix(prefix.as_str()))
}

/// A shell snippet that dumps the environment to `path` atomically.
fn dump_env_to(path: &Path) -> String {
    let p = path.display();
    format!("env > '{p}.tmp' && mv '{p}.tmp' '{p}'")
}

fn sessions(root: &Path) -> Vec<SessionInfo> {
    list_sessions_in(root, &ListOptions::default())
}

fn running_session(root: &Path, name: &str) -> bool {
    sessions(root)
        .iter()
        .any(|s| s.name == name && s.status == SessionStatus::Running)
}

/// A private registry that kills whatever it holds when the test ends,
/// including sessions that landed in a directory below it.
struct Root {
    dir: PathBuf,
}

impl Root {
    fn new() -> Root {
        use_local_pty();
        // Short: a session socket path has to fit the kernel's limit.
        let dir = std::env::temp_dir().join(format!("ce-{}", pty_testkit::server::random_id()));
        std::fs::create_dir_all(&dir).expect("create the test root");
        Root { dir }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    fn pty(&self) -> Command {
        pty_at(&self.dir)
    }

    fn mkdir(&self, rel: &str) -> PathBuf {
        let dir = self.path(rel);
        std::fs::create_dir_all(&dir).expect("create a directory");
        dir
    }
}

fn kill_sessions_below(dir: &Path, depth: u32) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(session) = name.strip_suffix(".sock") {
            let _ = pty_at(dir)
                .args(["kill", session])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        } else if depth > 0 && entry.file_type().is_ok_and(|t| t.is_dir()) {
            kill_sessions_below(&path, depth - 1);
        }
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        kill_sessions_below(&self.dir, 6);
        // A daemon whose program already ended lingers a moment and may
        // recreate the registry to record its exit. Let them all finish.
        wait_until(Duration::from_secs(5), || !daemons_rooted_in(&self.dir));
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Is any session daemon still running with its registry (or home) below
/// `dir`? Only Linux can tell cheaply; elsewhere give them a moment.
fn daemons_rooted_in(dir: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        let prefixes = [
            format!("PTY_ROOT={}", dir.display()),
            format!("HOME={}", dir.display()),
        ];
        let Ok(procs) = std::fs::read_dir("/proc") else {
            return false;
        };
        procs.flatten().any(|p| {
            let path = p.path();
            std::fs::read_to_string(path.join("comm")).is_ok_and(|c| c.trim() == "pty-daemon")
                && std::fs::read(path.join("environ")).is_ok_and(|env| {
                    env.split(|b| *b == 0).any(|var| {
                        let var = String::from_utf8_lossy(var);
                        prefixes.iter().any(|p| var.starts_with(p.as_str()))
                    })
                })
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = dir;
        std::thread::sleep(Duration::from_millis(2500));
        false
    }
}

fn have(tool: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {tool}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

// ── The terminal a session advertises ──

/// Each session is born from the terminal that created it, so a session
/// created later from another terminal names that terminal, not the first
/// one — there is no long-lived server whose birth environment every later
/// session inherits.
#[test]
fn a_session_advertises_the_terminal_that_created_it() {
    let root = Root::new();
    let a_dump = root.path("a.env");
    let b_dump = root.path("b.env");

    let mut a = root.pty();
    a.env("TERM", "xterm-term-a")
        .env("COLORTERM", "truecolor")
        .env("TERM_PROGRAM", "TermA")
        .env("TERM_PROGRAM_VERSION", "1.2.0")
        .args(["run", "-d", "--id", "from-a", "--", "sh", "-c"])
        .arg(format!("{}; exec sleep 30", dump_env_to(&a_dump)));
    run_ok(a);
    let a_env = wait_for_file(&a_dump, Duration::from_secs(8));
    assert_eq!(env_value(&a_env, "TERM"), Some("xterm-term-a"));
    assert_eq!(env_value(&a_env, "COLORTERM"), Some("truecolor"));
    assert_eq!(env_value(&a_env, "TERM_PROGRAM"), Some("TermA"));

    // Terminal A's session is still running when terminal B creates one.
    let mut b = root.pty();
    b.env("TERM", "xterm-term-b")
        .env("COLORTERM", "truecolor")
        .env("TERM_PROGRAM", "TermB")
        .env_remove("TERM_PROGRAM_VERSION")
        .args(["run", "-d", "--id", "from-b", "--", "sh", "-c"])
        .arg(format!("{}; exec sleep 30", dump_env_to(&b_dump)));
    run_ok(b);
    let b_env = wait_for_file(&b_dump, Duration::from_secs(8));
    assert!(running_session(&root.dir, "from-a"), "the first session should still run");
    assert_eq!(env_value(&b_env, "TERM"), Some("xterm-term-b"), "{b_env}");
    assert_eq!(env_value(&b_env, "TERM_PROGRAM"), Some("TermB"), "{b_env}");
    assert_eq!(
        env_value(&b_env, "TERM_PROGRAM_VERSION"),
        None,
        "terminal A's version must not reach terminal B's session"
    );
}

/// A session outlives the terminal it was created in, and is later attached
/// from another. Programs in it must not be told they run inside the first
/// terminal, or inside a wrapper around it: they would drive a terminal that
/// is gone (ask a closed terminal to split a pane, skip shell integration
/// because a wrapper believes it already wrapped this shell, write to a
/// wrapper's descriptor that was never passed down).
#[test]
#[ignore = "fails on main: the creating terminal's identity (ITERM_SESSION_ID, TERM_PROGRAM, WT_SESSION, Q_TERM, IRIS_FD) is inherited verbatim and still names it after a reattach from another terminal"]
fn host_terminal_identity_does_not_follow_a_session_to_another_terminal() {
    let root = Root::new();

    // Terminal A, inside a shell-integration wrapper that owns a descriptor.
    let mut a = root.pty();
    a.env("PS1", "ready> ")
        .env("TERM", "xterm-256color")
        .env("TERM_PROGRAM", "TermA")
        .env("TERM_PROGRAM_VERSION", "3.5.0")
        .env("LC_TERMINAL", "TermA")
        .env("ITERM_SESSION_ID", "w0t0p0:AAAA-A")
        .env("TERM_SESSION_ID", "w0t0p0:AAAA-A")
        .env("WT_SESSION", "11111111-aaaa")
        .env("Q_TERM", "2.12.3")
        .env("IRIS_FD", "13")
        .args(["run", "-d", "--id", "wrapped", "--", "sh"]);
    run_ok(a);

    // Terminal A is gone; terminal B attaches.
    let mut b = Session::spawn(
        &pty_bin(),
        &["attach", "wrapped"],
        SpawnOptions {
            rows: Some(24),
            cols: Some(120),
            env: vec![
                ("PTY_ROOT".into(), root.dir.to_string_lossy().into_owned()),
                ("TERM".into(), "xterm-term-b".into()),
                ("TERM_PROGRAM".into(), "TermB".into()),
                ("COLORTERM".into(), "truecolor".into()),
            ],
            ..Default::default()
        },
    )
    .expect("attach from terminal B");
    b.wait_for_text("ready> ", 8000).expect("the session's shell prompt");
    b.type_str(
        "printf 'SEEN%s ITERM=[%s] TSID=[%s] WT=[%s] Q=[%s] IRIS=[%s] PROG=[%s] LCT=[%s]\\n' : \
         \"$ITERM_SESSION_ID\" \"$TERM_SESSION_ID\" \"$WT_SESSION\" \"$Q_TERM\" \"$IRIS_FD\" \
         \"$TERM_PROGRAM\" \"$LC_TERMINAL\"\r",
    );
    let shot = b.wait_for_text("SEEN:", 8000).expect("the probe answered");
    let line = shot
        .lines
        .iter()
        .find(|l| l.contains("SEEN:"))
        .cloned()
        .unwrap_or_default();
    b.close();

    for stale in ["AAAA", "11111111", "2.12.3", "IRIS=[13]", "TermA"] {
        assert!(
            !line.contains(stale),
            "a program attached from terminal B still sees terminal A's identity ({stale}): {line}"
        );
    }
}

/// With no terminal behind its creation (a script, an agent, a service),
/// a session still renders through libghostty, which draws 24-bit color. The
/// child should be told so in a way terminfo-driven programs can see.
#[test]
#[ignore = "fails on main: with no TERM/COLORTERM to inherit the child gets TERM=xterm-256color (no RGB/Tc terminfo caps) and no COLORTERM, so truecolor is never advertised"]
fn a_session_started_without_a_terminal_still_advertises_truecolor() {
    let root = Root::new();
    let dump = root.path("t.env");
    let caps = root.path("t.caps");
    let mut cmd = root.pty();
    cmd.env_remove("TERM")
        .env_remove("COLORTERM")
        .env_remove("TERM_PROGRAM")
        .args(["run", "-d", "--id", "headless", "--", "sh", "-c"])
        .arg(format!(
            "{}; (infocmp -x \"$TERM\" 2>&1 || true) > '{c}.tmp'; mv '{c}.tmp' '{c}'; exec sleep 30",
            dump_env_to(&dump),
            c = caps.display()
        ));
    run_ok(cmd);
    let env = wait_for_file(&dump, Duration::from_secs(8));
    let caps = wait_for_file(&caps, Duration::from_secs(8));
    let colorterm = env_value(&env, "COLORTERM").unwrap_or("");
    let terminfo_truecolor = ["RGB", "Tc", "setrgbf"]
        .iter()
        .any(|cap| caps.split([',', ' ', '\t', '\n', '=']).any(|w| w == *cap));
    assert!(
        matches!(colorterm, "truecolor" | "24bit") || terminfo_truecolor,
        "TERM={:?} COLORTERM={colorterm:?}: nothing tells the child the session renders 24-bit color",
        env_value(&env, "TERM")
    );
}

// ── The login session a child belongs to ──

/// A session gets the desktop, agent, locale and PATH of whoever created
/// it — including when an earlier session was created from somewhere that
/// had none of them.
#[test]
fn a_session_gets_the_session_environment_of_whoever_created_it() {
    let root = Root::new();
    let bin_dir = root.mkdir("user-bin");
    let runtime_dir = root.mkdir("run");
    let agent = root.path("run/agent.sock");
    let headless_dump = root.path("headless.env");
    let desktop_dump = root.path("desktop.env");
    let base_path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());

    // First, a session created from a text console: no display, no agent.
    let mut headless = root.pty();
    for key in ["DISPLAY", "WAYLAND_DISPLAY", "XDG_SESSION_TYPE", "SSH_AUTH_SOCK"] {
        headless.env_remove(key);
    }
    headless
        .env("LANG", "C")
        .args(["run", "-d", "--id", "console", "--", "sh", "-c"])
        .arg(format!("{}; exec sleep 30", dump_env_to(&headless_dump)));
    run_ok(headless);
    let headless_env = wait_for_file(&headless_dump, Duration::from_secs(8));
    assert_eq!(env_value(&headless_env, "DISPLAY"), None);

    // Then one from a graphical terminal, while the first still runs.
    let path = format!("{}:{base_path}", bin_dir.display());
    let wanted = [
        ("DISPLAY", ":77".to_string()),
        ("WAYLAND_DISPLAY", "wayland-7".to_string()),
        ("XDG_SESSION_TYPE", "wayland".to_string()),
        ("XDG_RUNTIME_DIR", runtime_dir.to_string_lossy().into_owned()),
        ("SSH_AUTH_SOCK", agent.to_string_lossy().into_owned()),
        ("LANG", "en_US.UTF-8".to_string()),
        ("LC_CTYPE", "en_US.UTF-8".to_string()),
        ("PATH", path),
    ];
    let mut desktop = root.pty();
    for (k, v) in &wanted {
        desktop.env(k, v);
    }
    desktop
        .args(["run", "-d", "--id", "desktop", "--", "sh", "-c"])
        .arg(format!("{}; exec sleep 30", dump_env_to(&desktop_dump)));
    run_ok(desktop);
    let desktop_env = wait_for_file(&desktop_dump, Duration::from_secs(8));
    assert!(running_session(&root.dir, "console"));
    for (k, v) in &wanted {
        assert_eq!(
            env_value(&desktop_env, k),
            Some(v.as_str()),
            "{k} did not reach the session\n{desktop_env}"
        );
    }
}

/// `pty gc` brings a permanent session back when its program ends, and it is
/// what a service manager runs on a timer (`gc --print-launchd-plist`). The
/// session it brings back must still belong to its user's login session, not
/// to the bare service environment the collector happens to run in.
#[test]
#[ignore = "fails on main: a permanent session respawned by `pty gc` from a service environment loses LANG/LC_*, DISPLAY, WAYLAND_DISPLAY, XDG_SESSION_TYPE, SSH_AUTH_SOCK and the user's PATH"]
fn a_respawned_permanent_session_keeps_its_session_environment() {
    let root = Root::new();
    let bin_dir = root.mkdir("user-bin");
    let dumps = root.mkdir("dumps");
    let home = root.mkdir("home");
    let base_path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
    let user_path = format!("{}:{base_path}", bin_dir.display());
    let wanted = [
        ("LANG", "en_US.UTF-8"),
        ("LC_CTYPE", "en_US.UTF-8"),
        ("DISPLAY", ":77"),
        ("WAYLAND_DISPLAY", "wayland-7"),
        ("XDG_SESSION_TYPE", "wayland"),
        ("SSH_AUTH_SOCK", "/tmp/agent-for-this-login.sock"),
    ];

    // Each incarnation dumps its environment under its own generation.
    let script = format!(
        "d='{d}'; env > \"$d/$PTY_SESSION_GENERATION.tmp\" && mv \"$d/$PTY_SESSION_GENERATION.tmp\" \"$d/$PTY_SESSION_GENERATION.env\"; sleep 1",
        d = dumps.display()
    );
    let mut create = root.pty();
    for (k, v) in wanted {
        create.env(k, v);
    }
    create
        .env("PATH", &user_path)
        .args(["run", "-d", "--id", "perm", "--tag", "strategy=permanent", "--", "sh", "-c"])
        .arg(&script);
    run_ok(create);

    let env_files = || -> Vec<PathBuf> {
        std::fs::read_dir(&dumps)
            .map(|d| {
                d.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "env"))
                    .collect()
            })
            .unwrap_or_default()
    };
    assert!(wait_until(Duration::from_secs(8), || env_files().len() == 1));
    let first = std::fs::read_to_string(&env_files()[0]).unwrap();
    for (k, v) in wanted {
        assert_eq!(env_value(&first, k), Some(v), "sanity: the first run saw {k}");
    }
    assert!(
        wait_until(Duration::from_secs(8), || !running_session(&root.dir, "perm")),
        "the permanent session's program should have ended"
    );

    // The collector, the way a service manager runs it: a bare environment.
    // Its daemon lingers a moment after the program ends; the next tick of
    // the collector is what brings the session back.
    let mut last = String::new();
    let respawned = wait_until(Duration::from_secs(10), || {
        let out = Command::new(pty_bin())
            .env_clear()
            .env("HOME", &home)
            .env("PATH", "/usr/bin:/bin")
            .env("PTY_ROOT", &root.dir)
            .arg("gc")
            .stdin(Stdio::null())
            .output()
            .expect("run gc");
        let (so, se) = text(&out);
        last = format!("{so}{se}");
        so.contains("Respawned: perm")
    });
    assert!(respawned, "gc did not respawn: {last}");
    assert!(wait_until(Duration::from_secs(8), || env_files().len() == 2));
    let respawned = env_files()
        .into_iter()
        .map(|p| std::fs::read_to_string(p).unwrap())
        .find(|dump| dump != &first)
        .expect("the respawned incarnation's environment");

    let mut lost: Vec<String> = wanted
        .iter()
        .filter(|(k, v)| env_value(&respawned, k) != Some(*v))
        .map(|(k, _)| k.to_string())
        .collect();
    if !env_value(&respawned, "PATH").is_some_and(|p| p.contains(&*bin_dir.to_string_lossy())) {
        lost.push("PATH".into());
    }
    assert!(
        lost.is_empty(),
        "the respawned session lost {lost:?} from the login session it was created in:\n{respawned}"
    );
}

/// A process started in a session — a tmux server is the classic case —
/// can outlive it and start unrelated programs in other terminals. Those
/// programs are not inside the session and must not be treated as nested.
#[test]
#[ignore = "fails on main: PTY_SESSION leaks through a tmux server started in a session; in a later tmux session `pty run` says `Already inside pty session` and runs the command directly instead of creating a session"]
fn a_nesting_marker_does_not_follow_a_process_out_of_its_session() {
    if !have("tmux") {
        eprintln!("skipped: tmux is not installed");
        return;
    }
    fn tmux_in(dir: &Path, args: &[&str]) -> Output {
        let mut cmd = Command::new("tmux");
        for key in AMBIENT_RUNTIME_KEYS {
            cmd.env_remove(key);
        }
        cmd.env("TMUX_TMPDIR", dir)
            .args(["-L", "leak", "-f", "/dev/null"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.output().expect("run tmux")
    }
    struct KillServer(PathBuf);
    impl Drop for KillServer {
        fn drop(&mut self) {
            let _ = tmux_in(&self.0, &["kill-server"]);
        }
    }
    let root = Root::new();
    let tmux_dir = root.mkdir("tm");
    let tmux = |args: &[&str]| tmux_in(&tmux_dir, args);
    let _server = KillServer(tmux_dir.clone());

    // A session starts a tmux server, then ends; the server lives on.
    let mut outer = root.pty();
    outer
        .env("TMUX_TMPDIR", &tmux_dir)
        .args(["run", "-d", "--id", "outer", "--", "sh", "-c"])
        .arg("tmux -L leak -f /dev/null new-session -d -s first 'sleep 60'; exec sleep 60");
    run_ok(outer);
    assert!(
        wait_until(Duration::from_secs(8), || tmux(&["has-session", "-t", "first"]).status.success()),
        "the session's tmux server did not start"
    );
    run_ok({
        let mut kill = root.pty();
        kill.args(["kill", "outer"]);
        kill
    });
    assert!(
        tmux(&["has-session", "-t", "first"]).status.success(),
        "the tmux server should outlive the session that started it"
    );

    // Later, from outside any session, somebody opens a tmux session on that
    // server and creates a pty session in it.
    let script = format!(
        "'{bin}' run --id inner -- sh -c 'sleep 3'; exec sleep 30",
        bin = pty_bin()
    );
    assert!(
        tmux(&["new-session", "-d", "-s", "second", "-x", "100", "-y", "20", &script])
            .status
            .success()
    );
    let created = wait_until(Duration::from_secs(6), || root.path("inner.json").exists());
    let pane = String::from_utf8_lossy(&tmux(&["capture-pane", "-p", "-t", "second"]).stdout)
        .into_owned();
    assert!(
        created && !pane.contains("Already inside pty session"),
        "a program in an unrelated tmux session was treated as nested inside the gone session:\n{pane}"
    );
}

/// The runtime passes its daemon a couple of one-hop control values. They
/// are the runtime's own business and must not reach the session's child,
/// from where they would travel on into anything the child starts.
#[test]
#[ignore = "fails on main: PTY_CREATION_LOCK_OWNER_PID (and an inherited PTY_SPAWNER_PID) are passed through into the session child's environment"]
fn runtime_control_values_stay_out_of_the_child_environment() {
    let root = Root::new();
    let dump = root.path("ctl.env");
    let mut spawner = Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("a stand-in spawner");
    let mut cmd = root.pty();
    cmd.env("PTY_SPAWNER_PID", spawner.id().to_string())
        .args(["run", "-d", "--id", "ctl", "--", "sh", "-c"])
        .arg(format!("{}; exec sleep 30", dump_env_to(&dump)));
    let out = cmd.output().expect("run pty");
    let env = wait_for_file(&dump, Duration::from_secs(8));
    let _ = spawner.kill();
    let _ = spawner.wait();
    assert!(out.status.success());
    let leaked: Vec<&str> = ["PTY_CREATION_LOCK_OWNER_PID", "PTY_SPAWNER_PID", "PTY_DAEMON_READY_FD", "PTY_SERVER_CONFIG"]
        .into_iter()
        .filter(|k| env_value(&env, k).is_some())
        .collect();
    assert!(leaked.is_empty(), "runtime control values reached the child: {leaked:?}");
}

/// The picker's one-key create starts "your shell". With `SHELL` unset (a
/// container's entrypoint, a service), that has to be the account's login
/// shell from the user database, not a fixed guess.
///
/// The account database is simulated with a preloaded shim that reports
/// `/bin/sh` as this user's login shell, so the test does not depend on the
/// shell the machine's account happens to have.
#[test]
#[ignore = "fails on main: with SHELL unset the picker starts /bin/bash, never consulting the account's login shell"]
fn with_shell_unset_the_picker_starts_the_accounts_login_shell() {
    if !have("cc") || !have("python3") {
        eprintln!("skipped: needs cc and python3");
        return;
    }
    let root = Root::new();
    let home = root.mkdir("home");
    let shim_src = root.path("pwshim.c");
    let shim = root.path("pwshim.so");
    std::fs::write(&shim_src, PASSWD_SHIM).unwrap();
    let built = Command::new("cc")
        .args(["-shared", "-fPIC", "-o"])
        .arg(&shim)
        .arg(&shim_src)
        .arg("-ldl")
        .output()
        .expect("run cc");
    if !built.status.success() {
        eprintln!("skipped: could not build the passwd shim: {}", text(&built).1);
        return;
    }
    let preload = format!("LD_PRELOAD={}", shim.display());
    let account_shell = Command::new("env")
        .args([preload.as_str(), "python3", "-c", "import os,pwd;print(pwd.getpwuid(os.getuid()).pw_shell)"])
        .output()
        .expect("ask the account database");
    let account_shell = String::from_utf8_lossy(&account_shell.stdout).trim().to_string();
    assert_eq!(account_shell, "/bin/sh", "the shim should define this account's login shell");

    let bin = pty_bin();
    let mut picker = Session::spawn(
        "env",
        &["-u", "SHELL", preload.as_str(), bin.as_str()],
        SpawnOptions {
            rows: Some(24),
            cols: Some(100),
            env: vec![
                ("PTY_ROOT".into(), root.dir.to_string_lossy().into_owned()),
                ("HOME".into(), home.to_string_lossy().into_owned()),
            ],
            ..Default::default()
        },
    )
    .expect("start the picker");
    picker
        .wait_for_text("Create new session", 10_000)
        .expect("the picker's create row");
    picker.type_str("\r");
    let mut started = String::new();
    let found = wait_until(Duration::from_secs(10), || {
        if let Some(s) = sessions(&root.dir).into_iter().find(|s| s.status == SessionStatus::Running)
        {
            started = s.metadata.map(|m| m.command).unwrap_or_default();
            !started.is_empty()
        } else {
            false
        }
    });
    picker.close();
    assert!(found, "the picker created no session");
    assert_eq!(
        started, account_shell,
        "with SHELL unset the new session should run the account's login shell"
    );
}

/// Makes every user-database lookup report `/bin/sh` as the login shell.
const PASSWD_SHIM: &str = r#"
#define _GNU_SOURCE
#include <dlfcn.h>
#include <pwd.h>
#include <stddef.h>
#include <sys/types.h>
static char shell[] = "/bin/sh";
typedef int (*uid_r)(uid_t, struct passwd *, char *, size_t, struct passwd **);
typedef int (*nam_r)(const char *, struct passwd *, char *, size_t, struct passwd **);
typedef struct passwd *(*uid_f)(uid_t);
typedef struct passwd *(*nam_f)(const char *);
int getpwuid_r(uid_t u, struct passwd *p, char *b, size_t n, struct passwd **o) {
    int rc = ((uid_r)dlsym(RTLD_NEXT, "getpwuid_r"))(u, p, b, n, o);
    if (rc == 0 && o && *o) (*o)->pw_shell = shell;
    return rc;
}
int getpwnam_r(const char *s, struct passwd *p, char *b, size_t n, struct passwd **o) {
    int rc = ((nam_r)dlsym(RTLD_NEXT, "getpwnam_r"))(s, p, b, n, o);
    if (rc == 0 && o && *o) (*o)->pw_shell = shell;
    return rc;
}
struct passwd *getpwuid(uid_t u) {
    struct passwd *p = ((uid_f)dlsym(RTLD_NEXT, "getpwuid"))(u);
    if (p) p->pw_shell = shell;
    return p;
}
struct passwd *getpwnam(const char *s) {
    struct passwd *p = ((nam_f)dlsym(RTLD_NEXT, "getpwnam"))(s);
    if (p) p->pw_shell = shell;
    return p;
}
"#;

/// Detach, log in again (a new `ssh -A`, a new desktop session) and
/// reattach: the old agent socket is gone. Programs in the session must reach
/// the agent of the login that is attached now.
#[test]
#[ignore = "fails on main: the session keeps the SSH_AUTH_SOCK it was created with; a reattach from a new login does not bring the live agent socket"]
fn a_reattach_from_a_new_login_reaches_its_agent() {
    let root = Root::new();
    let first_dir = root.mkdir("a");
    let second_dir = root.mkdir("b");
    let first_agent = first_dir.join("agent.sock");
    let second_agent = second_dir.join("agent.sock");

    let listener = std::os::unix::net::UnixListener::bind(&first_agent).unwrap();
    let mut create = root.pty();
    create
        .env("PS1", "ready> ")
        .env("SSH_AUTH_SOCK", &first_agent)
        .args(["run", "-d", "--id", "agent", "--", "sh"]);
    run_ok(create);
    // The first login ends and its forwarded agent goes with it.
    drop(listener);
    std::fs::remove_file(&first_agent).unwrap();

    let _live = std::os::unix::net::UnixListener::bind(&second_agent).unwrap();
    let mut client = Session::spawn(
        &pty_bin(),
        &["attach", "agent"],
        SpawnOptions {
            rows: Some(24),
            cols: Some(120),
            env: vec![
                ("PTY_ROOT".into(), root.dir.to_string_lossy().into_owned()),
                ("SSH_AUTH_SOCK".into(), second_agent.to_string_lossy().into_owned()),
            ],
            ..Default::default()
        },
    )
    .expect("reattach from the new login");
    client.wait_for_text("ready> ", 8000).expect("the session's prompt");
    client.type_str(
        "if [ -S \"$SSH_AUTH_SOCK\" ]; then printf 'AGENT%s\\n' -LIVE; else printf 'AGENT%s\\n' -GONE; fi\r",
    );
    let shot = client.wait_for_text("AGENT-", 8000).expect("the probe answered");
    client.close();
    assert!(
        shot.text.contains("AGENT-LIVE"),
        "the session cannot reach the attached login's agent:\n{}",
        shot.text
    );
}

// ── The controlling terminal ──

/// A picker run inside `$(...)` (a fuzzy finder, a password prompt) reads its list on
/// stdin and talks to the person through `/dev/tty`: it opens it, switches it
/// to raw mode, draws, and reads a key. That needs a controlling terminal
/// and a foreground process group that includes the substitution — a
/// background one would be stopped by `SIGTTOU` the moment it touched the
/// modes, and hang.
#[test]
fn a_command_substitution_can_run_a_picker_on_dev_tty() {
    use_local_pty();
    let mut s = Session::server(
        "bash",
        &["--norc", "--noprofile", "-i"],
        ServerOptions {
            rows: Some(24),
            cols: Some(120),
            env: vec![
                ("PS1".into(), "ready> ".into()),
                ("HISTFILE".into(), "/dev/null".into()),
            ],
            ..Default::default()
        },
    )
    .expect("start a session");
    s.wait_for_text("ready> ", 8000).expect("prompt");
    s.type_str(
        // Raw mode first, then the prompt: once the prompt shows, a key
        // must not echo.
        "r=$(printf 'a\\nb\\nc\\n' | sh -c 'n=$(wc -l); exec 3<>/dev/tty; \
         saved=$(stty -g <&3); stty -icanon -echo min 1 time 0 <&3; printf \"PICK(%s)> \" \"$n\" >&3; \
         k=$(dd bs=1 count=1 <&3 2>/dev/null); stty \"$saved\" <&3; printf \"picked-%s\" \"$k\"'); \
         echo \"RESULT=[$r]\"\r",
    );
    s.wait_for_text("PICK(3)> ", 8000)
        .expect("the picker drew on /dev/tty");
    s.type_str("x");
    let shot = s
        .wait_for_text("RESULT=[picked-x]", 8000)
        .expect("the picker read a key from /dev/tty");
    assert!(
        shot.text.contains("PICK(3)> RESULT=[picked-x]"),
        "the key was echoed, so the picker's raw mode did not take:\n{}",
        shot.text
    );
    s.close();
}

// ── The argument vector ──

/// Everything after `--` belongs to the child, including words that look
/// like the runtime's own options.
#[test]
fn arguments_after_the_separator_reach_the_child_verbatim() {
    let root = Root::new();
    let out_file = root.path("argv.out");
    let child_args = [
        "--session", "foo", "--id", "x", "--cwd", "/nope", "-d", "--force", "--preselect-new",
        "--filter-tag", "k=v", "--env", "A=B", "--name", "n", "-a", "-e", "--", "--help", "-h",
        "", "two words", "$HOME",
    ];
    let mut cmd = root.pty();
    cmd.args(["run", "-d", "--id", "argv", "--", "/bin/sh", "-c"])
        .arg(format!(
            "printf '<%s>' \"$@\" > '{o}.tmp'; mv '{o}.tmp' '{o}'",
            o = out_file.display()
        ))
        .arg("sh")
        .args(child_args);
    run_ok(cmd);
    let got = wait_for_file(&out_file, Duration::from_secs(8));
    let want: String = child_args.iter().map(|a| format!("<{a}>")).collect();
    assert_eq!(got, want);
}

/// `--root` is the runtime's one global option. After `--` it is the child's.
#[test]
#[ignore = "fails on main: the global --root scan runs over the whole argv, so `--root <dir>` after `--` is removed from the child's arguments and the session is created in <dir> instead"]
fn a_root_option_after_the_separator_belongs_to_the_child() {
    let root = Root::new();
    let out_file = root.path("root-argv.out");
    let elsewhere = root.path("elsewhere");
    let elsewhere_s = elsewhere.to_string_lossy().into_owned();
    let mut cmd = root.pty();
    cmd.args(["run", "-d", "--id", "rootarg", "--", "/bin/sh", "-c"])
        .arg(format!(
            "printf '<%s>' \"$@\" > '{o}.tmp'; mv '{o}.tmp' '{o}'; exec sleep 30",
            o = out_file.display()
        ))
        .args(["sh", "--root", &elsewhere_s]);
    run_ok(cmd);
    let got = wait_for_file(&out_file, Duration::from_secs(8));
    assert_eq!(
        got,
        format!("<--root><{elsewhere_s}>"),
        "the child's arguments were consumed by the runtime"
    );
    assert!(
        root.path("rootarg.json").exists() && !elsewhere.exists(),
        "the session should be registered in its own root, not in the child's argument"
    );

    // A value that looks like an option must not stop the start either.
    let mut dash = root.pty();
    dash.args(["run", "-d", "--id", "rootdash", "--", "/bin/echo", "--root", "-n"]);
    let out = dash.output().unwrap();
    assert!(out.status.success(), "{}", text(&out).1);
}

// ── The working directory ──

/// The child starts in the directory it asked for, and `$PWD` agrees with
/// it — whatever directory the launcher or the daemon stood in, whatever
/// `PWD` the launcher had.
#[test]
fn the_child_starts_in_the_requested_directory_and_pwd_names_it() {
    if !have("python3") {
        eprintln!("skipped: python3 is not installed");
        return;
    }
    let root = Root::new();
    let launcher_dir = root.mkdir("launcher");
    let wanted = root.mkdir("work/project");
    let real = root.mkdir("real");
    let link = root.path("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let probe = "import os,sys; p=sys.argv[1]; open(p+'.tmp','w').write(os.environ.get('PWD','<unset>')+'\\n'+os.getcwd()+'\\n'); os.rename(p+'.tmp',p)";

    let start = |id: &str, cwd: &str, from: &Path| -> (String, String) {
        let out = root.path(&format!("{id}.cwd"));
        let mut cmd = root.pty();
        cmd.current_dir(from)
            .env("PWD", "/")
            .args(["run", "-d", "--id", id, "--cwd", cwd, "--", "python3", "-c", probe])
            .arg(&out);
        run_ok(cmd);
        let got = wait_for_file(&out, Duration::from_secs(8));
        let mut lines = got.lines();
        (
            lines.next().unwrap_or_default().to_string(),
            lines.next().unwrap_or_default().to_string(),
        )
    };

    // An absolute directory away from the launcher.
    let (pwd, cwd) = start("abs", &wanted.to_string_lossy(), &launcher_dir);
    assert_eq!(pwd, wanted.to_string_lossy());
    assert_eq!(cwd, wanted.to_string_lossy());

    // A relative directory, resolved against the launcher.
    let (pwd, cwd) = start("rel", "project", &root.path("work"));
    assert_eq!(pwd, wanted.to_string_lossy(), "PWD must be absolute and name the directory");
    assert_eq!(cwd, wanted.to_string_lossy());

    // Through a symlink: PWD is the path as written, the directory is the same.
    let (pwd, cwd) = start("link", &link.to_string_lossy(), &launcher_dir);
    assert_eq!(pwd, link.to_string_lossy());
    let same = |a: &str, b: &str| {
        let (a, b) = (std::fs::metadata(a).unwrap(), std::fs::metadata(b).unwrap());
        (a.dev(), a.ino()) == (b.dev(), b.ino())
    };
    assert!(same(&pwd, &cwd), "PWD {pwd} and the working directory {cwd} differ");
}

// ── Starting, and saying why not ──

/// The first session creates the state directory, owner only, whether it
/// comes from `PTY_ROOT` or the default under a fresh home.
#[test]
fn the_first_session_creates_its_state_directory() {
    let root = Root::new();
    let fresh = root.path("fresh/nested/state");
    let mut cmd = pty_at(&fresh);
    cmd.args(["run", "-d", "--id", "first", "--", "sleep", "30"]);
    run_ok(cmd);
    assert!(fresh.join("first.sock").exists());
    assert_eq!(std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777, 0o700);

    let home = root.mkdir("h");
    let mut cmd = pty_at(&root.dir);
    cmd.env_remove("PTY_ROOT")
        .env("HOME", &home)
        .args(["run", "-d", "--id", "second", "--", "sleep", "30"]);
    run_ok(cmd);
    let default_root = home.join(".local/state/pty");
    assert!(default_root.join("second.sock").exists());
    assert_eq!(
        std::fs::metadata(&default_root).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

/// A start that cannot happen fails at once with the reason, and leaves no
/// session behind.
#[test]
#[ignore = "fails on main: a missing working directory can produce a generic daemon-exited error instead of its cause"]
fn a_start_that_cannot_happen_says_why() {
    let root = Root::new();
    let file = root.path("a-file");
    std::fs::write(&file, "").unwrap();
    let cases: Vec<(Command, Vec<String>)> = vec![
        (
            {
                let mut c = root.pty();
                c.args(["run", "-d", "--id", "noshell", "--", "/no/such/shell"]);
                c
            },
            vec!["Command not found: /no/such/shell".into()],
        ),
        (
            {
                let mut c = root.pty();
                c.args(["run", "-d", "--id", "nobare", "--", "no-such-shell-anywhere"]);
                c
            },
            vec!["Command not found: no-such-shell-anywhere".into()],
        ),
        (
            {
                let mut c = root.pty();
                c.args(["run", "-d", "--id", "nocwd", "--cwd", "/no/such/dir", "--", "sleep", "30"]);
                c
            },
            vec!["Working directory does not exist: /no/such/dir".into()],
        ),
        (
            {
                let mut c = pty_at(&file.join("state"));
                c.args(["run", "-d", "--id", "nostate", "--", "sleep", "30"]);
                c
            },
            vec![file.join("state").to_string_lossy().into_owned(), "Not a directory".into()],
        ),
    ];
    for (mut cmd, wanted) in cases {
        let started = Instant::now();
        let out = cmd.output().expect("run pty");
        let (so, se) = text(&out);
        assert!(!out.status.success(), "should have failed: {so}{se}");
        for w in &wanted {
            assert!(se.contains(w.as_str()), "stderr should say {w:?}, got: {se}");
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the reason should come at once, not after a start timeout"
        );
        assert!(!se.contains("Timeout") && !se.contains("Timed out"), "{se}");
    }
    assert!(
        sessions(&root.dir).is_empty(),
        "a failed start must not leave a session behind"
    );
}

/// A login shell that does not exist, given to the picker's one-key
/// create, is reported rather than producing an empty session.
#[test]
fn the_picker_reports_a_login_shell_that_does_not_exist() {
    let root = Root::new();
    let home = root.mkdir("home");
    let bin = pty_bin();
    let mut picker = Session::spawn(
        &bin,
        &[] as &[&str],
        SpawnOptions {
            rows: Some(24),
            cols: Some(120),
            env: vec![
                ("PTY_ROOT".into(), root.dir.to_string_lossy().into_owned()),
                ("HOME".into(), home.to_string_lossy().into_owned()),
                ("SHELL".into(), "/nonexistent/zsh".into()),
            ],
            ..Default::default()
        },
    )
    .expect("start the picker");
    picker
        .wait_for_text("Create new session", 10_000)
        .expect("the picker's create row");
    picker.type_str("\r");
    picker
        .wait_for_text("Command not found: /nonexistent/zsh", 10_000)
        .expect("the picker names the missing shell");
    picker.close();
    assert!(sessions(&root.dir).is_empty(), "no empty session should be left");
}

/// A program that exists but cannot be executed — no execute bit, or an
/// interpreter that is not installed — is a start that failed, and the
/// person who asked for it must be told why.
#[test]
#[ignore = "fails on main: `pty run -d` reports `Session created` for a program that cannot be executed; the wrapper shell's exec error is shown to nobody and the reaped session vanishes"]
fn a_program_that_cannot_be_executed_is_refused_with_its_cause() {
    let root = Root::new();
    let not_executable = root.path("tool");
    std::fs::write(&not_executable, "#!/bin/sh\necho hi\n").unwrap();
    std::fs::set_permissions(&not_executable, std::fs::Permissions::from_mode(0o644)).unwrap();
    let missing_interpreter = root.path("script");
    std::fs::write(&missing_interpreter, "#!/no/such/interpreter\necho hi\n").unwrap();
    std::fs::set_permissions(&missing_interpreter, std::fs::Permissions::from_mode(0o755)).unwrap();

    for (id, program) in [("noexec", &not_executable), ("nointerp", &missing_interpreter)] {
        let mut cmd = root.pty();
        cmd.args(["run", "-d", "--id", id, "--"]).arg(program);
        let out = cmd.output().expect("run pty");
        let (so, se) = text(&out);
        // Give a session that did start the time to vanish, so the failure
        // message can say what was left to look at.
        std::thread::sleep(Duration::from_millis(500));
        let left = sessions(&root.dir).iter().any(|s| s.name == id);
        assert!(
            !out.status.success() && se.contains(&*program.to_string_lossy()),
            "{}: `pty run -d` said {:?} (exit {:?}, stderr {se:?}); session left to inspect: {left}",
            program.display(),
            so.trim(),
            out.status.code()
        );
    }
}

// ── The control socket ──

/// The session socket and its directory are owner only even when the
/// caller's umask would allow anybody, and no matter whether the directory
/// existed first.
#[test]
fn the_session_socket_is_owner_only_under_any_umask() {
    let root = Root::new();
    let me = std::fs::metadata(&root.dir).unwrap().uid();
    let fresh = root.path("fresh");
    let existing = root.mkdir("existing");
    std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o755)).unwrap();

    for (dir, id) in [(&fresh, "own1"), (&existing, "own2")] {
        let mut sh = Command::new("sh");
        for key in AMBIENT_RUNTIME_KEYS {
            sh.env_remove(key);
        }
        sh.env("PTY_ROOT", dir)
            .args(["-c", "umask 000; exec \"$0\" run -d --id \"$1\" -- sleep 30"])
            .arg(pty_bin())
            .arg(id)
            .stdin(Stdio::null());
        run_ok(sh);
        let sock = std::fs::metadata(dir.join(format!("{id}.sock"))).unwrap();
        assert_eq!(sock.permissions().mode() & 0o777, 0o600, "{id}.sock");
        assert_eq!(sock.uid(), me);
    }
    assert_eq!(std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777, 0o700);
}

/// Connections the daemon accepts are blocking, whatever the platform's
/// accept(2) inherits from the listener: a client that goes quiet is not
/// dropped for "no data yet".
#[test]
fn accepted_connections_survive_a_quiet_period() {
    use_local_pty();
    let mut s = Session::server(
        "sh",
        &["-c", "sleep 2; printf 'AFTER-QUIET\\n'; exec cat"],
        ServerOptions {
            rows: Some(24),
            cols: Some(80),
            ..Default::default()
        },
    )
    .expect("start a session");
    s.wait_for_text("AFTER-QUIET", 10_000)
        .expect("the client survived two quiet seconds");
    s.type_str("still-here\r");
    s.wait_for_text("still-here", 5000).expect("input after the quiet period");

    #[cfg(target_os = "linux")]
    {
        // The listener's flags are what a macOS accept(2) copies onto every
        // connection; neither it nor an accepted stream may be non-blocking.
        const O_NONBLOCK: u32 = 0o4000;
        let root = s.root().unwrap().to_path_buf();
        let pid = std::fs::read_to_string(root.join(format!("{}.pid", s.name())))
            .unwrap()
            .trim()
            .to_string();
        let mut sockets = 0;
        for entry in std::fs::read_dir(format!("/proc/{pid}/fd")).unwrap().flatten() {
            let Ok(target) = std::fs::read_link(entry.path()) else { continue };
            if !target.to_string_lossy().starts_with("socket:") {
                continue;
            }
            sockets += 1;
            let fd = entry.file_name().to_string_lossy().into_owned();
            let info = std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}")).unwrap();
            let flags = info
                .lines()
                .find_map(|l| l.strip_prefix("flags:"))
                .map(|v| u32::from_str_radix(v.trim(), 8).unwrap())
                .unwrap();
            assert_eq!(flags & O_NONBLOCK, 0, "daemon socket fd {fd} is non-blocking");
        }
        assert!(sockets >= 2, "expected the listener and this client's connection");
    }
    s.close();
}

/// A home directory long enough to push the default socket path past the
/// kernel's limit gets a clear refusal, not a crash; commands that only read
/// keep working.
#[test]
fn a_socket_path_past_the_kernel_limit_is_refused_clearly() {
    let root = Root::new();
    let home = root.mkdir(&"h".repeat(90));
    let mut cmd = pty_at(&root.dir);
    cmd.env_remove("PTY_ROOT")
        .env("HOME", &home)
        .args(["run", "-d", "--", "sleep", "30"]);
    let out = cmd.output().unwrap();
    let (so, se) = text(&out);
    assert_eq!(out.status.code(), Some(1), "{so}{se}");
    assert!(se.contains("byte kernel limit"), "{se}");
    assert!(!se.contains("panicked"), "{se}");

    let mut list = pty_at(&root.dir);
    list.env_remove("PTY_ROOT").env("HOME", &home).args(["list", "--json"]);
    let out = list.output().unwrap();
    assert!(out.status.success(), "{}", text(&out).1);
    assert!(!text(&out).1.contains("panicked"));
}

// ── Where state lives ──

/// Hosts that mount the same home directory (NFS, a VM share) must not see
/// each other's sockets and session records: a unix socket cannot be
/// connected across hosts, so each host would take the other's live
/// sessions for dead ones and sweep or reuse them.
#[test]
#[ignore = "fails on main: the default registry (sockets, pids, locks) is $HOME/.local/state/pty, shared by every host that mounts the home directory, with nothing host-specific in the path"]
fn session_sockets_do_not_live_in_a_shared_home_directory() {
    let root = Root::new();
    let home = root.mkdir("home");
    let runtime = root.mkdir("run");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut cmd = pty_at(&root.dir);
    cmd.env_remove("PTY_ROOT")
        .env_remove("XDG_STATE_HOME")
        .env("HOME", &home)
        .env("XDG_RUNTIME_DIR", &runtime)
        .args(["run", "-d", "--id", "where", "--", "sleep", "30"]);
    run_ok(cmd);

    fn find(dir: &Path, file: &str, depth: u32) -> Option<PathBuf> {
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if entry.file_name() == file {
                return Some(path);
            }
            if depth > 0 && entry.file_type().is_ok_and(|t| t.is_dir())
                && let Some(found) = find(&path, file, depth - 1)
            {
                return Some(found);
            }
        }
        None
    }
    let socket = find(&root.dir, "where.sock", 6).expect("the session's socket");
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap_or_default()
        .trim()
        .to_string();
    let host_specific = !host.is_empty() && socket.to_string_lossy().contains(&host);
    assert!(
        !socket.starts_with(&home) || host_specific,
        "the socket {} lives in the (shareable) home directory with nothing host-specific in its path",
        socket.display()
    );
}

// ── Session names ──

/// Names are matched exactly: a name that differs only in case is a
/// different session, and no command reaches one through the other.
#[test]
fn a_name_that_differs_only_in_case_never_reaches_another_session() {
    let root = Root::new();
    let mut create = root.pty();
    create.args(["run", "-d", "--id", "Foo", "--tag", "keep=true", "--", "sh", "-c", "echo Foo-record"]);
    run_ok(create);
    assert!(wait_until(Duration::from_secs(8), || {
        sessions(&root.dir)
            .iter()
            .any(|s| s.name == "Foo" && s.status != SessionStatus::Running)
    }));
    let foo_record = || std::fs::read_to_string(root.path("Foo.json")).unwrap_or_default();
    assert!(foo_record().contains("Foo-record"));

    for verb in ["rm", "kill", "peek"] {
        let mut cmd = root.pty();
        cmd.args([verb, "foo"]);
        let out = cmd.output().unwrap();
        assert!(!out.status.success(), "`pty {verb} foo` should not find Foo");
        assert!(foo_record().contains("Foo-record"), "`pty {verb} foo` touched Foo");
    }
}

/// On a case-insensitive filesystem (the macOS default), `foo.json` and
/// `Foo.json` are one file. Removing or creating `foo` must not delete or
/// overwrite `Foo`'s saved state.
#[cfg(target_os = "macos")]
#[test]
fn case_variant_names_do_not_share_state_on_a_case_insensitive_filesystem() {
    let root = Root::new();
    let mut create = root.pty();
    create.args(["run", "-d", "--id", "Foo", "--tag", "keep=true", "--", "sh", "-c", "echo Foo-record"]);
    run_ok(create);
    assert!(wait_until(Duration::from_secs(8), || {
        sessions(&root.dir)
            .iter()
            .any(|s| s.name == "Foo" && s.status != SessionStatus::Running)
    }));
    let foo_record = || {
        std::fs::read_dir(&root.dir)
            .unwrap()
            .flatten()
            .find(|e| e.file_name() == "Foo.json")
            .map(|e| std::fs::read_to_string(e.path()).unwrap_or_default())
            .unwrap_or_default()
    };
    let mut rm = root.pty();
    rm.args(["rm", "foo"]);
    let _ = rm.output();
    assert!(foo_record().contains("Foo-record"), "`pty rm foo` deleted Foo's state");

    let mut other = root.pty();
    other.args(["run", "-d", "--id", "foo", "--", "sleep", "30"]);
    let _ = other.output();
    assert!(
        foo_record().contains("Foo-record"),
        "creating `foo` overwrote Foo's saved state"
    );
}

// ── macOS login-session context ──

/// A session's programs keep the launchd session and user identity of the
/// terminal that started it (Aqua, not Background), so the keychain, DNS,
/// user lookups and privacy grants work as they do in that terminal.
#[cfg(target_os = "macos")]
#[test]
fn a_session_keeps_the_login_session_context_it_was_started_from() {
    let root = Root::new();
    let here = Command::new("sh")
        .args(["-c", "launchctl managername; id -un"])
        .output()
        .unwrap();
    let here = String::from_utf8_lossy(&here.stdout).into_owned();
    let dump = root.path("ctx.out");
    let mut cmd = root.pty();
    cmd.args(["run", "-d", "--id", "ctx", "--", "sh", "-c"]).arg(format!(
        "(launchctl managername; id -un) > '{d}.tmp' 2>&1; mv '{d}.tmp' '{d}'; exec sleep 30",
        d = dump.display()
    ));
    run_ok(cmd);
    let inside = wait_for_file(&dump, Duration::from_secs(8));
    assert_eq!(inside, here, "the session runs in a different login-session context");
}
