//! Spawning a session daemon and waiting for it to publish, the way Node's
//! `spawnDaemon` does: one detached child (`<self> __daemon`), the config as
//! JSON on inherited fd 3 (never argv), stderr collected for the early-exit
//! message, then the socket, matching metadata plus `session_start`, and the
//! daemon's post-child-spawn readiness acknowledgement.
//!
//! node: src/spawn.ts:129-260

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::io::{FromRawFd, RawFd};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use pty_core::registry::{self, EnvMap, TagMap};

use super::config::DaemonConfig;
use crate::StartupLeaseOptions;

/// `DEFAULT_START_TIMEOUT_MS`.
pub const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(30);

/// Inherited descriptor on which the daemon acknowledges that its child and
/// serving threads have started. Kept out of the public session registry: the
/// owner record and `session_start` must still precede the attached child.
pub const READY_FD_ENV: &str = "PTY_DAEMON_READY_FD";

/// Node's `SpawnDaemonOptions`, minus the launcher/server-module knobs.
/// `command` is already resolved absolute (`pty_core::spawn::resolve_command`).
/// The launcher executable is supplied separately to [`spawn_daemon`].
#[derive(Debug, Clone, Default)]
pub struct SpawnParams {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub display_command: String,
    pub cwd: String,
    pub rows: u16,
    pub cols: u16,
    pub ephemeral: bool,
    /// Written to the config only when non-empty.
    pub tags: TagMap,
    pub display_name: Option<String>,
    pub isolate_env: bool,
    /// Written only when non-empty.
    pub extra_env: EnvMap,
    /// Selected inherited values retained across permanent-session respawns.
    pub session_env: Option<EnvMap>,
    /// Written only when non-empty.
    pub unset_env: Vec<String>,
    /// The verbatim replacement environment; exclusive with the three above.
    pub env: Option<EnvMap>,
    /// Daemon-owned startup deadline and lifecycle tag.
    pub startup_lease: Option<StartupLeaseOptions>,
    /// Variables removed from the daemon's (and so the child's) environment.
    pub scrub_env: Vec<String>,
    /// The pid that already holds `<name>.lock` for this creation; carried
    /// for the CLI-fallback path (`PTY_CREATION_LOCK_OWNER_PID`).
    pub creation_lock_owner_pid: Option<i32>,
    /// Set `PTY_SPAWNER_PID` so the daemon shuts down when this process dies.
    pub bind_to_spawner_lifetime: bool,
    /// Override of the 30 s start budget.
    pub start_timeout: Option<Duration>,
    /// Ask the daemon to publish `session_respawn` with `session_start`.
    pub respawn: bool,
}

impl SpawnParams {
    /// Parameters for `command` with the caller's terminal size (or 24×80).
    pub fn new(name: &str, command: &str, args: &[String]) -> Self {
        let (rows, cols) = pty_client::tty::size_or_default(libc::STDOUT_FILENO);
        Self {
            name: name.to_string(),
            command: command.to_string(),
            args: args.to_vec(),
            display_command: std::iter::once(command.to_string())
                .chain(args.iter().cloned())
                .collect::<Vec<_>>()
                .join(" "),
            cwd: std::env::current_dir()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "/".to_string()),
            rows,
            cols,
            ..Self::default()
        }
    }
}
/// Carry a session's launch-time settings into its replacement. Older
/// records simply have fewer of them.
///
/// node: src/cli.ts:3874-3884 (`persistedLaunchOptions`)
pub fn apply_persisted_launch_options(params: &mut SpawnParams, meta: &registry::SessionMetadata) {
    if let Some(rows) = meta.rows {
        params.rows = rows;
    }
    if let Some(cols) = meta.cols {
        params.cols = cols;
    }
    if let Some(ephemeral) = meta.ephemeral {
        params.ephemeral = ephemeral;
    }
    if meta.isolate_env == Some(true) {
        params.isolate_env = true;
    }
    if let Some(extra) = &meta.extra_env
        && !extra.is_empty()
    {
        params.extra_env = extra.clone();
    }
    params.session_env = meta.session_env.clone();
    if let Some(unset) = &meta.unset_env
        && !unset.is_empty()
    {
        params.unset_env = unset.clone();
    }
    if let Some(env) = &meta.env {
        params.env = Some(env.clone());
    }
}

/// What a successful spawn learned about the daemon it started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnedDaemon {
    pub pid: u32,
    pub generation: String,
}

/// A failed spawn, with Node's message as its `Display`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnError {
    /// `env` together with `isolate_env` / `extra_env` / `unset_env`.
    EnvExclusive,
    /// The daemon process died before becoming ready.
    DaemonExited { code: Option<i32>, stderr: String },
    /// No socket within the budget.
    SocketTimeout { name: String },
    /// A socket, but no matching publication and readiness acknowledgement
    /// within the budget.
    PublicationTimeout { name: String },
    /// The session was published, by somebody else. This attempt cannot win,
    /// so there is nothing to wait for.
    AlreadyPublished { name: String },
    /// Spawning the process itself failed.
    Io(String),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::EnvExclusive => write!(
                f,
                "SpawnDaemonOptions.env is mutually exclusive with isolateEnv/extraEnv/unsetEnv. \
                 Use env for verbatim control, or inherited environment policy options — not both."
            ),
            SpawnError::DaemonExited { code, stderr } => {
                let code = code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                let msg = format!("Daemon process exited immediately (code {code}).");
                if stderr.is_empty() {
                    write!(f, "{msg} Is the command valid?")
                } else {
                    write!(f, "{msg}\n{stderr}")
                }
            }
            SpawnError::SocketTimeout { name } => {
                write!(f, "Timeout waiting for session \"{name}\" to start")
            }
            SpawnError::PublicationTimeout { name } => write!(
                f,
                "Timed out waiting for daemon publication for session \"{name}\"."
            ),
            // The same sentence `pty run` prints when it sees a running session
            // before it spawns anything. Losing the race later should not
            // produce a different explanation of the same situation.
            SpawnError::AlreadyPublished { name } => write!(
                f,
                "Session \"{name}\" is already running. Use \"pty attach {name}\" to connect."
            ),
            SpawnError::Io(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for SpawnError {}


/// The config object Node's spawner serializes, in its key order.
///
/// node: src/spawn.ts:169-184
pub fn config_for(params: &SpawnParams) -> DaemonConfig {
    DaemonConfig {
        name: params.name.clone(),
        command: params.command.clone(),
        args: params.args.clone(),
        display_command: params.display_command.clone(),
        cwd: Some(params.cwd.clone()),
        rows: Some(params.rows),
        cols: Some(params.cols),
        ephemeral: params.ephemeral,
        tags: (!params.tags.is_empty()).then(|| params.tags.clone()),
        display_name: params.display_name.clone().filter(|d| !d.is_empty()),
        isolate_env: params.isolate_env.then_some(true),
        extra_env: (!params.extra_env.is_empty()).then(|| params.extra_env.clone()),
        session_env: params.session_env.clone(),
        unset_env: (!params.unset_env.is_empty()).then(|| params.unset_env.clone()),
        env: params.env.clone(),
        startup_lease: params.startup_lease.clone(),
        generation: None,
        respawn: params.respawn,
    }
}

/// Name this process for `ps`, `top` and `/proc/<pid>/comm`.
///
/// **Linux only, and that is a gap rather than a decision.** On Linux this is
/// `prctl(PR_SET_NAME)`, which moves the name `ps -o comm=` reads.
///
/// **On macOS the daemon still shows the binary's whole path, where the Node
/// tool shows `pty-daemon`.** Two attempts are recorded here so nobody
/// repeats them:
///
/// - **"macOS has no equivalent" — false.** This file said so until
///   2026-09-02 and the Node tool disproves it.
/// - **`pthread_setname_np` — compiles, and does not do this job.** Apple's
///   one-argument form names the calling THREAD, for debuggers and
///   Instruments, and macOS does not surface thread names through `ps` at
///   all. Tried on 2026-09-02: `comm`, `ucomm` and `args` were all unchanged.
///
/// **What Node actually does is rewrite the process-argument region**, which
/// is where macOS `ps -o comm=` reads its answer. Its daemon's `args` field
/// is the bare string `pty-daemon` rather than a command line, which is the
/// tell. Doing the same here means overwriting the memory `argv` points at,
/// in place, within the space the kernel gave us — and getting that wrong
/// corrupts the arguments of a live process.
///
/// **So it is left undone rather than half done.** It is cosmetic: it changes
/// what `ps` and `top` display and nothing reads it. `docs/parity.md` records
/// it as absent.
pub fn set_process_title(title: &str) {
    #[cfg(target_os = "linux")]
    {
        let truncated: String = title.chars().take(15).collect();
        if let Ok(c) = std::ffi::CString::new(truncated) {
            // SAFETY: PR_SET_NAME reads a NUL-terminated string.
            unsafe {
                libc::prctl(libc::PR_SET_NAME, c.as_ptr() as libc::c_ulong, 0, 0, 0);
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = title;
    }
}

/// Node's `hasPublishedSessionStart`: a `session_start` line for `name`
/// whose `ts >= created_at` (string comparison, as Node does).
///
/// node: src/spawn.ts:245-260
pub fn has_published_session_start(name: &str, created_at: &str) -> bool {
    let Ok(content) = std::fs::read_to_string(registry::events_path(name)) else {
        return false;
    };
    content.trim_end().split('\n').any(|line| {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            return false;
        };
        v.get("session").and_then(|s| s.as_str()) == Some(name)
            && v.get("type").and_then(|t| t.as_str()) == Some("session_start")
            && v.get("ts")
                .and_then(|t| t.as_str())
                .map(|ts| ts >= created_at)
                .unwrap_or(false)
    })
}

/// Return the generation from the metadata snapshot whose daemon pid and
/// `session_start` publication match the process we spawned.
fn published_generation_by(name: &str, pid: u32) -> Option<String> {
    let meta = registry::read_metadata(name)?;
    (meta.daemon_pid == Some(pid as i32) && has_published_session_start(name, &meta.created_at))
        .then(|| meta.generation.unwrap_or_default())
}

/// The daemon side of the launcher's one-byte readiness pipe.
pub struct ReadyNotifier(Option<File>);

impl ReadyNotifier {
    pub fn from_process() -> Self {
        let Some(fd) = std::env::var(READY_FD_ENV)
            .ok()
            .and_then(|raw| raw.parse::<RawFd>().ok())
            .filter(|fd| *fd >= 0)
        else {
            return Self(None);
        };
        // The launcher names an inherited descriptor. Validate it before
        // taking ownership, then keep it out of the session child.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            return Self(None);
        }
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            Self(Some(File::from_raw_fd(fd)))
        }
    }

    pub fn notify(mut self) {
        if let Some(mut pipe) = self.0.take() {
            let _ = pipe.write_all(&[1]);
        }
    }
}

struct ChildWatch {
    child: std::process::Child,
    stderr: std::sync::Arc<std::sync::Mutex<String>>,
    exit_code: Option<Option<i32>>,
    readiness: Receiver<bool>,
}

impl ChildWatch {
    fn check_early_exit(&mut self) -> Result<(), SpawnError> {
        if self.exit_code.is_none()
            && let Ok(Some(status)) = self.child.try_wait()
        {
            self.exit_code = Some(status.code());
        }
        if let Some(code) = self.exit_code {
            let stderr = self
                .stderr
                .lock()
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            return Err(SpawnError::DaemonExited { code, stderr });
        }
        Ok(())
    }
}

/// Spawn `<daemon_executable> __daemon` detached and wait until it has
/// published the session. On any failure after the process started (and it has
/// not already exited) the daemon is sent SIGTERM.
///
/// The executable is explicit so a library caller's own `current_exe` is never
/// assumed to implement the private `__daemon` entrypoint.
///
/// node: src/spawn.ts:164-243
pub fn spawn_daemon(
    daemon_executable: &std::path::Path,
    params: SpawnParams,
) -> Result<SpawnedDaemon, SpawnError> {
    if params.env.is_some()
        && (params.isolate_env || !params.extra_env.is_empty() || !params.unset_env.is_empty())
    {
        return Err(SpawnError::EnvExclusive);
    }
    let name = params.name.clone();
    let timeout = params.start_timeout.unwrap_or(DEFAULT_START_TIMEOUT);
    let config = serde_json::to_string(&config_for(&params))
        .map_err(|e| SpawnError::Io(format!("cannot encode daemon config: {e}")))?;

    // The config travels on a pipe the child inherits as fd 3.
    let mut fds = [0i32; 2];
    // SAFETY: pipe(2) fills two descriptors.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(SpawnError::Io(format!(
            "cannot create config pipe: {}",
            std::io::Error::last_os_error()
        )));
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);
    // SAFETY: freshly created descriptors owned here.
    let mut write_end = unsafe { std::fs::File::from_raw_fd(write_fd) };
    // The write end must not leak into the daemon (it would keep its own
    // config pipe open for ever).
    // SAFETY: fcntl on a descriptor we own.
    unsafe {
        libc::fcntl(write_fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }

    // A second pipe distinguishes registry publication from a daemon that is
    // actually ready. Publication intentionally happens before the session
    // child starts; the daemon writes this pipe only after that spawn and the
    // serving threads succeed.
    let mut ready_fds = [0i32; 2];
    // SAFETY: pipe(2) fills two descriptors.
    if unsafe { libc::pipe(ready_fds.as_mut_ptr()) } != 0 {
        let error = std::io::Error::last_os_error();
        // SAFETY: the config read descriptor is still owned by this process.
        unsafe {
            libc::close(read_fd);
        }
        return Err(SpawnError::Io(format!(
            "cannot create readiness pipe: {error}"
        )));
    }
    let (ready_read_fd, ready_write_fd) = (ready_fds[0], ready_fds[1]);
    // SAFETY: both descriptors are freshly created and owned here.
    let ready_read = unsafe { File::from_raw_fd(ready_read_fd) };
    // The daemon needs only the write end.
    unsafe {
        libc::fcntl(ready_read_fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }

    let mut cmd = Command::new(daemon_executable);
    cmd.arg("__daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .env_remove("PTY_SERVER_CONFIG");
    for key in &params.scrub_env {
        cmd.env_remove(key);
    }
    cmd.env(READY_FD_ENV, ready_write_fd.to_string());
    if params.bind_to_spawner_lifetime {
        cmd.env("PTY_SPAWNER_PID", std::process::id().to_string());
    }
    match params.creation_lock_owner_pid {
        Some(pid) => {
            cmd.env("PTY_CREATION_LOCK_OWNER_PID", pid.to_string());
        }
        None => {
            cmd.env_remove("PTY_CREATION_LOCK_OWNER_PID");
        }
    }
    // SAFETY: the pre_exec body only calls async-signal-safe functions
    // (setsid, dup2, close).
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            libc::setsid();
            if read_fd != super::config::CONFIG_FD {
                if libc::dup2(read_fd, super::config::CONFIG_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(read_fd);
            } else {
                // Already fd 3: clear CLOEXEC so it survives the exec.
                libc::fcntl(read_fd, libc::F_SETFD, 0);
            }
            Ok(())
        });
    }
    let spawned = cmd.spawn();
    // SAFETY: the parent's copies of the inherited descriptors close here
    // regardless of whether spawning succeeded.
    unsafe {
        libc::close(read_fd);
        libc::close(ready_write_fd);
    }
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => return Err(SpawnError::Io(format!("failed to start daemon: {e}"))),
    };

    // Hand the config over from a helper thread so a config larger than the
    // pipe buffer cannot wedge this side if the daemon dies early.
    let config_bytes = config.into_bytes();
    std::thread::spawn(move || {
        let _ = write_end.write_all(&config_bytes);
        let _ = write_end.flush();
        drop(write_end);
    });

    let (ready_tx, ready_rx) = channel();
    std::thread::spawn(move || {
        let mut ready_read = ready_read;
        let mut byte = [0u8; 1];
        let ready = ready_read.read_exact(&mut byte).is_ok() && byte == [1];
        let _ = ready_tx.send(ready);
    });

    let stderr_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    if let Some(mut stderr) = child.stderr.take() {
        let buf = stderr_buf.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match stderr.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Ok(mut b) = buf.lock() {
                            b.push_str(&String::from_utf8_lossy(&chunk[..n]));
                        }
                    }
                }
            }
        });
    }
    let pid = child.id();
    let mut watch = ChildWatch {
        child,
        stderr: stderr_buf,
        exit_code: None,
        readiness: ready_rx,
    };
    let outcome = wait_for_publication(&name, pid, timeout, &mut watch);
    if let Err(err) = &outcome
        && !matches!(err, SpawnError::DaemonExited { .. })
    {
        // SAFETY: SIGTERM to the pid we spawned.
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    // Keep a waiter alive for this direct child. Detaching changes its session,
    // not its parent; without waitpid an embedding process accumulates zombies.
    std::thread::spawn(move || {
        let _ = watch.child.wait();
    });
    outcome
}

fn wait_for_publication(
    name: &str,
    pid: u32,
    timeout: Duration,
    watch: &mut ChildWatch,
) -> Result<SpawnedDaemon, SpawnError> {
    let started = Instant::now();
    wait_for_socket(name, timeout, || watch.check_early_exit())?;
    loop {
        if let Some(generation) = published_generation_by(name, pid)
            && matches!(watch.readiness.try_recv(), Ok(true))
        {
            return Ok(SpawnedDaemon { pid, generation });
        }
        // **Read the fact that is already there before waiting for one that is
        // not.** If somebody else has published this name, this attempt can
        // never win: `published_generation_by` compares against our own pid
        // and will be false for the rest of the budget. The only other way out
        // of this loop is noticing our own daemon die, so when that is slow —
        // a loaded machine, a daemon still starting up — the loop spends the
        // whole `DEFAULT_START_TIMEOUT` and then reports a timeout, when the
        // true answer was on disk in the first iteration.
        //
        // Measured on a Mac by `Silber.pty` on 2026-09-03: the losing
        // `pty run` took 30.06 s against a 30 s budget and said
        // "Timed out waiting for daemon publication" instead of
        // "is already running".
        if published_by_another(name, pid) {
            return Err(SpawnError::AlreadyPublished {
                name: name.to_string(),
            });
        }
        watch.check_early_exit()?;
        if started.elapsed() >= timeout {
            return Err(SpawnError::PublicationTimeout {
                name: name.to_string(),
            });
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Has this session been published by a live process that is not us?
///
/// All three conditions matter. **Published**, or we would refuse a name whose
/// metadata is still being written. **By a different pid**, or we would refuse
/// our own success. **By a live one**, or stale metadata from a daemon that
/// died would make the name permanently unusable.
///
/// "Live" here means `has_process_exited_for_reap`, never `pid_alive`. **A
/// zombie answers `kill(pid, 0)`**, measured on Linux 2026-09-03, so the cheap
/// predicate calls a corpse live. An unreaped daemon is the precise case this
/// has to get right.
pub fn published_by_another(name: &str, mine: u32) -> bool {
    let meta = registry::read_metadata(name);
    published_elsewhere(
        meta.as_ref().and_then(|m| m.daemon_pid),
        mine,
        // **NOT `pid_alive`.** A zombie answers `kill(pid, 0)`, so the cheap
        // predicate calls a corpse live — and a corpse recorded as the owner
        // would make this session name refuse every future `pty run`, which is
        // precisely the failure this check exists to avoid.
        |pid| !registry::has_process_exited_for_reap(pid),
        || {
            meta.as_ref()
                .is_some_and(|m| has_published_session_start(name, &m.created_at))
        },
    )
}

/// The decision, without the registry, so all four ways of answering "no" can
/// be tested rather than raced for.
fn published_elsewhere(
    owner: Option<i32>,
    mine: u32,
    alive: impl Fn(i32) -> bool,
    published: impl Fn() -> bool,
) -> bool {
    match owner {
        Some(owner) => owner != mine as i32 && alive(owner) && published(),
        None => false,
    }
}

/// Node's `waitForSocket`: stat every 50 ms, then settle 100 ms once the
/// socket exists. `early_check` runs before every probe.
///
/// node: src/spawn.ts:336-370
pub fn wait_for_socket(
    name: &str,
    timeout: Duration,
    mut early_check: impl FnMut() -> Result<(), SpawnError>,
) -> Result<(), SpawnError> {
    let socket = registry::socket_path(name);
    let start = Instant::now();
    loop {
        early_check()?;
        if start.elapsed() > timeout {
            return Err(SpawnError::SocketTimeout {
                name: name.to_string(),
            });
        }
        if std::fs::metadata(&socket).is_ok() {
            std::thread::sleep(Duration::from_millis(100));
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_texts_match_node() {
        assert_eq!(
            SpawnError::DaemonExited {
                code: Some(1),
                stderr: "boom".into()
            }
            .to_string(),
            "Daemon process exited immediately (code 1).\nboom"
        );
        assert_eq!(
            SpawnError::DaemonExited {
                code: None,
                stderr: String::new()
            }
            .to_string(),
            "Daemon process exited immediately (code unknown). Is the command valid?"
        );
        assert_eq!(
            SpawnError::SocketTimeout { name: "id".into() }.to_string(),
            "Timeout waiting for session \"id\" to start"
        );
        assert_eq!(
            SpawnError::PublicationTimeout { name: "id".into() }.to_string(),
            "Timed out waiting for daemon publication for session \"id\"."
        );
        // Deliberately the same sentence `pty run` prints before it spawns.
        assert_eq!(
            SpawnError::AlreadyPublished { name: "id".into() }.to_string(),
            "Session \"id\" is already running. Use \"pty attach id\" to connect."
        );
    }

    /// Every way of answering "no", so the one way of answering "yes" means
    /// something. Raced for, only the last of these would ever be exercised.
    #[test]
    fn only_a_live_different_and_published_owner_counts() {
        let live = |_: i32| true;
        let dead = |_: i32| false;
        let yes = || true;
        let no = || false;

        assert!(
            published_elsewhere(Some(200), 100, live, yes),
            "a live, different, published owner is somebody else's session"
        );
        assert!(
            !published_elsewhere(Some(100), 100, live, yes),
            "our own pid is not somebody else"
        );
        assert!(
            !published_elsewhere(Some(200), 100, dead, yes),
            "a dead owner would make the name permanently unusable"
        );

        // The predicate the production path actually passes, against a real
        // corpse. `pid_alive` would say true here and refuse the name forever.
        let mut child = std::process::Command::new("true").spawn().expect("spawn");
        let corpse = child.id() as i32;
        for _ in 0..500 {
            if registry::has_process_exited_for_reap(corpse) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            registry::pid_alive(corpse),
            "precondition: an unreaped child still answers kill(pid, 0)"
        );
        assert!(
            !published_elsewhere(
                Some(corpse),
                100,
                |pid| !registry::has_process_exited_for_reap(pid),
                yes
            ),
            "a zombie daemon must not make the session name unusable"
        );
        let _ = child.wait();
        assert!(
            !published_elsewhere(Some(200), 100, live, no),
            "metadata still being written is not a published session"
        );
        assert!(
            !published_elsewhere(None, 100, live, yes),
            "no owner recorded is not somebody else"
        );
    }

    #[test]
    fn config_follows_the_spawner_shape() {
        let mut tags = TagMap::new();
        tags.insert("k".into(), "v".into());
        let params = SpawnParams {
            name: "n".into(),
            command: "/bin/sh".into(),
            args: vec!["-c".into(), "true".into()],
            display_command: "sh -c true".into(),
            cwd: "/tmp".into(),
            rows: 24,
            cols: 80,
            tags,
            display_name: Some(String::new()),
            ..Default::default()
        };
        let value = serde_json::to_value(config_for(&params)).unwrap();
        assert_eq!(
            value,
            serde_json::from_str::<serde_json::Value>(
                r#"{"name":"n","command":"/bin/sh","args":["-c","true"],"displayCommand":"sh -c true","cwd":"/tmp","rows":24,"cols":80,"ephemeral":false,"tags":{"k":"v"}}"#
            )
            .unwrap()
        );
        let mut respawn = params;
        respawn.respawn = true;
        let json = serde_json::to_string(&config_for(&respawn)).unwrap();
        assert!(json.ends_with(r#","respawn":true}"#));
    }

    #[test]
    fn env_exclusivity_is_checked_before_spawning() {
        let params = SpawnParams {
            name: "n".into(),
            env: Some(EnvMap::new()),
            isolate_env: true,
            ..Default::default()
        };
        assert_eq!(
            spawn_daemon(std::path::Path::new("/not-used"), params).unwrap_err(),
            SpawnError::EnvExclusive
        );
    }
}
