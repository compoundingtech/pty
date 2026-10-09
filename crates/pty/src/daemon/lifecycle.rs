//! The daemon's life: spawn the child, publish the session, serve clients,
//! record the exit, shut down.
//!
//! Everything that touches the terminal runs on one thread — this one. The
//! PTY reader, the child waiter, the listener, every client socket, the
//! signal handler and the spawner watchdog only send [`Msg`]s here; timers
//! are deadlines the loop wakes for with `recv_timeout`.
//!
//! node: src/server.ts:323-690 (constructor), 571-598 (exit), 1295-1337
//! (exit metadata), 1340-1456 (close, watchdog), 1458-1616 (entry, shutdown)

use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pty_core::events::{Event, EventWriter};
use pty_core::protocol::{
    AcceptedSocketOwnershipResult, LifecycleCompareAndSetRequest, LifecycleCompareAndSetResult,
    Packet, PacketReader, decode_accepted_socket_ownership_request,
    decode_lifecycle_compare_and_set_request, encode_accepted_socket_ownership_response,
    encode_data, encode_exit, encode_lifecycle_compare_and_set_response,
};
use pty_core::registry::{
    self, MutateOptions, MutateStatus, SESSION_EXIT_LAST_LINES_LIMIT, SessionGenerationOwner,
    SessionMetadata, TagCompareAndSetResult, TagMap,
};
use pty_spawn::substrate::{ExitStatus, Lifecycle, SessionEvent, SessionOwner, SessionRef};
use pty_terminal::{TerminalActor, serialize};

use super::DaemonConfig;
use super::clients::{Client, ClientFacts, Out, REDRAW_SETTLE, Role};
use super::daemon_warn;
use super::env::{build_child_env, describe_invalid_cwd, invalid_cwd_error, permanent_respawn_env};
use pty_core::process_tree::{
    KILL_WAIT, ProcTable, ProcessIdentity, TERM_WAIT, TreeSnapshot, complete_snapshot_from_table,
    freeze_descendants, signal_process_identities, terminate_process_group,
    terminate_process_identities, terminate_process_identities_after_hangup,
};
use pty_lifecycle::{
    ArmedStartupLease, StartupLeaseTerminalCause, arm_startup_lease, monotonic_now_ns,
    remaining_lease_delay, startup_lease_deadline_cause, terminal_startup_lease_value,
};

/// What the helper threads tell the actor.
pub(crate) enum Msg {
    PtyData(Vec<u8>),
    PtyEof,
    /// The typed child status from the substrate reaper.
    ChildExited(ExitStatus),
    Connect {
        id: u64,
        tx: Sender<Out>,
        disconnect: UnixStream,
        write_lock: Arc<Mutex<()>>,
    },
    Packet {
        id: u64,
        packet: Packet,
    },
    Closed {
        id: u64,
    },
    /// SIGTERM, SIGINT, or the spawner watchdog.
    ExternalKill,
}

/// Pull pending client/control work ahead of terminal output without changing
/// the order of output, EOF and child-exit notifications from the bridge.
/// Moving packets is cheap; parsing and rendering each one is not.
fn next_queued_message(
    rx: &Receiver<Msg>,
    deferred: &mut VecDeque<Msg>,
    timeout: Option<Duration>,
) -> Result<Msg, RecvTimeoutError> {
    // Bound one scan even if the child can produce output faster than this
    // thread can drain the channel. The next turn resumes the scan.
    for _ in 0..8192 {
        match rx.try_recv() {
            Ok(msg @ (Msg::PtyData(_) | Msg::PtyEof | Msg::ChildExited(_))) => {
                deferred.push_back(msg);
            }
            Ok(control) => return Ok(control),
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                return deferred.pop_front().ok_or(RecvTimeoutError::Disconnected);
            }
        }
    }
    if let Some(msg) = deferred.pop_front() {
        return Ok(msg);
    }
    match timeout {
        Some(wait) => rx.recv_timeout(wait),
        None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
    }
}

/// Grace after the child's exit before the daemon shuts down, so attached
/// clients receive EXIT.
pub const EXIT_GRACE: Duration = Duration::from_millis(500);
/// How long a child's last output may trail its exit status before the
/// exit is recorded without waiting for the PTY to close.
const EXIT_DRAIN: Duration = Duration::from_millis(300);
/// `saveExitMetadata` retry budget at exit time.
const EXIT_METADATA_RETRY: Duration = Duration::from_millis(400);
/// `saveExitMetadataUntilSettled` budget at close time.
const EXIT_METADATA_SETTLE: Duration = Duration::from_millis(2_000);
/// How long `close()` waits for the child after SIGHUP.
const CHILD_HUP_WAIT: Duration = Duration::from_millis(2_000);
/// Grace for foreground jobs to handle the terminal's hangup before TERM.
const DESCENDANT_HUP_WAIT: Duration = Duration::from_millis(1_000);
/// …and after SIGKILL.
const CHILD_KILL_WAIT: Duration = Duration::from_millis(500);
/// `SPAWNER_POLL_INTERVAL_MS`.
const SPAWNER_POLL: Duration = Duration::from_millis(5_000);
/// The default `PTY_SHUTDOWN_DEADLINE_MS`.
const SHUTDOWN_DEADLINE_DEFAULT_MS: f64 = 5_000.0;

pub(crate) struct Daemon {
    pub(crate) name: String,
    pub(crate) generation: String,
    pub(crate) cfg: DaemonConfig,
    pub(crate) actor: TerminalActor,
    /// The substrate owner is the sole PTY master reader/writer/reaper.
    _owner: SessionOwner,
    pub(crate) session: pty_spawn::substrate::SessionClient,
    pub(crate) child_pid: i32,
    child_identity: Option<ProcessIdentity>,
    pub(crate) clients: BTreeMap<u64, Client>,
    pub(crate) attach_counter: u64,
    pub(crate) last_resize: Option<Instant>,
    pub(crate) settle: Duration,
    pub(crate) exited: bool,
    pub(crate) exit_code: i32,
    pub(crate) events: EventWriter,
    child_status: Option<ExitStatus>,
    pty_eof: bool,
    rx: Receiver<Msg>,
    /// Bridge output moved aside while waiting client commands are served.
    deferred_bridge: VecDeque<Msg>,
    external_kill: bool,
    shutdown_code: Option<i32>,
    exit_drain_deadline: Option<Instant>,
    exit_shutdown_at: Option<Instant>,
    exit_meta_retry: Option<(Instant, Instant)>,
    /// Unix milliseconds for the newest child output, in memory. `None`
    /// until the child prints something.
    last_output_at_ms: Option<i64>,
    /// When the pending activity write is due. A trailing-edge debounce: the
    /// first chunk after a quiet period schedules one write a second out, and
    /// every chunk inside that window folds into it, so a chatty session
    /// costs one sidecar write per second rather than one per chunk.
    activity_persist_at: Option<Instant>,
    /// The `clientGeneration` this daemon last bumped to (docs/decisions/0015).
    pub(crate) client_generation: u64,
    /// The client facts `client_generation` was bumped for.
    pub(crate) published_client_facts: ClientFacts,
    /// An ATTACH stamp whose write has not landed yet.
    pub(crate) pending_attach_at: Option<String>,
    /// `(next attempt, give up at)` for a `clientGeneration` write that met
    /// a held lock.
    pub(crate) client_meta_retry: Option<(Instant, Instant)>,
    listener_fd: i32,
    startup_lease: Option<ArmedStartupLease>,
    startup_lease_timer: Option<Instant>,
    startup_lease_disarmed: bool,
    startup_deadline_pending_shutdown: bool,
    startup_lease_terminal_value: Option<String>,
    /// The child's tree as the teardown will see it: frozen and observed at
    /// the startup deadline, before the terminal cause is chosen, or taken at
    /// `close` for any other external kill.
    teardown_tree: Option<TreeSnapshot>,
    /// The tree was stopped with SIGSTOP by the deadline freeze. A stopped
    /// process acts on nothing but SIGKILL, so the teardown skips the
    /// graceful signals.
    teardown_frozen: bool,
}

/// How long the activity write waits after the first chunk of a burst.
///
/// node: src/server.ts `scheduleActivityPersist` (1 s), the Node pty
/// repository's `docs/vrs` R14 (not this repository's `docs/vrs`).
const ACTIVITY_PERSIST_DEBOUNCE: Duration = Duration::from_secs(1);

/// How soon an activity write retries when the creation lock was held.
const ACTIVITY_PERSIST_BUSY_RETRY: Duration = Duration::from_millis(50);

/// 32 hex characters, Node's `randomBytes(16).toString("hex")`.
fn new_generation() -> String {
    registry::atomic::random_bytes(16)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `PTY_REDRAW_SETTLE_MS` overrides Node's 80 ms for tests that need a
/// wide synchronization window; the default is Node's.
fn settle_duration() -> Duration {
    std::env::var("PTY_REDRAW_SETTLE_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(REDRAW_SETTLE)
}

/// `PTY_SHUTDOWN_DEADLINE_MS`: finite and > 0, else 5000.
///
/// node: src/server.ts:1535-1538
fn shutdown_deadline() -> Duration {
    let ms = std::env::var("PTY_SHUTDOWN_DEADLINE_MS")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(SHUTDOWN_DEADLINE_DEFAULT_MS);
    Duration::from_millis(ms as u64)
}

/// `code = signal ? 128 + signal : exitCode`.
///
/// node: src/server.ts:571-578
pub fn decode_wait_status(status: Option<i32>) -> (i32, Option<i32>) {
    let Some(status) = status else {
        return (-1, None);
    };
    if libc::WIFSIGNALED(status) {
        let sig = libc::WTERMSIG(status);
        (128 + sig, Some(sig))
    } else if libc::WIFEXITED(status) {
        (libc::WEXITSTATUS(status), None)
    } else {
        (-1, None)
    }
}

/// Node's exit-time reap decision, re-reading the on-disk tags: refuse when
/// the on-disk generation is someone else's; never for a startup-lease
/// generation; never on an external kill unless ephemeral; else the
/// tag/ephemeral/config precedence.
///
/// A startup-lease generation's terminal lifecycle value and exit evidence
/// are the handoff its launcher reads after the child is gone, so they
/// outlive the daemon until the generation-fenced `evidence remove`. That
/// holds for `ephemeral` too: the lease is the stronger, later request.
///
/// node: src/server.ts:1481-1524; startup leases: PR #182
pub fn reap_at_exit(
    name: &str,
    generation: &str,
    external_kill: bool,
    ephemeral: bool,
    startup_lease: bool,
    config_tags: Option<&TagMap>,
) -> bool {
    let metadata = registry::read_metadata(name);
    if let Some(g) = metadata.as_ref().and_then(|m| m.generation.as_deref())
        && g != generation
    {
        return false;
    }
    if startup_lease {
        return false;
    }
    if external_kill && !ephemeral {
        return false;
    }
    let tags = metadata
        .as_ref()
        .and_then(|m| m.tags.as_ref())
        .or(config_tags);
    registry::should_reap_at_exit(tags, ephemeral, registry::reap_on_exit_default())
}

fn pid_alive(pid: i32) -> bool {
    registry::pid_alive(pid)
}

/// Debug builds honor `PTY_TEST_NO_STARTUP_TIMER`, so a test can reach an
/// expired startup deadline through a lifecycle CAS instead of racing the
/// deadline timer to it. Release builds never read it.
fn startup_timer_disabled_for_test() -> bool {
    cfg!(debug_assertions) && std::env::var_os("PTY_TEST_NO_STARTUP_TIMER").is_some()
}

/// The process table the teardown observes.
///
/// Debug builds honor `PTY_TEST_PROCESS_TABLE` so the integration tests can
/// drive the incomplete-observation path end to end: `unreadable` gives a
/// table that could not be read at all, `incomplete` the real rows without
/// the claim that they are every process. Release builds never read it.
fn teardown_process_table() -> ProcTable {
    #[cfg(debug_assertions)]
    match std::env::var("PTY_TEST_PROCESS_TABLE").as_deref() {
        Ok("unreadable") => return ProcTable::unreadable(),
        Ok("incomplete") => return ProcTable::read().marked_incomplete(),
        _ => {}
    }
    ProcTable::read()
}

/// Run the daemon for `cfg` to completion; the return value is the process
/// exit status (the child's code after a natural exit, 0 after a kill).
pub(crate) fn run(cfg: DaemonConfig, readiness: super::ReadyNotifier) -> Result<i32, String> {
    let name = cfg.name.clone();
    let generation = cfg
        .generation
        .clone()
        .filter(|g| !g.is_empty())
        .unwrap_or_else(new_generation);
    let startup_lease = cfg
        .startup_lease
        .as_ref()
        .map(|options| arm_startup_lease(options, &generation))
        .transpose()?;
    let mut published_tags = cfg.tags().cloned().unwrap_or_default();
    if let Some(lease) = &startup_lease {
        published_tags.insert(lease.lifecycle_tag.clone(), lease.starting_value.clone());
    }
    let events = EventWriter::new(&name);
    let (rows, cols, cwd) = (cfg.rows(), cfg.cols(), cfg.cwd());

    let child_env = build_child_env(&cfg, &generation)?;
    if let Some(reason) = describe_invalid_cwd(&cwd) {
        return Err(invalid_cwd_error(&reason, &name, &cfg.command));
    }

    // The PTY pair first: opening it fails before anything is published.
    let pair = pty_spawn::open(rows, cols)
        .map_err(|e| format!("Failed to open a PTY for session \"{name}\": {e}"))?;

    // Publication before the child spawns: dir → clear events → stale
    // socket → listen (umask 077, chmod 600) → pid → metadata →
    // session_start. An attached child's first action runs after this block,
    // so its session record — the `<name>.pid` owner sidecar, the metadata,
    // and the `session_start` line `pty run` waits for — is already on disk
    // when the child starts (compoundingtech/pty#180). Spawning first left
    // the child racing publication: its immediate `metadata patch` met the
    // creation lock its own `pty run` parent still held, and the sidecar
    // could be unpublished when it first ran.
    registry::ensure_session_dir().map_err(|e| e.to_string())?;
    pty_core::events::clear_events(&name)?;
    let socket_path = registry::socket_path(&name);
    let _ = std::fs::remove_file(&socket_path);
    // SAFETY: umask(2) has no preconditions.
    let prev_umask = unsafe { libc::umask(0o077) };
    let listener = UnixListener::bind(&socket_path);
    // SAFETY: restoring the mask we read above.
    unsafe {
        libc::umask(prev_umask);
    }
    let listener = listener.map_err(|e| format!("Socket server error: {e}"))?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600));
    }
    registry::write_pid(&name, std::process::id()).map_err(|e| e.to_string())?;
    let created_at = registry::now_iso8601();
    let metadata = SessionMetadata {
        generation: Some(generation.clone()),
        daemon_pid: Some(std::process::id() as i32),
        daemon_start_token: registry::read_process_start_token(std::process::id() as i32),
        recovery: None,
        command: cfg.command.clone(),
        args: cfg.args.clone(),
        display_command: cfg.display_command.clone(),
        cwd: cwd.clone(),
        rows: Some(rows),
        cols: Some(cols),
        ephemeral: Some(cfg.ephemeral),
        created_at,
        display_name: cfg.display_name().map(str::to_string),
        tags: (!published_tags.is_empty()).then(|| published_tags.clone()),
        isolate_env: cfg.isolate_env().then_some(true),
        extra_env: cfg.extra_env().cloned(),
        session_env: permanent_respawn_env(&cfg, &published_tags, &child_env),
        unset_env: (!cfg.unset_env().is_empty()).then(|| cfg.unset_env().to_vec()),
        env: cfg.env.clone(),
        ..Default::default()
    };
    registry::write_metadata_publication(&name, &metadata).map_err(|e| e.to_string())?;
    events.append(Event::session_start(
        &name,
        (!published_tags.is_empty()).then_some(&published_tags),
    ));
    if cfg.respawn {
        events.append(Event::session_respawn(&name));
    }
    events.flush();

    // The child: `/bin/sh -c 'exec "$@"' sh <command> <args...>`, so PATH
    // lookups, shebangs and symlinks behave like a shell's.
    let mut command = pty_spawn::shell_exec(&cfg.command, &cfg.args);
    command.cwd(&cwd);
    command.env_clear();
    for (k, v) in &child_env {
        command.env(k, v);
    }
    let child = match pair.slave.spawn_command(command) {
        Ok(child) => child,
        Err(error) => {
            if let Some(lease) = &startup_lease {
                let mut terminal_metadata = metadata.clone();
                terminal_metadata
                    .tags
                    .get_or_insert_with(TagMap::new)
                    .insert(
                        lease.lifecycle_tag.clone(),
                        terminal_startup_lease_value(&generation, StartupLeaseTerminalCause::Exit),
                    );
                let _ = registry::write_metadata_publication(&name, &terminal_metadata);
            }
            let _ = std::fs::remove_file(&socket_path);
            let _ = std::fs::remove_file(registry::pid_path(&name));
            return Err(format!(
                "Failed to spawn PTY shell \"/bin/sh\" for command \"{}\" in cwd \"{cwd}\": {error}",
                cfg.command
            ));
        }
    };
    let child_pid = child.process_id().map(|p| p as i32).unwrap_or(0);
    let child_identity = pty_core::proctable::process(child_pid)
        .known()
        .and_then(|row| row.identity)
        .map(|identity| ProcessIdentity {
            pid: child_pid,
            identity,
            depth: 0,
        });
    let session = SessionRef::new(registry::session_dir(), name.clone(), generation.clone());
    let (owner, session_client, session_stream) =
        pty_spawn::external_owned_pair_attached(session, pair, child).map_err(|e| {
            format!("Failed to hand PTY to the session substrate for \"{name}\": {e}")
        })?;
    let (tx, rx) = mpsc::channel::<Msg>();
    spawn_session_bridge(session_stream, tx.clone());

    let listener_fd = listener.as_raw_fd();
    spawn_acceptor(listener, tx.clone());
    spawn_signal_listener(tx.clone());
    install_spawner_watchdog(tx);

    let daemon = Daemon {
        name,
        generation,
        cfg,
        actor: terminal_actor(rows, cols),
        _owner: owner,
        session: session_client,
        child_pid,
        child_identity,
        clients: BTreeMap::new(),
        attach_counter: 0,
        last_resize: None,
        settle: settle_duration(),
        exited: false,
        exit_code: 0,
        events,
        child_status: None,
        pty_eof: false,
        rx,
        deferred_bridge: VecDeque::new(),
        external_kill: false,
        shutdown_code: None,
        exit_drain_deadline: None,
        exit_shutdown_at: None,
        exit_meta_retry: None,
        last_output_at_ms: None,
        activity_persist_at: None,
        client_generation: 0,
        published_client_facts: ClientFacts::unattached(rows, cols),
        pending_attach_at: None,
        client_meta_retry: None,
        listener_fd,
        startup_lease_timer: startup_lease
            .as_ref()
            .and_then(|lease| lease.deadline_monotonic_ns)
            .filter(|_| !startup_timer_disabled_for_test())
            .map(|deadline| Instant::now() + remaining_lease_delay(deadline)),
        startup_lease,
        startup_lease_disarmed: false,
        startup_deadline_pending_shutdown: false,
        startup_lease_terminal_value: None,
        teardown_tree: None,
        teardown_frozen: false,
    };

    readiness.notify();
    Ok(daemon.serve())
}

/// The session's terminal, with kitty graphics on.
///
/// A session is the durable owner of the child's screen, and since libghostty
/// keeps image state per screen, that includes the child's images: without it
/// the `SCREEN` a late client replays would carry placeholder cells naming
/// images nobody has (docs/decisions/0012-kitty-graphics-replay.md). The
/// storage limit is a cap, not an allocation — a session whose child never
/// transmits an image holds nothing and serializes exactly as before.
fn terminal_actor(rows: u16, cols: u16) -> TerminalActor {
    let mut actor = TerminalActor::new(rows, cols, pty_terminal::actor::DEFAULT_SCROLLBACK);
    if !actor.enable_graphics(pty_terminal::GraphicsOptions::DEFAULT) {
        // The session still runs: text is unaffected and the actor rolled the
        // storage limit back, so the only loss is images. Say so rather than
        // leaving a client to wonder why its replay carries placeholder cells
        // and no pictures.
        daemon_warn!("pty: kitty graphics unavailable for this session");
    }
    actor
}

fn spawn_session_bridge(stream: pty_spawn::substrate::AttachStream, tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let mut exited = false;
        let mut output_closed = false;
        loop {
            match stream.recv() {
                Ok(SessionEvent::Data(bytes)) => {
                    if tx.send(Msg::PtyData(bytes)).is_err() {
                        return;
                    }
                }
                Ok(SessionEvent::OutputClosed) => {
                    output_closed = true;
                    if tx.send(Msg::PtyEof).is_err() || exited {
                        return;
                    }
                }
                Ok(SessionEvent::Lifecycle(Lifecycle::Running)) => {}
                Ok(SessionEvent::Lifecycle(Lifecycle::Exited(status))) => {
                    exited = true;
                    if tx.send(Msg::ChildExited(status)).is_err() || output_closed {
                        return;
                    }
                }
                Ok(SessionEvent::Lifecycle(Lifecycle::OwnerLost)) | Err(_) => {
                    if !output_closed {
                        let _ = tx.send(Msg::PtyEof);
                    }
                    return;
                }
                Ok(SessionEvent::Geometry(_)) => {}
            }
        }
    });
}

/// Will this `accept` failure pass on its own?
///
/// A signal, a peer that hung up before we reached it, or a machine with no
/// descriptors to spare: all of these end. Anything else says the listener
/// itself is broken and waiting will not mend it.
pub(crate) fn accept_failure_passes(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(
        e.kind(),
        Interrupted | ConnectionAborted | WouldBlock | TimedOut
    ) || e.raw_os_error() == Some(libc::EMFILE)
        || e.raw_os_error() == Some(libc::ENFILE)
}

/// Accept forever, and do not go deaf on a passing failure.
///
/// Leaving this loop is the worst thing it can do. The daemon keeps running,
/// the child keeps running, and the registry keeps saying the session is
/// running, but nothing can ever attach, peek or ask for stats again. A
/// descriptor shortage would do it, and a descriptor shortage ends. So a
/// failure that can pass is reported and retried, and only a listener that
/// cannot work at all shuts the session down, where somebody can see it.
fn spawn_acceptor(listener: UnixListener, tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let ids = Arc::new(AtomicU64::new(1));
        let mut consecutive = 0u32;
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(stream) => {
                    consecutive = 0;
                    stream
                }
                Err(e) => {
                    if !accept_failure_passes(&e) {
                        daemon_warn!("pty daemon: the listener failed: {e}");
                        let _ = tx.send(Msg::ExternalKill);
                        break;
                    }
                    consecutive += 1;
                    if consecutive == 1 || consecutive % 100 == 0 {
                        daemon_warn!("pty daemon: accept failed ({consecutive}): {e}");
                    }
                    // Long enough that a descriptor shortage is not made
                    // worse by spinning on it.
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
            };
            let id = ids.fetch_add(1, Ordering::Relaxed);
            spawn_client(id, stream, tx.clone());
        }
    });
}

/// One writer thread (packets → socket) and one reader thread (socket →
/// [`Msg`]) per connection.
fn spawn_client(id: u64, stream: UnixStream, tx: Sender<Msg>) {
    let (out_tx, out_rx) = mpsc::channel::<Out>();
    let write_lock = Arc::new(Mutex::new(()));
    let writer_lock = Arc::clone(&write_lock);
    let Ok(mut wstream) = stream.try_clone() else {
        return;
    };
    let Ok(disconnect) = stream.try_clone() else {
        return;
    };
    let writer = std::thread::Builder::new().spawn(move || {
        while let Ok(out) = out_rx.recv() {
            match out {
                Out::Bytes(packet) => {
                    let _guard = writer_lock.lock().unwrap_or_else(|error| error.into_inner());
                    if wstream.write_all(&packet.bytes).is_err() {
                        break;
                    }
                }
                Out::End => {
                    let _ = wstream.shutdown(std::net::Shutdown::Write);
                }
                Out::Destroy => break,
            }
        }
        let _ = wstream.shutdown(std::net::Shutdown::Both);
    });
    if let Err(e) = writer {
        daemon_warn!("pty daemon: cannot start client writer thread: {e}");
        return;
    }
    let _ = tx.send(Msg::Connect {
        id,
        tx: out_tx,
        disconnect,
        write_lock,
    });
    let reader_tx = tx.clone();
    let reader = std::thread::Builder::new().spawn(move || {
        let mut stream = stream;
        let mut parser = PacketReader::new();
        let mut buf = [0u8; 16384];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => match parser.feed(&buf[..n]) {
                    Ok(packets) => {
                        for packet in packets {
                            if reader_tx.send(Msg::Packet { id, packet }).is_err() {
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        // A peer declaring an oversize frame is dropped
                        // rather than buffered without bound.
                        crate::daemon::daemon_warn!("Rejected client packet: {e}");
                        let _ = stream.shutdown(std::net::Shutdown::Both);
                        break;
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let _ = reader_tx.send(Msg::Closed { id });
    });
    if let Err(e) = reader {
        daemon_warn!("pty daemon: cannot start client reader thread: {e}");
        let _ = tx.send(Msg::Closed { id });
    }
}

/// SIGTERM and SIGINT are external kills.
///
/// node: src/server.ts:1598-1603
fn spawn_signal_listener(tx: Sender<Msg>) {
    let Ok(mut signals) = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
    ]) else {
        return;
    };
    std::thread::spawn(move || {
        for _ in signals.forever() {
            if tx.send(Msg::ExternalKill).is_err() {
                return;
            }
        }
    });
}

/// `PTY_SPAWNER_PID`: an integer > 1; dead at boot → shut down now; else
/// poll every 5 s.
///
/// node: src/server.ts:1439-1456
fn install_spawner_watchdog(tx: Sender<Msg>) {
    let Some(raw) = std::env::var("PTY_SPAWNER_PID")
        .ok()
        .filter(|r| !r.is_empty())
    else {
        return;
    };
    let Some(pid) = raw
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|p| p.fract() == 0.0 && *p > 1.0 && *p <= i32::MAX as f64)
        .map(|p| p as i32)
    else {
        return;
    };
    if !pid_alive(pid) {
        let _ = tx.send(Msg::ExternalKill);
        return;
    }
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(SPAWNER_POLL);
            if !pid_alive(pid) {
                let _ = tx.send(Msg::ExternalKill);
                return;
            }
        }
    });
}

fn clipboard_client_available(clients: &BTreeMap<u64, Client>) -> bool {
    clients.values().any(|client| {
        client.role == Role::Writable && client.attached.is_some() && !client.is_settling()
    })
}

impl Daemon {
    fn owner(&self) -> SessionGenerationOwner {
        SessionGenerationOwner {
            generation: self.generation.clone(),
            pid: std::process::id() as i32,
        }
    }

    pub(crate) fn write_pty(&mut self, bytes: &[u8]) {
        if bytes.is_empty() || self.child_status.is_some() {
            return;
        }
        let _ = self.session.input(bytes.to_vec());
    }

    /// The serving loop, then the shutdown. Returns the process exit status.
    fn serve(mut self) -> i32 {
        loop {
            let now = Instant::now();
            let deadline = [
                self.next_cut_deadline(),
                self.exit_drain_deadline,
                self.exit_shutdown_at,
                self.exit_meta_retry.map(|(next, _)| next),
                self.activity_persist_at,
                self.client_meta_retry.map(|(next, _)| next),
                self.startup_lease_timer,
            ]
            .into_iter()
            .flatten()
            .min();
            let msg = match deadline {
                Some(d) => match next_queued_message(
                    &self.rx,
                    &mut self.deferred_bridge,
                    Some(d.saturating_duration_since(now)),
                ) {
                    Ok(m) => Some(m),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                },
                None => match next_queued_message(&self.rx, &mut self.deferred_bridge, None) {
                    Ok(m) => Some(m),
                    Err(_) => break,
                },
            };
            if let Some(m) = msg {
                self.handle(m);
            }
            self.service_timers(Instant::now());
            if let Some(code) = self.shutdown_code {
                return self.close(code);
            }
        }
        let code = if self.exited { self.exit_code } else { 0 };
        self.close(code)
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::PtyData(bytes) => self.on_pty_data(&bytes),
            Msg::PtyEof => {
                self.pty_eof = true;
                if self.child_status.is_some() && !self.exited {
                    self.finalize_exit();
                }
            }
            Msg::ChildExited(status) => {
                if self.child_status.is_some() {
                    return;
                }
                self.child_status = Some(status);
                if self.pty_eof {
                    if !self.exited {
                        self.finalize_exit();
                    }
                } else {
                    self.exit_drain_deadline = Some(Instant::now() + EXIT_DRAIN);
                }
            }
            Msg::Connect { id, tx, disconnect, write_lock } => {
                self.clients.insert(
                    id,
                    Client::new(tx, disconnect, self.actor.rows(), self.actor.cols(), write_lock),
                );
            }
            Msg::Packet { id, packet } => self.on_packet(id, packet),
            Msg::Closed { id } => self.on_closed(id),
            Msg::ExternalKill => {
                if self.shutdown_code.is_none() {
                    self.external_kill = true;
                    self.shutdown_code = Some(0);
                }
            }
        }
    }

    /// Child output: into the terminal (queries answered back to the
    /// child), events to the log, the remainder to live clients.
    ///
    /// node: src/server.ts:559-569
    fn on_pty_data(&mut self, bytes: &[u8]) {
        self.stamp_output_activity();
        self.actor.set_clipboard_client_available(clipboard_client_available(&self.clients));
        let cleaned = self.actor.write(bytes);
        let replies = self.actor.take_pty_replies();
        self.write_pty(&replies);
        self.forward_terminal_events();
        if !cleaned.is_empty() {
            self.broadcast(&encode_data(&cleaned));
        }
    }

    /// Record that the child has just printed, and make sure a write is
    /// pending. Both halves are O(1); the write itself happens later.
    fn stamp_output_activity(&mut self) {
        self.last_output_at_ms = Some(registry::now_epoch_ms());
        if self.activity_persist_at.is_none() {
            self.activity_persist_at = Some(Instant::now() + ACTIVITY_PERSIST_DEBOUNCE);
        }
    }

    /// Publish the newest output stamp to the `.activity/<name>.json`
    /// sidecar. The record itself is left alone: output is not a fact its
    /// observers wait on, and rewriting it once a second made every busy
    /// session look changed (docs/decisions/0015).
    ///
    /// Best effort on purpose: a lost stamp reads as slightly older activity,
    /// and it must never take the daemon down or block the output path.
    fn persist_output_activity(&mut self) {
        self.activity_persist_at = None;
        if self.exited {
            return;
        }
        let Some(stamped) = self.last_output_at_ms else {
            return;
        };
        let status = registry::publish_output_activity(
            &self.name,
            &registry::OutputActivity {
                generation: self.generation.clone(),
                last_output_at_ms: stamped,
            },
        );
        // A held creation lock is transient (an attach or a patch): try
        // again shortly rather than drop the stamp. Any other refusal means
        // this generation no longer owns the name, and its sidecar is not
        // this daemon's to write.
        if status == MutateStatus::Busy {
            self.activity_persist_at = Some(Instant::now() + ACTIVITY_PERSIST_BUSY_RETRY);
        }
    }

    pub(crate) fn on_accepted_socket_ownership(&mut self, id: u64, payload: &[u8]) {
        let unavailable = |reason: &str| AcceptedSocketOwnershipResult::Unavailable {
            reason: reason.to_string(),
        };
        let result = match decode_accepted_socket_ownership_request(payload) {
            None => unavailable("invalid-request"),
            Some(request) if request.expected_generation != self.generation => {
                unavailable("generation-mismatch")
            }
            Some(_) if self.exited => unavailable("child-exited"),
            Some(request) => match registry::read_metadata(&self.name) {
                None => unavailable("metadata-unavailable"),
                Some(metadata)
                    if metadata.generation.as_deref() != Some(self.generation.as_str()) =>
                {
                    unavailable("generation-mismatch")
                }
                Some(_) => match &self.child_identity {
                    None => unavailable("process-identity-unavailable"),
                    Some(child) => super::ownership::inspect_accepted_socket_ownership(
                        child.pid,
                        &child.identity,
                        &request.connection,
                    ),
                },
            },
        };
        if let Some(client) = self.clients.get(&id) {
            client.send(encode_accepted_socket_ownership_response(&result));
        }
    }

    pub(crate) fn on_lifecycle_cas(&mut self, id: u64, payload: &[u8]) {
        let result = match decode_lifecycle_compare_and_set_request(payload) {
            Some(request) => self.compare_and_set_lifecycle(&request),
            None => LifecycleCompareAndSetResult::InvalidRequest {
                reason: "invalid request payload".to_string(),
            },
        };
        if let Some(client) = self.clients.get(&id) {
            client.send(encode_lifecycle_compare_and_set_response(&result));
        }
    }

    fn compare_and_set_lifecycle(
        &mut self,
        request: &LifecycleCompareAndSetRequest,
    ) -> LifecycleCompareAndSetResult {
        if request.expected_generation != self.generation {
            return LifecycleCompareAndSetResult::GenerationMismatch;
        }
        let Some(current) = registry::read_metadata(&self.name) else {
            return LifecycleCompareAndSetResult::Missing;
        };
        if current.generation.as_deref() != Some(self.generation.as_str()) {
            return LifecycleCompareAndSetResult::GenerationMismatch;
        }
        if self.exited {
            let value = self
                .settle_startup_lifecycle(StartupLeaseTerminalCause::Exit, true)
                .unwrap_or_else(|| {
                    terminal_startup_lease_value(&self.generation, StartupLeaseTerminalCause::Exit)
                });
            return LifecycleCompareAndSetResult::Terminal { value };
        }
        let expired = self.startup_lease.as_ref().is_some_and(|lease| {
            !self.startup_lease_disarmed
                && lease
                    .deadline_monotonic_ns
                    .is_some_and(|deadline| monotonic_now_ns().is_none_or(|now| now >= deadline))
        });
        if expired {
            if self.settle_startup_deadline().is_none() {
                return LifecycleCompareAndSetResult::Busy;
            }
            self.startup_deadline_pending_shutdown = true;
            self.startup_lease_timer = Some(Instant::now() + Duration::from_millis(25));
            let value = self
                .startup_lease_terminal_value
                .clone()
                .expect("settled startup deadline has a terminal value");
            return LifecycleCompareAndSetResult::DeadlineExpired { value };
        }

        let result = registry::compare_and_set_tag_value(
            &self.name,
            &request.expected_generation,
            &request.tag,
            &request.expected_value,
            &request.value,
        );
        let changed = matches!(&result, TagCompareAndSetResult::Changed { .. });
        match result {
            TagCompareAndSetResult::Changed { value }
            | TagCompareAndSetResult::Unchanged { value } => {
                if let Some(lease) = &self.startup_lease
                    && request.tag == lease.lifecycle_tag
                    && request.expected_value == lease.starting_value
                    && request.value != lease.starting_value
                {
                    self.startup_lease_disarmed = true;
                    self.startup_lease_timer = None;
                }
                if request.tag
                    == self
                        .startup_lease
                        .as_ref()
                        .map(|lease| lease.lifecycle_tag.as_str())
                        .unwrap_or("")
                    && serde_json::from_str::<serde_json::Value>(&request.value)
                        .ok()
                        .is_some_and(|lifecycle| {
                            lifecycle.get("_tag").and_then(|tag| tag.as_str()) == Some("terminal")
                                && lifecycle
                                    .get("generation")
                                    .and_then(|generation| generation.as_str())
                                    == Some(self.generation.as_str())
                        })
                {
                    self.startup_lease_terminal_value = Some(request.value.clone());
                }
                if changed {
                    LifecycleCompareAndSetResult::Changed { value }
                } else {
                    LifecycleCompareAndSetResult::Unchanged { value }
                }
            }
            TagCompareAndSetResult::ValueMismatch { value } => {
                LifecycleCompareAndSetResult::ValueMismatch { value }
            }
            TagCompareAndSetResult::Missing => LifecycleCompareAndSetResult::Missing,
            TagCompareAndSetResult::GenerationMismatch => {
                LifecycleCompareAndSetResult::GenerationMismatch
            }
            TagCompareAndSetResult::Busy | TagCompareAndSetResult::Stale => {
                LifecycleCompareAndSetResult::Busy
            }
        }
    }

    fn settle_startup_lifecycle(
        &mut self,
        cause: StartupLeaseTerminalCause,
        force: bool,
    ) -> Option<String> {
        let lease = self.startup_lease.as_ref()?;
        if let Some(value) = &self.startup_lease_terminal_value {
            return Some(value.clone());
        }
        if self.startup_lease_disarmed && !force {
            return None;
        }
        let terminal = terminal_startup_lease_value(&self.generation, cause);
        let tag = lease.lifecycle_tag.clone();
        let terminal_for_write = terminal.clone();
        let result = registry::mutate_metadata_under_lock_with_wait(
            &self.name,
            move |metadata| {
                if metadata.tags.as_ref().and_then(|tags| tags.get(&tag))
                    == Some(&terminal_for_write)
                {
                    return false;
                }
                metadata
                    .tags
                    .get_or_insert_with(TagMap::new)
                    .insert(tag, terminal_for_write);
                true
            },
            &MutateOptions {
                expected_generation: Some(self.generation.clone()),
                expected_metadata: None,
            },
            |_| {},
            Duration::from_millis(100),
        );
        match result {
            MutateStatus::Changed(_) | MutateStatus::Unchanged(_) => {
                self.startup_lease_disarmed = true;
                self.startup_lease_timer = None;
                self.startup_lease_terminal_value = Some(terminal.clone());
                Some(terminal)
            }
            MutateStatus::GenerationMismatch => {
                self.startup_lease_disarmed = true;
                self.startup_lease_timer = None;
                None
            }
            MutateStatus::Busy | MutateStatus::Stale | MutateStatus::Missing => None,
        }
    }

    /// The startup deadline has passed: freeze and observe the child's tree
    /// first, then record `deadline` only if that observation was complete.
    /// An unreadable or partial one records `teardown-unavailable`, and the
    /// teardown adds the process-group fallback to the exact identities it
    /// did observe.
    ///
    /// The tree is frozen once. If the terminal value is then refused for
    /// good (a newer generation owns the name), the tree is resumed: the
    /// deadline no longer belongs to this daemon.
    ///
    /// node: src/server.ts `settleStartupLeaseDeadline` (PR #182)
    fn settle_startup_deadline(&mut self) -> Option<String> {
        if self.teardown_tree.is_none() {
            let tree = self.freeze_child_tree();
            if let TreeSnapshot::Unavailable { reason, identities } = &tree {
                daemon_warn!(
                    "pty daemon \"{}\": complete child containment unavailable: {reason}; publishing teardown-unavailable and exact-signalling {} observed descendant(s) plus process-group fallback",
                    self.name,
                    identities.len()
                );
            }
            self.teardown_tree = Some(tree);
            self.teardown_frozen = true;
        }
        let complete = self
            .teardown_tree
            .as_ref()
            .is_some_and(TreeSnapshot::is_complete);
        let settled = self.settle_startup_lifecycle(startup_lease_deadline_cause(complete), false);
        if settled.is_none() && self.startup_lease_disarmed {
            self.resume_frozen_tree();
        }
        settled
    }

    /// Stop the child, its process group and every descendant, and observe
    /// the tree. Every signal to the child goes through the substrate, which
    /// refuses once the child is reaped; a child that cannot be stopped makes
    /// the observation incomplete.
    fn freeze_child_tree(&self) -> TreeSnapshot {
        let session = self.session.clone();
        freeze_descendants(
            self.child_pid,
            move || {
                let _ = session.signal_process_group(libc::SIGSTOP);
                session.signal(libc::SIGSTOP).is_ok()
            },
            teardown_process_table,
            |identities| signal_process_identities(identities, libc::SIGSTOP),
        )
    }

    fn resume_frozen_tree(&mut self) {
        if !std::mem::take(&mut self.teardown_frozen) {
            return;
        }
        let _ = self.session.signal_process_group(libc::SIGCONT);
        let _ = self.session.signal(libc::SIGCONT);
        if let Some(tree) = self.teardown_tree.take() {
            signal_process_identities(tree.identities(), libc::SIGCONT);
        }
    }

    fn service_timers(&mut self, now: Instant) {
        self.service_cuts(now);
        if let Some(at) = self.activity_persist_at
            && at <= now
        {
            self.persist_output_activity();
        }
        if let Some((next, _)) = self.client_meta_retry
            && next <= now
        {
            self.write_client_generation();
        }
        if let Some(d) = self.exit_drain_deadline
            && d <= now
        {
            self.exit_drain_deadline = None;
            if !self.exited && self.child_status.is_some() {
                self.finalize_exit();
            }
        }
        if let Some((next, deadline)) = self.exit_meta_retry
            && next <= now
        {
            match self.save_exit_metadata() {
                MutateStatus::Busy | MutateStatus::Stale if now < deadline => {
                    self.exit_meta_retry = Some((now + Duration::from_millis(10), deadline));
                }
                _ => self.exit_meta_retry = None,
            }
        }
        if let Some(at) = self.startup_lease_timer
            && at <= now
        {
            if self.startup_deadline_pending_shutdown {
                self.startup_deadline_pending_shutdown = false;
                self.startup_lease_timer = None;
                self.external_kill = true;
                if self.shutdown_code.is_none() {
                    self.shutdown_code = Some(124);
                }
            } else if self.startup_lease_disarmed {
                self.startup_lease_timer = None;
            } else if let Some(deadline) = self
                .startup_lease
                .as_ref()
                .and_then(|lease| lease.deadline_monotonic_ns)
            {
                let remaining = remaining_lease_delay(deadline);
                if remaining.is_zero() {
                    if self.settle_startup_deadline().is_some() {
                        self.external_kill = true;
                        self.shutdown_code = Some(124);
                    } else {
                        self.startup_lease_timer = Some(now + Duration::from_millis(10));
                    }
                } else {
                    self.startup_lease_timer = Some(now + remaining);
                }
            } else {
                self.startup_lease_timer = None;
            }
        }
        if let Some(at) = self.exit_shutdown_at
            && at <= now
        {
            self.exit_shutdown_at = None;
            if self.shutdown_code.is_none() {
                self.shutdown_code = Some(self.exit_code);
            }
        }
    }

    /// The child is gone: EXIT to live clients (settling ones get it after
    /// their SCREEN), `session_exit`, exit metadata, shutdown in 500 ms.
    ///
    /// node: src/server.ts:571-598
    fn finalize_exit(&mut self) {
        let status = self.child_status.unwrap_or(ExitStatus {
            code: None,
            signal: None,
        });
        let signal = status.signal;
        let code = signal.map_or(status.code.unwrap_or(-1), |signal| 128 + signal);
        self.settle_startup_lifecycle(StartupLeaseTerminalCause::Exit, true);
        self.exited = true;
        self.exit_code = code;
        self.exit_drain_deadline = None;
        self.broadcast(&encode_exit(code));
        self.events
            .append(Event::session_exit(&self.name, code, signal));
        if matches!(
            self.save_exit_metadata(),
            MutateStatus::Busy | MutateStatus::Stale
        ) {
            let now = Instant::now();
            self.exit_meta_retry =
                Some((now + Duration::from_millis(10), now + EXIT_METADATA_RETRY));
        }
        if self.shutdown_code.is_none() {
            self.exit_shutdown_at = Some(Instant::now() + EXIT_GRACE);
        }
    }

    /// All rows, trailing empties trimmed, the last 200.
    ///
    /// node: src/server.ts:1295-1309
    fn last_lines(&self) -> Vec<String> {
        let lines = serialize::plain_lines_full(self.actor.terminal());
        let start = lines.len().saturating_sub(SESSION_EXIT_LAST_LINES_LIMIT);
        lines[start..].to_vec()
    }

    /// node: src/server.ts:1311-1319
    fn save_exit_metadata(&self) -> MutateStatus {
        let code = self.exit_code;
        let last_lines = self.last_lines();
        // Carry the newest output stamp even when its own write was still
        // waiting out the debounce, so the last thing the child printed is
        // never lost to the exit.
        let last_output = self.last_output_at_ms;
        // The exit record now carries the final stamp, so the sidecar is
        // retired in the same critical section, while this generation still
        // owns the name: a replacement's sidecar is never this daemon's to
        // remove.
        registry::record_exit_retiring_output_activity(&self.name, &self.generation, move |m| {
            let mut changed = false;
            if m.exit_code != Some(code) {
                m.exit_code = Some(code);
                changed = true;
            }
            if m.exited_at.is_none() {
                m.exited_at = Some(registry::now_iso8601());
                changed = true;
            }
            if m.last_lines.as_ref() != Some(&last_lines) {
                m.last_lines = Some(last_lines);
                changed = true;
            }
            if let Some(stamp) = last_output
                && m.last_output_at_ms != Some(stamp)
            {
                m.last_output_at_ms = Some(stamp);
                changed = true;
            }
            changed
        })
    }

    /// node: src/server.ts:1321-1337
    fn save_exit_metadata_until_settled(&self, budget: Duration) {
        let deadline = Instant::now() + budget;
        loop {
            match self.save_exit_metadata() {
                MutateStatus::Changed(_)
                | MutateStatus::Unchanged(_)
                | MutateStatus::Missing
                | MutateStatus::GenerationMismatch => return,
                MutateStatus::Busy | MutateStatus::Stale => {}
            }
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn settle_startup_lifecycle_before_close(&mut self) {
        if self.startup_lease.is_none() || self.startup_lease_terminal_value.is_some() {
            return;
        }
        let deadline = Instant::now() + EXIT_METADATA_SETTLE;
        loop {
            if self
                .settle_startup_lifecycle(StartupLeaseTerminalCause::Exit, true)
                .is_some()
            {
                return;
            }
            let Some(metadata) = registry::read_metadata(&self.name) else {
                return;
            };
            if metadata.generation.as_deref() != Some(self.generation.as_str())
                || Instant::now() >= deadline
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn reap_at_exit(&self) -> bool {
        reap_at_exit(
            &self.name,
            &self.generation,
            self.external_kill,
            self.cfg.ephemeral,
            self.startup_lease.is_some(),
            self.cfg.tags(),
        )
    }

    /// The hard deadline behind a graceful shutdown.
    ///
    /// node: src/server.ts:1545-1558
    fn start_backstop(
        &self,
        code: i32,
        descendants: Arc<Mutex<Vec<ProcessIdentity>>>,
        group_fallback: bool,
    ) {
        let deadline = shutdown_deadline();
        let name = self.name.clone();
        let generation = self.generation.clone();
        let (external, ephemeral) = (self.external_kill, self.cfg.ephemeral);
        let startup_lease = self.startup_lease.is_some();
        let tags = self.cfg.tags().cloned();
        let session = self.session.clone();
        let owner = self.owner();
        std::thread::spawn(move || {
            std::thread::sleep(deadline);
            crate::daemon::daemon_warn!(
                "pty daemon \"{name}\": graceful shutdown exceeded {}ms — forcing exit (child reaped)",
                deadline.as_millis()
            );
            // Through the substrate, which refuses once the child is reaped:
            // its pid, and the group id equal to it, may belong to someone
            // else by then.
            if group_fallback {
                let _ = session.signal_process_group(libc::SIGKILL);
            }
            let _ = session.kill();
            let descendants = descendants.lock().map(|d| d.clone()).unwrap_or_default();
            signal_process_identities(&descendants, libc::SIGKILL);
            if reap_at_exit(
                &name,
                &generation,
                external,
                ephemeral,
                startup_lease,
                tags.as_ref(),
            ) {
                registry::cleanup_owned_all(&name, &owner);
            } else {
                registry::cleanup_owned_socket(&name, &owner);
            }
            std::process::exit(code);
        });
    }

    /// Wait until the child's exit has been recorded, still feeding its
    /// last output into the terminal. `true` when it exited in time.
    fn wait_child_exit(&mut self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        while !self.exited {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let wake = self
                .exit_drain_deadline
                .map_or(deadline, |d| d.min(deadline));
            match next_queued_message(
                &self.rx,
                &mut self.deferred_bridge,
                Some(wake.saturating_duration_since(now)),
            ) {
                Ok(Msg::PtyData(bytes)) => self.on_pty_data(&bytes),
                Ok(msg @ (Msg::PtyEof | Msg::ChildExited(_))) => self.handle(msg),
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return self.exited,
            }
            let now = Instant::now();
            if let Some(d) = self.exit_drain_deadline
                && d <= now
            {
                self.exit_drain_deadline = None;
                if !self.exited && self.child_status.is_some() {
                    self.finalize_exit();
                }
            }
        }
        true
    }

    /// Node's `close()` followed by the reap decision.
    ///
    /// node: src/server.ts:1340-1408, 1559-1568
    fn close(mut self, code: i32) -> i32 {
        self.settle_startup_lifecycle_before_close();
        let tree = self.external_kill.then(|| {
            self.teardown_tree.take().unwrap_or_else(|| {
                let tree = complete_snapshot_from_table(self.child_pid, &teardown_process_table());
                if let TreeSnapshot::Unavailable { reason, identities } = &tree {
                    daemon_warn!(
                        "pty daemon \"{}\": complete child snapshot unavailable: {reason}; exact-signalling {} observed descendant(s) plus process-group fallback",
                        self.name,
                        identities.len()
                    );
                }
                tree
            })
        });
        // An incomplete snapshot may have missed members of the child's
        // group; the group itself still reaches them.
        let group_fallback = tree.as_ref().is_some_and(|tree| !tree.is_complete());
        let frozen = self.teardown_frozen;
        let descendants = Arc::new(Mutex::new(
            tree.as_ref()
                .map(|tree| tree.identities().to_vec())
                .unwrap_or_default(),
        ));
        self.start_backstop(code, descendants.clone(), group_fallback);
        if self.exited {
            self.save_exit_metadata();
        }
        for (_, c) in std::mem::take(&mut self.clients) {
            c.send_out(Out::Destroy);
        }
        // SAFETY: shutdown on the listening socket unblocks accept(2).
        unsafe {
            libc::shutdown(self.listener_fd, libc::SHUT_RDWR);
        }
        registry::cleanup_owned_socket(&self.name, &self.owner());
        if self.child_status.is_none() {
            if frozen {
                // Stopped by the deadline freeze: only SIGKILL acts on it.
                // Sent through the substrate while the child is unreaped, the
                // group id cannot name anyone else.
                if group_fallback {
                    let _ = self.session.signal_process_group(libc::SIGKILL);
                }
                let _ = self.session.kill();
            } else {
                let _ = self.session.hangup();
            }
        }
        let descendant_wait = self.external_kill.then(|| {
            let ids = descendants.lock().map(|d| d.clone()).unwrap_or_default();
            let term_wait = if frozen { Duration::ZERO } else { TERM_WAIT };
            let group = group_fallback.then_some(self.child_pid);
            std::thread::spawn(move || {
                let survivors = if frozen {
                    terminate_process_identities(&ids, term_wait, KILL_WAIT)
                } else {
                    terminate_process_identities_after_hangup(
                        &ids,
                        DESCENDANT_HUP_WAIT,
                        term_wait,
                        KILL_WAIT,
                    )
                };
                let group_gone =
                    group.is_none_or(|pgid| terminate_process_group(pgid, term_wait, KILL_WAIT));
                (survivors, group_gone)
            })
        });
        if !self.wait_child_exit(CHILD_HUP_WAIT) {
            let _ = self.session.kill();
            self.wait_child_exit(CHILD_KILL_WAIT);
        }
        let (survivors, group_gone) = descendant_wait
            .and_then(|t| t.join().ok())
            .unwrap_or((Vec::new(), true));
        if !group_gone {
            crate::daemon::daemon_warn!(
                "pty daemon \"{}\": process-group fallback could not verify teardown after an incomplete snapshot",
                self.name
            );
        }
        if !survivors.is_empty() {
            let pids: Vec<i32> = survivors.iter().map(|s| s.pid).collect();
            crate::daemon::daemon_warn!(
                "pty daemon \"{}\": {} child process(es) did not exit after exact TERM and KILL signals: {pids:?}",
                self.name,
                survivors.len()
            );
            // And somewhere a person can find it. The warning above goes to
            // this daemon's standard error, which has had no reader since
            // the command that launched it stopped listening — so the one
            // moment it has something worth saying is the one moment nobody
            // is there. `pty kill` reports on the daemon and cannot see a
            // surviving child, so without this line the fact reaches no one.
            self.events
                .append(Event::session_descendants_survived(&self.name, &pids));
        }
        if self.exited {
            self.save_exit_metadata_until_settled(EXIT_METADATA_SETTLE);
        }
        self.events.flush();
        if self.reap_at_exit() {
            registry::cleanup_owned_all(&self.name, &self.owner());
        }
        code
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use pty_core::protocol::AttachedClient;
    use pty_spawn::{external_owned_pair, open, shell_exec};

    #[test]
    fn command_socket_does_not_suppress_clipboard_fallback() {
        let (tx, _rx) = mpsc::channel();
        let (disconnect, _peer) = UnixStream::pair().unwrap();
        let mut clients = BTreeMap::new();
        clients.insert(1, Client::new(tx, disconnect, 24, 80, Arc::new(Mutex::new(()))));
        let mut actor = TerminalActor::new(24, 80, 0);
        actor.set_clipboard_client_available(clipboard_client_available(&clients));
        assert_eq!(actor.write(b"\x1b]52;c;?\x07"), b"");
        assert_eq!(actor.take_pty_replies(), b"\x1b]52;c;\x1b\\");

        let client = clients.get_mut(&1).unwrap();
        client.role = Role::Writable;
        client.attached = Some(AttachedClient {
            pid: None,
            tty: None,
            attached_at: String::new(),
        });
        assert!(clipboard_client_available(&clients));
    }

    #[test]
    fn wait_status_maps_signals_to_128_plus() {
        // SIGKILL death: status = 9.
        assert_eq!(decode_wait_status(Some(9)), (137, Some(9)));
        // exit 5: status = 5 << 8.
        assert_eq!(decode_wait_status(Some(5 << 8)), (5, None));
        assert_eq!(decode_wait_status(Some(0)), (0, None));
        assert_eq!(decode_wait_status(None), (-1, None));
    }

    #[test]
    fn generation_is_32_hex() {
        let g = new_generation();
        assert_eq!(g.len(), 32);
        assert!(g.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn client_commands_pass_queued_output_without_reordering_the_bridge() {
        let (tx, rx) = mpsc::channel();
        tx.send(Msg::PtyData(b"one".to_vec())).unwrap();
        tx.send(Msg::PtyData(b"two".to_vec())).unwrap();
        tx.send(Msg::PtyEof).unwrap();
        tx.send(Msg::ExternalKill).unwrap();
        let mut deferred = VecDeque::new();
        assert!(matches!(
            next_queued_message(&rx, &mut deferred, Some(Duration::ZERO)).unwrap(),
            Msg::ExternalKill
        ));
        assert!(matches!(deferred.pop_front(), Some(Msg::PtyData(b)) if b == b"one"));
        assert!(matches!(deferred.pop_front(), Some(Msg::PtyData(b)) if b == b"two"));
        assert!(matches!(deferred.pop_front(), Some(Msg::PtyEof)));
    }

    #[test]
    fn substrate_adapter_fences_generation_and_forwards_owner_loss() {
        let pair = open(24, 80).unwrap();
        let child = pair.slave.spawn_command(shell_exec("cat", &[])).unwrap();
        let session = SessionRef::new("/tmp", "daemon-adapter-test", "generation-a");
        let owner = external_owned_pair(session.clone(), pair, child).unwrap();
        let (daemon_client, daemon_stream) = owner.attach(&session).unwrap();
        let (peer_client, peer_stream) = owner.attach(&session).unwrap();
        let stale = SessionRef::new("/tmp", "daemon-adapter-test", "generation-b");
        assert!(matches!(
            owner.attach(&stale),
            Err(pty_spawn::substrate::SessionError::StaleGeneration { .. })
        ));
        assert_eq!(daemon_client.session(), peer_client.session());
        assert!(matches!(
            peer_stream.recv_timeout(Duration::from_secs(2)).unwrap(),
            SessionEvent::Lifecycle(Lifecycle::Running)
        ));

        let (tx, rx) = mpsc::channel();
        spawn_session_bridge(daemon_stream, tx);
        peer_client.input(b"equal\n".to_vec()).unwrap();
        let mut forwarded = match rx.recv_timeout(Duration::from_secs(2)).unwrap() {
            Msg::PtyData(bytes) if !bytes.is_empty() => bytes,
            _ => panic!("the bridge did not forward the input's output"),
        };
        assert!(matches!(
            peer_stream.recv_timeout(Duration::from_secs(2)).unwrap(),
            SessionEvent::Data(bytes) if !bytes.is_empty()
        ));

        // **Output read before the loss is delivered ahead of it.** The input
        // produces the terminal's echo and then `cat`'s copy, and the kernel
        // decides whether that is one read or two. When it is two, the second
        // chunk is still in flight here, so "the next message is EOF" failed
        // 36 runs of this test in 600 on Linux on 2026-09-28. The contract is:
        // the input's output, then EOF, then nothing, which is what this
        // checks.
        owner.lose_ownership();
        loop {
            match rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                Msg::PtyData(bytes) => forwarded.extend_from_slice(&bytes),
                Msg::PtyEof => break,
                _ => panic!("the bridge sent something other than output before EOF"),
            }
        }
        assert!(
            b"equal\r\nequal\r\n".starts_with(&forwarded),
            "only the input's echo and output may be forwarded, got {:?}",
            String::from_utf8_lossy(&forwarded)
        );
        assert!(
            matches!(
                rx.recv_timeout(Duration::from_secs(2)),
                Err(RecvTimeoutError::Disconnected)
            ),
            "the bridge must stop after owner loss"
        );
        assert_eq!(
            daemon_client.input(Vec::new()),
            Err(pty_spawn::substrate::SessionError::OwnerLost)
        );
    }

    #[test]
    fn substrate_adapter_clients_share_lifecycle_state() {
        let pair = open(24, 80).unwrap();
        let session = SessionRef::new("/tmp", "daemon-adapter-lifecycle", "generation-a");
        let child = pair
            .slave
            .spawn_command(shell_exec("sleep", &["30".to_string()]))
            .unwrap();
        let owner = external_owned_pair(session.clone(), pair, child).unwrap();
        let (first, first_stream) = owner.attach(&session).unwrap();
        let (second, second_stream) = owner.attach(&session).unwrap();
        for stream in [&first_stream, &second_stream] {
            assert!(matches!(
                stream.recv_timeout(Duration::from_secs(2)).unwrap(),
                SessionEvent::Lifecycle(Lifecycle::Running)
            ));
        }

        first.terminate().unwrap();
        let event = second_stream.recv_timeout(Duration::from_secs(2)).unwrap();
        let exit = match event {
            SessionEvent::OutputClosed => {
                second_stream.recv_timeout(Duration::from_secs(2)).unwrap()
            }
            event => event,
        };
        assert!(matches!(
            exit,
            SessionEvent::Lifecycle(Lifecycle::Exited(_))
        ));
        assert!(matches!(second.lifecycle(), Ok(Lifecycle::Exited(_))));
    }
}

#[cfg(test)]
mod accept_failure_tests {
    use super::accept_failure_passes;
    use std::io::{Error, ErrorKind};

    /// The old loop left on ANY error, which took the daemon deaf while it
    /// went on reporting itself as running. These are the failures that end
    /// on their own, so leaving on them is the wrong answer.
    ///
    /// This covers the decision, not the loop. Nothing here proves the
    /// acceptor keeps serving after a real descriptor shortage; that needs a
    /// daemon under a lowered descriptor limit and is not written.
    #[test]
    fn failures_that_pass_are_not_fatal() {
        for kind in [
            ErrorKind::Interrupted,
            ErrorKind::ConnectionAborted,
            ErrorKind::WouldBlock,
            ErrorKind::TimedOut,
        ] {
            assert!(
                accept_failure_passes(&Error::new(kind, "x")),
                "{kind:?} should not end the acceptor"
            );
        }
        for errno in [libc::EMFILE, libc::ENFILE] {
            assert!(
                accept_failure_passes(&Error::from_raw_os_error(errno)),
                "errno {errno} should not end the acceptor"
            );
        }
    }

    /// A listener that cannot work is a different answer, and the daemon
    /// shuts down rather than sitting deaf while the registry says running.
    #[test]
    fn a_broken_listener_is_fatal() {
        for errno in [libc::EBADF, libc::EINVAL, libc::ENOTSOCK] {
            assert!(
                !accept_failure_passes(&Error::from_raw_os_error(errno)),
                "errno {errno} should end the acceptor"
            );
        }
    }
}
