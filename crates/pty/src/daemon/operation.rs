//! `pty __daemon --launch-operation <h>`: the daemon of a launch operation.
//!
//! Three threads share the work:
//!
//! - **The actor** alone makes transitions, writes the record and issues
//!   tickets. It runs for the whole operation, before and after `ready`, and
//!   ends at transfer or at retirement.
//! - **The control I/O thread** turns the owner's frames into actor events
//!   and writes the actor's frames, never blocking on either.
//! - **The startup worker** (this process's main thread) performs effects:
//!   publication and the held child's fork, each only once the actor commits
//!   its ticket. After `ready` the same thread serves clients, exactly as
//!   any daemon does.
//!
//! A ticket goes `issued → committed → completed`, or `issued → revoked`. A
//! cancel processed before a commit revokes the ticket and no effect
//! happens; a committed ticket is always waited for before retirement is
//! recorded, so `retirement: verified` never races an effect in flight.

use std::collections::VecDeque;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use pty_core::events::EventWriter;
use pty_core::operation::barrier::{self, barrier};
use pty_core::operation::exact::{Exit, ExitWatch};
use pty_core::operation::frames::{
    FRAME_MAX_BYTES, OwnerCommand, OwnerFrame, RejectCode, TicketView, parse_owner_frame,
    producer_frame,
};
use pty_core::operation::settle::{MARKER_FIELD, observe_occupancy, occupancy_rule};
use pty_core::operation::{
    AFTER_EXIT_CAP, Cause, Decision, Incarnation, Liveness, OperationLock, OperationRecord,
    OperationStore, Phase, ProcessId, Retirement, RetirementPhase, State, Transfer, Writer,
    WriterRole, operation_digest,
};
use pty_core::proctable::ProcTable;
use pty_core::registry::{self, MutateOptions, MutateStatus};
use pty_spawn::substrate::{ExitStatus, SessionRef};

use super::DaemonConfig;
use super::held::{self, Ack, HeldChild, HeldSpec};
use super::lifecycle::{
    Daemon, DaemonParts, Msg, Publication, live_identity, new_generation, prepare_publication, publish_session,
    spawn_acceptor, spawn_session_bridge,
};

/// The daemon's link to its operation's actor while it serves.
pub(crate) struct OperationLink {
    actor: Sender<ActorEvent>,
    transferred: Arc<AtomicBool>,
    deferred_exit: Option<ExitStatus>,
    /// The actor retired the session: close without recording anything.
    pub(crate) ended: bool,
}

impl OperationLink {
    /// Before transfer a child exit is the actor's to judge: hold it, and
    /// hand it back to normal handling if the transfer commits after all.
    pub(crate) fn holds(&mut self, status: ExitStatus) -> bool {
        if self.transferred.load(Ordering::SeqCst) {
            return false;
        }
        if self.deferred_exit.is_none() {
            self.deferred_exit = Some(status);
            let _ = self.actor.send(ActorEvent::ChildExited);
        }
        true
    }

    /// Before transfer a kill retires the operation instead of ending a
    /// session that never ran.
    pub(crate) fn intercepts_kill(&self) -> bool {
        if self.transferred.load(Ordering::SeqCst) {
            return false;
        }
        let _ = self.actor.send(ActorEvent::Shutdown);
        true
    }

    pub(crate) fn take_deferred_exit(&mut self) -> Option<ExitStatus> {
        self.deferred_exit.take()
    }
}

/// What the actor hears.
enum ActorEvent {
    Owner(OwnerInput),
    /// The I/O thread has read everything the owner had queued.
    Synced,
    Commit {
        ticket: u64,
        reply: Sender<bool>,
    },
    Completed {
        ticket: u64,
        result: Result<Completion, String>,
    },
    ChildAck(Ack),
    ReadyPoint(Sender<bool>),
    ChildExited,
    Shutdown,
    /// A test released a barrier the actor parked a step at.
    Released(&'static str),
}

enum OwnerInput {
    Frame(OwnerFrame),
    Eof,
}

enum Completion {
    Published,
    Forked {
        pid: i32,
        gate: Arc<OwnedFd>,
        /// The substrate's handle, whose signals are fenced against reaping:
        /// the child's pid cannot be reused while it can still be signalled.
        session: pty_spawn::substrate::SessionClient,
    },
}

/// What the startup worker is told to do.
enum WorkerCmd {
    Publish {
        ticket: u64,
        generation: String,
        created_at: String,
        predecessor: Option<String>,
        marker: Value,
    },
    ForkChild {
        ticket: u64,
    },
    ReadyPoint,
    Stop(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TicketKind {
    Publish,
    ForkChild,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TicketState {
    Issued,
    Committed,
    Completed,
    Revoked,
}

struct Ticket {
    id: u64,
    kind: TicketKind,
    state: TicketState,
}

impl Ticket {
    fn view(&self) -> TicketView {
        TicketView {
            id: self.id,
            kind: match self.kind {
                TicketKind::Publish => "publish",
                TicketKind::ForkChild => "fork-child",
            },
            state: match self.state {
                TicketState::Issued => "issued",
                TicketState::Committed => "committed",
                TicketState::Completed => "completed",
                TicketState::Revoked => "revoked",
            },
        }
    }
}

/// Read an inherited descriptor number from the environment.
fn env_fd(name: &str) -> Option<RawFd> {
    let fd = std::env::var(name).ok()?.parse::<RawFd>().ok()?;
    // SAFETY: F_GETFD only inspects the descriptor.
    (fd >= 0 && unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0).then_some(fd)
}

fn set_cloexec(fd: RawFd) {
    // SAFETY: fcntl on an inherited descriptor this process owns.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
}

/// The daemon entry for `--launch-operation <h>`. Returns the exit status.
pub(crate) fn main(digest: &str) -> i32 {
    let (Some(gate), Some(control)) = (
        env_fd(pty_lifecycle::GATE_FD_ENV),
        env_fd(pty_lifecycle::OPERATION_FD_ENV),
    ) else {
        eprintln!("pty daemon: a launch operation needs its gate and control descriptors");
        return 64;
    };
    // The config arrives on fd 3; it is read to its end and closed below.
    for fd in [gate, control, 3] {
        set_cloexec(fd);
    }
    if let Some(ready) = env_fd(pty_lifecycle::READY_FD_ENV) {
        set_cloexec(ready);
    }

    // The gate first: until the launcher opens it nothing here may act.
    let mut byte = 0u8;
    loop {
        // SAFETY: one byte into a local from the inherited gate.
        let n = unsafe { libc::read(gate, (&mut byte as *mut u8).cast(), 1) };
        if n == 1 {
            break;
        }
        if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        // SAFETY: exit without running any handler or destructor.
        unsafe { libc::_exit(held::NOT_RELEASED) };
    }
    // SAFETY: the gate's read end is ours and needed no longer.
    unsafe {
        libc::close(gate);
        libc::setsid();
    }
    super::set_process_title("pty-daemon");
    // The umask the session child inherits today: read once, while this is
    // the only thread.
    // SAFETY: umask(2) has no preconditions; the value is restored at once.
    let child_umask = unsafe {
        let mask = libc::umask(0o077);
        libc::umask(mask);
        mask
    };

    let cfg = match DaemonConfig::from_process() {
        Ok(cfg) => cfg,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };
    let readiness = super::ReadyNotifier::from_process();
    let store = OperationStore::current();
    let lock = match OperationLock::exclusive(&store, digest, Duration::from_secs(5)) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            eprintln!("pty daemon: the operation lock is held by another writer");
            return 1;
        }
        Err(error) => {
            eprintln!("pty daemon: the operation lock: {error}");
            return 1;
        }
    };
    let me = match ProcessId::current() {
        Ok(me) => me,
        Err(error) => {
            eprintln!("pty daemon: {error}");
            return 1;
        }
    };
    let record = match store.read(digest) {
        Ok(Some(record)) => record,
        _ => {
            eprintln!("pty daemon: the operation record is missing or unreadable");
            return 1;
        }
    };
    let field = |key: &str| {
        record
            .envelope
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let (Some(operation_id), Some(runtime_id)) = (field("operationId"), field("runtimeId")) else {
        eprintln!("pty daemon: the operation record has no envelope");
        return 1;
    };
    if record.state != State::Pending
        || record.phase != Phase::DaemonGated
        || record.writer.role != WriterRole::Producer
        || record.writer.id.as_ref() != Some(&me)
        || operation_digest(&operation_id) != digest
        || runtime_id != cfg.name
    {
        eprintln!("pty daemon: the operation record does not name this daemon as its producer");
        return 1;
    }

    let (actor_tx, actor_rx) = mpsc::channel::<ActorEvent>();
    let (worker_tx, worker_rx) = mpsc::channel::<WorkerCmd>();
    let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
    let transferred = Arc::new(AtomicBool::new(false));
    forward_kill_signals(actor_tx.clone(), msg_tx.clone());
    // SAFETY: the inherited control descriptor, owned by the I/O thread now.
    let control = unsafe { OwnedFd::from_raw_fd(control) };
    let io = match ControlHandle::start(
        control,
        actor_tx.clone(),
        &operation_id,
        &record.envelope_sha256,
    ) {
        Ok(io) => io,
        Err(error) => {
            eprintln!("pty daemon: the control channel: {error}");
            return 1;
        }
    };
    let actor = Actor {
        store,
        digest: digest.to_string(),
        lock: Some(lock),
        me,
        operation_id,
        envelope_sha256: record.envelope_sha256.clone(),
        envelope: record.envelope.clone(),
        expected: record
            .envelope
            .get("expectedIncarnation")
            .and_then(Value::as_str)
            .map(str::to_string),
        runtime_id: runtime_id.clone(),
        root: registry::session_dir(),
        record,
        io,
        worker: worker_tx,
        msgs: msg_tx.clone(),
        transferred: Arc::clone(&transferred),
        seq: 0,
        tickets: Vec::new(),
        stage: Stage::Deciding,
        retiring: None,
        serving: false,
        published: false,
        child: None,
        generation: String::new(),
        own: actor_tx.clone(),
        pending: None,
        deferred_transfer: None,
        syncing: false,
    };
    std::thread::Builder::new()
        .name("pty-operation-actor".into())
        .spawn(move || actor.run(actor_rx))
        .expect("the operation actor thread starts");

    Worker {
        cfg,
        actor: actor_tx,
        msgs: msg_tx,
        child_umask,
        transferred,
    }
    .run(worker_rx, msg_rx, readiness)
}

/// SIGTERM and SIGINT from the start: before the transfer they retire the
/// operation through its actor; once the actor is done they are the serving
/// daemon's external kill, as for any session.
fn forward_kill_signals(actor: Sender<ActorEvent>, msgs: Sender<Msg>) {
    let Ok(mut signals) = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
    ]) else {
        return;
    };
    std::thread::spawn(move || {
        for _ in signals.forever() {
            if actor.send(ActorEvent::Shutdown).is_err() && msgs.send(Msg::ExternalKill).is_err() {
                return;
            }
        }
    });
}

// ── The startup worker ───────────────────────────────────────────────────

struct Worker {
    cfg: DaemonConfig,
    actor: Sender<ActorEvent>,
    msgs: Sender<Msg>,
    child_umask: libc::mode_t,
    transferred: Arc<AtomicBool>,
}

/// What publication left for the rest of startup.
struct Published {
    events: EventWriter,
    listener: std::os::unix::net::UnixListener,
    child_env: std::collections::BTreeMap<String, String>,
    pair: pty_spawn::PtyPair,
}

/// What the fork left: the substrate owns the child; the slave stays open
/// until the child holds the terminal itself.
struct Serving {
    events: Option<EventWriter>,
    owner: pty_spawn::substrate::SessionOwner,
    session: pty_spawn::substrate::SessionClient,
    child_pid: i32,
    listener_fd: i32,
    slave: Option<Box<dyn pty_spawn::SlavePty + Send>>,
}

impl Worker {
    fn commit(&self, ticket: u64) -> bool {
        let (reply, answer) = mpsc::channel();
        if self
            .actor
            .send(ActorEvent::Commit { ticket, reply })
            .is_err()
        {
            return false;
        }
        answer.recv().unwrap_or(false)
    }

    fn complete(&self, ticket: u64, result: Result<Completion, String>) {
        let _ = self.actor.send(ActorEvent::Completed { ticket, result });
    }

    fn run(
        self,
        commands: Receiver<WorkerCmd>,
        rx: Receiver<Msg>,
        readiness: super::ReadyNotifier,
    ) -> i32 {
        let mut published: Option<Published> = None;
        let mut serving: Option<Serving> = None;
        let mut generation = String::new();
        loop {
            let Ok(command) = commands.recv() else {
                return 1;
            };
            match command {
                WorkerCmd::Publish {
                    ticket,
                    generation: chosen,
                    created_at,
                    predecessor,
                    marker,
                } => {
                    generation = chosen;
                    // Everything that is not an effect is prepared before the
                    // commit is asked for.
                    let prepared = self.prepare(&generation);
                    barrier("publish-issued-before-commit");
                    let (child_env, pair) = match prepared {
                        Ok(prepared) => prepared,
                        Err(message) => {
                            self.complete(ticket, Err(message));
                            continue;
                        }
                    };
                    if !self.commit(ticket) {
                        continue;
                    }
                    barrier("publish-committed-before-effect");
                    let publication_lock = match prepare_publication(&self.cfg) {
                        Ok(guard) => guard,
                        Err(error) => {
                            self.complete(ticket, Err(error));
                            continue;
                        }
                    };
                    let events = EventWriter::new(&self.cfg.name);
                    let result = clear_predecessor(&self.cfg.name, predecessor.as_deref())
                        .and_then(|()| {
                            let tags = self.cfg.tags().cloned().unwrap_or_default();
                            publish_session(&Publication {
                                cfg: &self.cfg,
                                generation: &generation,
                                created_at,
                                published_tags: &tags,
                                child_env: &child_env,
                                events: &events,
                                marker: Some(marker),
                                creation_lock: &publication_lock,
                            })
                        });
                    // Completion hands the claim back to the actor, which
                    // releases it nonblocking before issuing ForkChild.
                    // End the worker's publication critical section first:
                    // otherwise that release races this still-held pin and
                    // can leave our live-PID claim behind after pin cleanup.
                    drop(publication_lock);
                    match result {
                        Ok((listener, _metadata)) => {
                            published = Some(Published {
                                events,
                                listener,
                                child_env,
                                pair,
                            });
                            self.complete(ticket, Ok(Completion::Published));
                        }
                        Err(message) => self.complete(ticket, Err(message)),
                    }
                }
                WorkerCmd::ForkChild { ticket } => {
                    barrier("published-before-child-fork");
                    let Some(publication) = published.take() else {
                        self.complete(ticket, Err("nothing was published".into()));
                        continue;
                    };
                    let spec = publication
                        .pair
                        .master
                        .tty_name()
                        .ok_or_else(|| "the terminal has no path".to_string())
                        .and_then(|slave| {
                            HeldSpec::build(
                                &self.cfg.command,
                                &self.cfg.args,
                                &self.cfg.cwd(),
                                &publication.child_env,
                                self.child_umask,
                                &slave,
                            )
                        });
                    let spec = match spec {
                        Ok(spec) => spec,
                        Err(message) => {
                            published = Some(publication);
                            self.complete(ticket, Err(message));
                            continue;
                        }
                    };
                    if !self.commit(ticket) {
                        published = Some(publication);
                        continue;
                    }
                    // Re-admit this later effect under the daemon's own PID
                    // claim. Unlike publication delegated by a launcher, no
                    // worker survives this claim's owner: if this daemon dies,
                    // it cannot fork again. Do not carry a flock into fork:
                    // a child stopped before descriptor cleanup inherits the
                    // open description and would pin a dead daemon's name.
                    // This committed ticket stays in flight while the worker
                    // waits; the actor can accept cancellation but cannot
                    // verify retirement until this bounded effect completes.
                    // Recheck the fence and generation only after admission.
                    let child_publication_lock = match registry::wait_for_metadata_lock(
                        &self.cfg.name,
                        registry::METADATA_PATCH_LOCK_WAIT,
                    ).map_err(|refusal| match refusal {
                        registry::LockRefusal::Busy => registry::metadata_busy_message(&self.cfg.name),
                        registry::LockRefusal::Unavailable(cause) => cause,
                    }).and_then(|guard| {
                        self.cfg.check_publication()?;
                        let current = registry::read_metadata(&self.cfg.name);
                        if !current.as_ref().is_some_and(|metadata| {
                            metadata.generation.as_deref() == Some(generation.as_str())
                                && metadata.daemon_pid == Some(std::process::id() as i32)
                        }) {
                            return Err("published generation changed before child creation".into());
                        }
                        Ok(guard)
                    }) {
                        Ok(guard) => guard,
                        Err(error) => {
                            published = Some(publication);
                            self.complete(ticket, Err(error));
                            continue;
                        }
                    };
                    let forked = match held::fork_held(&spec) {
                        Ok(forked) => forked,
                        Err(error) => {
                            published = Some(publication);
                            self.complete(ticket, Err(format!("fork: {error}")));
                            continue;
                        }
                    };
                    drop(child_publication_lock);
                    let gate = Arc::new(forked.gate);
                    let Published {
                        events,
                        listener,
                        pair,
                        ..
                    } = publication;
                    let session = SessionRef::new(
                        registry::session_dir(),
                        self.cfg.name.clone(),
                        generation.clone(),
                    );
                    let substrate = pty_spawn::external_owned_master_attached(
                        session,
                        pair.master,
                        Box::new(HeldChild {
                            pid: forked.pid as u32,
                        }),
                    );
                    let (owner, client, stream) = match substrate {
                        Ok(parts) => parts,
                        Err(error) => {
                            // Nothing owns the child: end it here, unreleased.
                            held::shut(&gate);
                            // Our own child, killed and reaped.
                            let _ = pty_core::kill::process(forked.pid, libc::SIGKILL);
                            // SAFETY: waitpid on our own child.
                            unsafe {
                                libc::waitpid(forked.pid, std::ptr::null_mut(), 0);
                            }
                            self.complete(ticket, Err(format!("the session substrate: {error}")));
                            continue;
                        }
                    };
                    spawn_session_bridge(stream, self.msgs.clone());
                    let listener_fd = listener.as_raw_fd();
                    spawn_acceptor(listener, self.msgs.clone());
                    let fenced = client.clone();
                    serving = Some(Serving {
                        events: Some(events),
                        owner,
                        session: client,
                        child_pid: forked.pid,
                        listener_fd,
                        slave: Some(pair.slave),
                    });
                    barrier("child-forked-before-record");
                    self.complete(
                        ticket,
                        Ok(Completion::Forked {
                            pid: forked.pid,
                            gate: Arc::clone(&gate),
                            session: fenced,
                        }),
                    );
                    barrier("child-prepared-before-ack-read");
                    let ack = held::read_ack(&gate);
                    // The child holds the terminal (or never will): the
                    // daemon's own slave is no longer needed.
                    if let Some(serving) = serving.as_mut() {
                        serving.slave = None;
                    }
                    let _ = self.actor.send(ActorEvent::ChildAck(ack));
                }
                WorkerCmd::ReadyPoint => {
                    barrier("before-ready");
                    let (reply, answer) = mpsc::channel();
                    if self.actor.send(ActorEvent::ReadyPoint(reply)).is_err()
                        || !answer.recv().unwrap_or(false)
                    {
                        continue;
                    }
                    let Some(mut serving) = serving.take() else {
                        return 1;
                    };
                    let Some(events) = serving.events.take() else {
                        return 1;
                    };
                    readiness.notify();
                    let daemon = Daemon::assemble(DaemonParts {
                        cfg: self.cfg,
                        generation,
                        owner: serving.owner,
                        session: serving.session,
                        child_pid: serving.child_pid,
                        child_identity: live_identity(serving.child_pid),
                        events,
                        rx,
                        listener_fd: serving.listener_fd,
                        startup_lease: None,
                        operation: Some(OperationLink {
                            actor: self.actor,
                            transferred: self.transferred,
                            deferred_exit: None,
                            ended: false,
                        }),
                    });
                    return daemon.serve();
                }
                WorkerCmd::Stop(code) => return code,
            }
        }
    }

    /// Everything publication needs that is not itself an effect.
    fn prepare(
        &self,
        generation: &str,
    ) -> Result<
        (
            std::collections::BTreeMap<String, String>,
            pty_spawn::PtyPair,
        ),
        String,
    > {
        let child_env = super::env::build_child_env(&self.cfg, generation)?;
        let cwd = self.cfg.cwd();
        if let Some(reason) = super::env::describe_invalid_cwd(&cwd) {
            return Err(super::env::invalid_cwd_error(
                &reason,
                &self.cfg.name,
                &self.cfg.command,
            ));
        }
        let pair = pty_spawn::open(self.cfg.rows(), self.cfg.cols()).map_err(|e| {
            format!(
                "Failed to open a PTY for session \"{}\": {e}",
                self.cfg.name
            )
        })?;
        Ok((child_env, pair))
    }
}

/// Clear the ended predecessor the launch decision named, and only it: under
/// the event lock (the daemon already holds the creation lock), the row must
/// still be that exact ended incarnation.
fn clear_predecessor(name: &str, predecessor: Option<&str>) -> Result<(), String> {
    let Some(expected) = predecessor else {
        return Ok(());
    };
    let _events = registry::take_event_lock(name)?;
    match observe_occupancy(&registry::session_dir(), name) {
        pty_core::operation::settle::Occupancy::Ended {
            incarnation: Some(found),
        } if found == expected => {
            registry::cleanup_all_while_locked(name);
            Ok(())
        }
        pty_core::operation::settle::Occupancy::Absent => Ok(()),
        other => Err(format!("the predecessor row changed: {other:?}")),
    }
}

// ── The control channel ──────────────────────────────────────────────────

enum IoCmd {
    Send(Vec<u8>),
    Sync,
    Close,
}

/// The actor's handle on the control I/O thread.
struct ControlHandle {
    commands: Sender<IoCmd>,
    wake: OwnedFd,
    done: Option<std::thread::JoinHandle<()>>,
}

impl ControlHandle {
    fn start(
        fd: OwnedFd,
        events: Sender<ActorEvent>,
        operation_id: &str,
        envelope_sha256: &str,
    ) -> std::io::Result<ControlHandle> {
        let mut pipe = [0 as RawFd; 2];
        // SAFETY: pipe(2) fills two descriptors, made close-on-exec below
        // (at creation, where the platform allows it).
        #[cfg(target_os = "linux")]
        let made = unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) };
        #[cfg(not(target_os = "linux"))]
        let made = unsafe { libc::pipe(pipe.as_mut_ptr()) };
        if made != 0 {
            return Err(std::io::Error::last_os_error());
        }
        for end in pipe {
            set_cloexec(end);
            // SAFETY: fcntl on a descriptor this call created.
            unsafe {
                let flags = libc::fcntl(end, libc::F_GETFL);
                libc::fcntl(end, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }
        // SAFETY: freshly created descriptors owned here.
        let (wake_read, wake) =
            unsafe { (OwnedFd::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1])) };
        // SAFETY: the control descriptor is ours; its readers are this thread.
        unsafe {
            let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let (commands, inbox) = mpsc::channel();
        let operation_id = operation_id.to_string();
        let envelope_sha256 = envelope_sha256.to_string();
        let done = std::thread::Builder::new()
            .name("pty-operation-control".into())
            .spawn(move || {
                control_io(
                    fd,
                    wake_read,
                    inbox,
                    events,
                    &operation_id,
                    &envelope_sha256,
                )
            })?;
        Ok(ControlHandle {
            commands,
            wake,
            done: Some(done),
        })
    }

    fn command(&self, command: IoCmd) {
        let _ = self.commands.send(command);
        // SAFETY: one byte to our own non-blocking wake pipe; a full pipe
        // already holds a wake-up.
        unsafe {
            libc::write(self.wake.as_raw_fd(), [1u8].as_ptr().cast(), 1);
        }
    }

    fn send(&self, frame: Vec<u8>) {
        self.command(IoCmd::Send(frame));
    }

    fn sync(&self) {
        self.command(IoCmd::Sync);
    }

    /// Flush what is queued, then close the control end, and wait for it.
    fn close(&mut self) {
        self.command(IoCmd::Close);
        if let Some(done) = self.done.take() {
            let _ = done.join();
        }
    }
}

/// How long a closing channel keeps trying to deliver queued frames to an
/// owner that is not reading.
const CLOSE_FLUSH: Duration = Duration::from_secs(2);

fn control_io(
    fd: OwnedFd,
    wake: OwnedFd,
    inbox: Receiver<IoCmd>,
    events: Sender<ActorEvent>,
    operation_id: &str,
    envelope_sha256: &str,
) {
    let mut input: Vec<u8> = Vec::new();
    let mut out: VecDeque<Vec<u8>> = VecDeque::new();
    let mut offset = 0usize;
    let mut reading = true;
    let mut writable = true;
    let mut closing: Option<Instant> = None;
    let raw = fd.as_raw_fd();

    let read_available = |input: &mut Vec<u8>, reading: &mut bool| {
        let mut buf = [0u8; 16384];
        while *reading {
            // SAFETY: reading into a local buffer from our own descriptor.
            let n =
                unsafe { libc::recv(raw, buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) };
            if n > 0 {
                input.extend_from_slice(&buf[..n as usize]);
                while let Some(end) = input.iter().position(|b| *b == b'\n') {
                    let frame = parse_owner_frame(&input[..end], operation_id, envelope_sha256);
                    let malformed = frame == OwnerFrame::Malformed;
                    let _ = events.send(ActorEvent::Owner(OwnerInput::Frame(frame)));
                    input.drain(..=end);
                    if malformed {
                        *reading = false;
                        input.clear();
                        return;
                    }
                }
                if input.len() >= FRAME_MAX_BYTES {
                    let _ =
                        events.send(ActorEvent::Owner(OwnerInput::Frame(OwnerFrame::Malformed)));
                    *reading = false;
                    input.clear();
                    return;
                }
                continue;
            }
            if n == 0 {
                *reading = false;
                let _ = events.send(ActorEvent::Owner(OwnerInput::Eof));
                return;
            }
            match std::io::Error::last_os_error().kind() {
                std::io::ErrorKind::Interrupted => continue,
                std::io::ErrorKind::WouldBlock => return,
                _ => {
                    *reading = false;
                    let _ = events.send(ActorEvent::Owner(OwnerInput::Eof));
                    return;
                }
            }
        }
    };

    loop {
        loop {
            match inbox.try_recv() {
                Ok(IoCmd::Send(frame)) => {
                    if writable {
                        out.push_back(frame);
                    }
                }
                Ok(IoCmd::Sync) => {
                    read_available(&mut input, &mut reading);
                    let _ = events.send(ActorEvent::Synced);
                }
                Ok(IoCmd::Close) => {
                    closing.get_or_insert_with(|| Instant::now() + CLOSE_FLUSH);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    closing.get_or_insert_with(|| Instant::now() + CLOSE_FLUSH);
                    break;
                }
            }
        }
        if let Some(deadline) = closing
            && (out.is_empty() || !writable || Instant::now() >= deadline)
        {
            // Unread input would reset the owner's end and could cost it the
            // frames already sent: discard it, then close.
            let mut drain = [0u8; 16384];
            // SAFETY: draining and shutting down our own socket.
            unsafe {
                while libc::recv(
                    raw,
                    drain.as_mut_ptr().cast(),
                    drain.len(),
                    libc::MSG_DONTWAIT,
                ) > 0
                {}
                libc::shutdown(raw, libc::SHUT_RDWR);
            }
            return;
        }
        let mut fds = [
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: raw,
                events: (if reading { libc::POLLIN } else { 0 })
                    | (if writable && !out.is_empty() {
                        libc::POLLOUT
                    } else {
                        0
                    }),
                revents: 0,
            },
        ];
        let watch_socket = fds[1].events != 0;
        let count = if watch_socket { 2 } else { 1 };
        let timeout = closing.map_or(-1, |deadline| {
            deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(i32::MAX as u128) as i32
        });
        // SAFETY: `count` pollfds of a live array.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), count, timeout) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if fds[0].revents != 0 {
            let mut drain = [0u8; 64];
            // SAFETY: draining our own non-blocking wake pipe.
            while unsafe { libc::read(wake.as_raw_fd(), drain.as_mut_ptr().cast(), drain.len()) }
                > 0
            {}
        }
        if watch_socket
            && fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
            && reading
        {
            read_available(&mut input, &mut reading);
        }
        if watch_socket && fds[1].revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) != 0 {
            while let Some(front) = out.front() {
                // SAFETY: writing the unsent tail of a queued frame.
                let n = unsafe {
                    libc::send(
                        raw,
                        front[offset..].as_ptr().cast(),
                        front.len() - offset,
                        send_flags() | libc::MSG_DONTWAIT,
                    )
                };
                if n > 0 {
                    offset += n as usize;
                    if offset == front.len() {
                        out.pop_front();
                        offset = 0;
                    }
                    continue;
                }
                let error = std::io::Error::last_os_error().kind();
                if error == std::io::ErrorKind::Interrupted {
                    continue;
                }
                if error != std::io::ErrorKind::WouldBlock {
                    // The owner is gone; frames are only ever informational.
                    writable = false;
                    out.clear();
                    offset = 0;
                }
                break;
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn send_flags() -> libc::c_int {
    libc::MSG_NOSIGNAL
}

#[cfg(not(target_os = "linux"))]
fn send_flags() -> libc::c_int {
    0
}

// ── The actor ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Before the decisive transition.
    Deciding,
    /// Launched; publication and the held child under way.
    Starting,
    /// `held` sent; `ready` may follow.
    Held,
    /// `ready` sent; serving.
    Ready,
    Done,
}

/// A step the actor parked at a held barrier, finished when the test
/// releases it. The actor keeps processing everything else meanwhile.
enum Pending {
    /// The launch decision (`before-decision`).
    Decide,
    /// An accepted transfer before its commit (`transfer-before-commit`).
    TransferCommit { request_id: String },
    /// A committed transfer before its release (`transfer-committed-before-release`).
    TransferRelease,
    /// A released transfer before its acknowledgement (`released-before-ack`).
    TransferAck { released: bool },
}

/// A retirement whose request is durably recorded. It waits for committed
/// tickets before any further effect.
struct Retiring;

struct ChildRecord {
    id: Option<ProcessId>,
    pid: i32,
    gate: Arc<OwnedFd>,
    session: pty_spawn::substrate::SessionClient,
}

struct Actor {
    store: OperationStore,
    digest: String,
    lock: Option<OperationLock>,
    record: OperationRecord,
    me: ProcessId,
    operation_id: String,
    envelope_sha256: String,
    envelope: Value,
    expected: Option<String>,
    runtime_id: String,
    root: PathBuf,
    io: ControlHandle,
    worker: Sender<WorkerCmd>,
    msgs: Sender<Msg>,
    transferred: Arc<AtomicBool>,
    seq: u64,
    tickets: Vec<Ticket>,
    stage: Stage,
    retiring: Option<Retiring>,
    /// The worker is serving: retirement ends the serve loop, not the worker.
    serving: bool,
    /// Publication was committed: retirement must unwind it.
    published: bool,
    child: Option<ChildRecord>,
    generation: String,
    /// The actor's own inbox, for barrier releases.
    own: Sender<ActorEvent>,
    pending: Option<Pending>,
    /// A transfer accepted while held, committed once `ready` is sent.
    deferred_transfer: Option<String>,
    /// The decision waits for the I/O thread to read what the owner queued.
    syncing: bool,
}

fn extra(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.clone()))
        .collect()
}

impl Actor {
    fn frame(&mut self, kind: &str, more: Map<String, Value>) {
        self.seq += 1;
        let tickets: Vec<TicketView> = self.tickets.iter().map(Ticket::view).collect();
        let line = producer_frame(
            kind,
            self.seq,
            &self.operation_id,
            &self.envelope_sha256,
            &self.envelope,
            Some(&self.record),
            &tickets,
            more,
        );
        self.io.send(line);
    }

    fn reject(&mut self, code: RejectCode, request_id: Option<String>) {
        self.frame(
            "command-rejected",
            extra(&[
                ("code", json!(code.as_str())),
                ("requestId", json!(request_id)),
            ]),
        );
    }

    /// Move the record by compare-and-swap. The actor is its only writer
    /// while the daemon lives; a refusal means the record cannot be trusted
    /// to say what happens next, and the caller must not act as if it did.
    fn write(&mut self, next: OperationRecord, label: &str) -> bool {
        let Some(lock) = &self.lock else {
            return false;
        };
        match self
            .store
            .write_cas_as(lock, &self.digest, &self.record, next, label)
        {
            Ok(written) => {
                self.record = written;
                true
            }
            Err(error) => {
                super::daemon_warn!(
                    "pty daemon: operation record write refused at {label}: {error}"
                );
                false
            }
        }
    }

    /// A record write did not land. Stop: no further effect, no positive
    /// frame. The owner reads end of file; the held child, if any, reads end
    /// of file on its gate when this process exits and leaves without exec;
    /// the record is left to `settle` and `retire`.
    fn abandon(&mut self, at: &str) {
        super::daemon_warn!(
            "pty daemon: stopping the launch operation without further effects after a failed record write at {at}"
        );
        self.io.close();
        self.lock = None;
        self.stage = Stage::Done;
        if self.serving {
            let _ = self.msgs.send(Msg::OperationEnd(1));
        } else {
            let _ = self.worker.send(WorkerCmd::Stop(1));
        }
    }

    /// Park `pending` at `name` if a test armed it; the release arrives as
    /// an event. `false` means go on now.
    fn hold(&mut self, name: &'static str, pending: Pending) -> bool {
        if !barrier::armed(name) {
            return false;
        }
        self.pending = Some(pending);
        let own = self.own.clone();
        std::thread::spawn(move || {
            barrier::wait_released(name);
            let _ = own.send(ActorEvent::Released(name));
        });
        true
    }

    fn issue(&mut self, kind: TicketKind) -> u64 {
        let id = self.tickets.len() as u64 + 1;
        self.tickets.push(Ticket {
            id,
            kind,
            state: TicketState::Issued,
        });
        id
    }

    fn ticket(&mut self, id: u64) -> Option<&mut Ticket> {
        self.tickets.iter_mut().find(|ticket| ticket.id == id)
    }

    fn committed_in_flight(&self) -> bool {
        self.tickets
            .iter()
            .any(|ticket| ticket.state == TicketState::Committed)
    }

    fn is_transferred(&self) -> bool {
        self.record.state == State::Transferred
    }

    fn run(mut self, events: Receiver<ActorEvent>) {
        self.frame("accepted", Map::new());
        if !self.hold("before-decision", Pending::Decide) {
            self.begin_sync();
        }
        while self.stage != Stage::Done {
            let Ok(event) = events.recv() else {
                return;
            };
            self.on_event(event);
        }
    }

    /// Everything the owner queued is read before the decision: a queued
    /// cancel, end of file or malformed input arrives as an event first.
    fn begin_sync(&mut self) {
        self.syncing = true;
        self.io.sync();
    }

    /// The decisive transition (contract §7.3), once nothing withdrew it.
    fn decide(&mut self) {
        let lock_path = registry::lock_path(&self.runtime_id);
        let holds_creation_lock = std::fs::read_to_string(&lock_path)
            .ok()
            .is_some_and(|owner| owner.split_whitespace().next() == Some(self.me.pid.to_string().as_str())
                && owner.split_whitespace().nth(1) == std::env::var("PTY_CREATION_LOCK_NONCE").ok().as_deref());
        if !holds_creation_lock {
            return self.cancel_before_launch(
                Cause::Refused,
                None,
                Some("creation-lock-not-delegated".into()),
            );
        }
        let occupancy = observe_occupancy(&self.root, &self.runtime_id);
        let predecessor = match occupancy_rule(&occupancy, self.expected.as_deref()) {
            Ok(predecessor) => predecessor,
            Err((cause, observed)) => {
                return self.cancel_before_launch(cause, None, Some(observed));
            }
        };
        let created_at = registry::now_iso8601();
        let generation = new_generation();
        let ticket = self.issue(TicketKind::Publish);
        let mut launched = self.record.clone();
        launched.state = State::Launched;
        launched.decision = Some(Decision::now(Cause::Launch, None, predecessor.clone()));
        launched.incarnation = Some(Incarnation {
            id: format!("{}:{created_at}", self.me.pid),
            runtime_id: self.runtime_id.clone(),
            generation: generation.clone(),
            daemon: self.me.clone(),
            child: None,
        });
        if !self.write(launched, "decision") {
            return self.abandon("decision");
        }
        self.generation = generation.clone();
        self.stage = Stage::Starting;
        let decision = json!(self.record.decision);
        self.frame("decision", extra(&[("decision", decision)]));
        let _ = self.worker.send(WorkerCmd::Publish {
            ticket,
            generation,
            created_at,
            predecessor,
            marker: json!({"digest": self.digest, "phase": "startup"}),
        });
    }

    /// `cancelled` before anything was published: no registry effect. The
    /// decision frame is sent only once the decision is durable.
    fn cancel_before_launch(
        &mut self,
        cause: Cause,
        request_id: Option<String>,
        observed: Option<String>,
    ) {
        let mut cancelled = self.record.clone();
        cancelled.state = State::Cancelled;
        cancelled.writer = Writer::none();
        cancelled.decision = Some(Decision::now(cause, request_id, observed));
        if !self.write(cancelled, "decision") {
            return self.abandon("decision");
        }
        let decision = json!(self.record.decision);
        self.frame("decision", extra(&[("decision", decision)]));
        registry::release_file_lock_owned_by(&registry::lock_path(&self.runtime_id), self.me.pid);
        self.finish(3);
    }

    /// Close the channel, drop the writer's lock, and end the process's
    /// startup or its serving loop.
    fn finish(&mut self, code: i32) {
        self.io.close();
        self.lock = None;
        self.stage = Stage::Done;
        if self.serving {
            let _ = self.msgs.send(Msg::OperationEnd(code));
        } else {
            let _ = self.worker.send(WorkerCmd::Stop(code));
        }
    }

    fn on_event(&mut self, event: ActorEvent) {
        match event {
            ActorEvent::Synced => {
                if self.stage == Stage::Deciding && self.syncing {
                    self.syncing = false;
                    self.decide();
                }
            }
            ActorEvent::Released(name) => self.on_released(name),
            ActorEvent::Commit { ticket, reply } => {
                let retiring = self.retiring.is_some();
                let answer = match self.ticket(ticket) {
                    Some(t) if t.state == TicketState::Issued && !retiring => {
                        t.state = TicketState::Committed;
                        if t.kind == TicketKind::Publish {
                            self.published = true;
                        }
                        true
                    }
                    Some(t) => {
                        if t.state == TicketState::Issued {
                            t.state = TicketState::Revoked;
                        }
                        false
                    }
                    None => false,
                };
                let _ = reply.send(answer);
            }
            ActorEvent::Completed { ticket, result } => self.on_completed(ticket, result),
            ActorEvent::ChildAck(ack) => self.on_ack(ack),
            ActorEvent::ReadyPoint(reply) => {
                let ok = self.stage == Stage::Held && self.retiring.is_none();
                if ok {
                    self.stage = Stage::Ready;
                    self.serving = true;
                    self.frame("ready", Map::new());
                }
                let _ = reply.send(ok);
                // A transfer accepted while held is committed now, after
                // `ready`, so the worker is already serving when it releases.
                if ok && let Some(request_id) = self.deferred_transfer.take() {
                    self.start_transfer(request_id);
                }
            }
            ActorEvent::ChildExited => {
                if self.record.state == State::Launched {
                    self.retire(None, "producer:child-exited".into());
                }
            }
            ActorEvent::Shutdown => match self.record.state {
                State::Launched => self.retire(None, "producer:terminated".into()),
                State::Pending if self.stage == Stage::Deciding => self.cancel_before_launch(
                    Cause::Refused,
                    None,
                    Some("producer:terminated".into()),
                ),
                // Committed: the session is the serving daemon's, and so is
                // the kill, even while the release and its last record write
                // are still to come. This actor commits the transfer, so a
                // kill it reads after the commit is never one it judged.
                State::Transferred => {
                    let _ = self.msgs.send(Msg::ExternalKill);
                }
                _ => {}
            },
            ActorEvent::Owner(input) => self.on_owner(input),
        }
    }

    fn on_released(&mut self, name: &'static str) {
        match (name, self.pending.take()) {
            ("before-decision", Some(Pending::Decide)) if self.stage == Stage::Deciding => {
                self.begin_sync();
            }
            ("transfer-before-commit", Some(Pending::TransferCommit { request_id })) => {
                self.commit_transfer(request_id);
            }
            ("transfer-committed-before-release", Some(Pending::TransferRelease)) => {
                self.release_transfer();
            }
            ("released-before-ack", Some(Pending::TransferAck { released })) => {
                self.finish_transfer(released);
            }
            (_, pending) => self.pending = pending,
        }
    }

    fn on_owner(&mut self, input: OwnerInput) {
        if self.stage == Stage::Deciding {
            // Before the decision any withdrawal wins at once, even while
            // the decision itself is held.
            return match input {
                OwnerInput::Eof => self.cancel_before_launch(Cause::OwnerEof, None, None),
                OwnerInput::Frame(OwnerFrame::Malformed) => {
                    self.reject(RejectCode::Malformed, None);
                    self.cancel_before_launch(Cause::Malformed, None, None)
                }
                OwnerInput::Frame(OwnerFrame::Command(OwnerCommand::Cancel { request_id })) => {
                    self.cancel_before_launch(Cause::Cancel, Some(request_id), None)
                }
                OwnerInput::Frame(OwnerFrame::Command(command)) => {
                    self.reject(RejectCode::NotHeld, Some(command.request_id().to_string()))
                }
                OwnerInput::Frame(OwnerFrame::Rejected { code, request_id }) => {
                    self.reject(code, request_id)
                }
            };
        }
        match input {
            OwnerInput::Frame(OwnerFrame::Command(OwnerCommand::Cancel { request_id })) => {
                if self.is_transferred() {
                    self.reject(RejectCode::AlreadyTransferred, Some(request_id));
                } else if self.retiring.is_some() {
                    self.reject(RejectCode::AlreadyDecided, Some(request_id));
                } else {
                    self.retire(Some(Cause::Cancel), request_id);
                }
            }
            OwnerInput::Frame(OwnerFrame::Command(OwnerCommand::Transfer {
                request_id,
                incarnation_id,
                generation,
            })) => {
                if self.is_transferred() {
                    return self.reject(RejectCode::AlreadyTransferred, Some(request_id));
                }
                if self.deferred_transfer.is_some()
                    || matches!(self.pending, Some(Pending::TransferCommit { .. }))
                {
                    return self.reject(RejectCode::AlreadyDecided, Some(request_id));
                }
                let held = self.record.phase == Phase::Held && self.child.is_some();
                if self.retiring.is_some() || !held {
                    return self.reject(RejectCode::NotHeld, Some(request_id));
                }
                let matches =
                    self.record.incarnation.as_ref().is_some_and(|inc| {
                        inc.id == incarnation_id && inc.generation == generation
                    });
                if !matches {
                    return self.reject(RejectCode::WrongIncarnation, Some(request_id));
                }
                if self.stage == Stage::Held {
                    self.deferred_transfer = Some(request_id);
                } else {
                    self.start_transfer(request_id);
                }
            }
            OwnerInput::Frame(OwnerFrame::Rejected { code, request_id }) => {
                let code = if self.is_transferred() && code != RejectCode::Malformed {
                    RejectCode::AlreadyTransferred
                } else {
                    code
                };
                self.reject(code, request_id)
            }
            OwnerInput::Frame(OwnerFrame::Malformed) => {
                self.reject(RejectCode::Malformed, None);
                if !self.is_transferred() && self.retiring.is_none() {
                    self.retire(Some(Cause::Malformed), "owner:malformed".into());
                }
            }
            // After the commit, end of file is never permission to retire.
            OwnerInput::Eof => {
                if !self.is_transferred() && self.retiring.is_none() {
                    self.retire(Some(Cause::OwnerEof), "owner:eof".into());
                }
            }
        }
    }

    fn on_completed(&mut self, id: u64, result: Result<Completion, String>) {
        let kind = match self.ticket(id) {
            Some(ticket) => {
                ticket.state = TicketState::Completed;
                ticket.kind
            }
            None => return,
        };
        match (kind, result) {
            (TicketKind::Publish, Ok(_)) => {
                // Publication is done; the creation lock's job is too.
                registry::release_file_lock_owned_by(
                    &registry::lock_path(&self.runtime_id),
                    self.me.pid,
                );
                if self.retiring.is_none() {
                    let ticket = self.issue(TicketKind::ForkChild);
                    let mut forking = self.record.clone();
                    forking.phase = Phase::ChildForking;
                    if self.write(forking, "child-forking") {
                        let _ = self.worker.send(WorkerCmd::ForkChild { ticket });
                    } else {
                        return self.abandon("child-forking");
                    }
                }
            }
            (TicketKind::ForkChild, Ok(Completion::Forked { pid, gate, session })) => {
                let id = ProcessId::of(pid as u32).ok();
                self.child = Some(ChildRecord {
                    id: id.clone(),
                    pid,
                    gate,
                    session,
                });
                if self.retiring.is_none() {
                    let Some(id) = id else {
                        // The child is already gone: nothing was released.
                        return self.retire(None, "producer:child-exited".into());
                    };
                    let mut preparing = self.record.clone();
                    preparing.phase = Phase::ChildPreparing;
                    preparing.participants.child = Some(id.clone());
                    if let Some(inc) = preparing.incarnation.as_mut() {
                        inc.child = Some(id);
                    }
                    if !self.write(preparing, "child-preparing") {
                        return self.abandon("child-preparing");
                    }
                }
            }
            (_, Ok(_)) => {}
            (_, Err(message)) => {
                super::daemon_warn!("pty daemon: launch operation startup failed: {message}");
                if self.retiring.is_none() {
                    self.retire(None, "producer:startup-failed".into());
                }
            }
        }
        if self.retiring.is_some() && !self.committed_in_flight() {
            self.complete_retirement();
        }
    }

    fn on_ack(&mut self, ack: Ack) {
        if self.retiring.is_some() || self.stage != Stage::Starting {
            return;
        }
        match ack {
            Ack::Prepared => {
                let mut held = self.record.clone();
                held.phase = Phase::Held;
                if !self.write(held, "held") {
                    return self.abandon("held");
                }
                self.stage = Stage::Held;
                self.frame("held", Map::new());
                let _ = self.worker.send(WorkerCmd::ReadyPoint);
            }
            Ack::Failed { step, errno } => {
                super::daemon_warn!(
                    "pty daemon: the held child failed to prepare at {step}: {}",
                    std::io::Error::from_raw_os_error(errno)
                );
                self.retire(None, format!("producer:prepare-{step}"));
            }
            Ack::Lost => self.retire(None, "producer:child-lost".into()),
        }
    }

    /// Cancel, owner end of file, or a startup failure while `launched`
    /// (contract §7.6): no user command ever ran.
    ///
    /// The request is recorded first. Only once it is durable is the
    /// owner's cause acknowledged and any effect allowed; effects still wait
    /// for every committed ticket.
    fn retire(&mut self, cause: Option<Cause>, request_id: String) {
        if self.retiring.is_some() || self.stage == Stage::Done || self.is_transferred() {
            return;
        }
        // A transfer that has not committed loses to the retirement.
        if let Some(Pending::TransferCommit { request_id }) = self.pending.take() {
            self.reject(RejectCode::NotHeld, Some(request_id));
        }
        if let Some(request_id) = self.deferred_transfer.take() {
            self.reject(RejectCode::NotHeld, Some(request_id));
        }
        for ticket in &mut self.tickets {
            if ticket.state == TicketState::Issued {
                ticket.state = TicketState::Revoked;
            }
        }
        let mut requested = self.record.clone();
        requested.retirement = Some(Retirement {
            request_id: request_id.clone(),
            phase: RetirementPhase::Requested,
            evidence: json!({}),
        });
        if !self.write(requested, "retirement-requested") {
            return self.abandon("retirement-requested");
        }
        if let Some(cause) = cause {
            let decision = Decision::now(
                cause,
                (cause == Cause::Cancel).then(|| request_id.clone()),
                None,
            );
            self.frame("decision", extra(&[("decision", json!(decision))]));
        }
        self.retiring = Some(Retiring);
        // A committed effect finishes first; its completion resumes here.
        if !self.committed_in_flight() {
            self.complete_retirement();
        }
    }

    fn complete_retirement(&mut self) {
        if self.retiring.is_none() || self.stage == Stage::Done {
            return;
        }
        let mut evidence = Map::new();
        let mut verified = true;
        if let Some(child) = &self.child {
            // Armed while the held child still runs, so the wait is on this
            // child's exit, however soon the shut gate brings it.
            let watch = ExitWatch::arm(child.pid);
            held::shut(&child.gate);
            let outcome = verify_child_gone(child, &watch);
            evidence.insert("child".into(), json!({"pid": child.pid, "status": outcome}));
            verified &= outcome == "gone";
        }
        registry::release_file_lock_owned_by(&registry::lock_path(&self.runtime_id), self.me.pid);
        if self.published {
            let row = unwind_publication(&self.runtime_id, &self.generation, self.me.pid as i32);
            evidence.insert("row".into(), json!(row));
            verified &= matches!(row, "removed" | "absent" | "other-generation");
        }
        let request_id = self
            .record
            .retirement
            .as_ref()
            .map(|retirement| retirement.request_id.clone())
            .unwrap_or_default();
        let mut terminal = self.record.clone();
        terminal.writer = Writer::none();
        terminal.retirement = Some(Retirement {
            request_id,
            phase: if verified {
                RetirementPhase::Verified
            } else {
                RetirementPhase::Incomplete
            },
            evidence: Value::Object(evidence),
        });
        if !self.write(terminal, "retirement-terminal") {
            return self.abandon("retirement-terminal");
        }
        let retirement = json!(self.record.retirement);
        self.frame("retirement", extra(&[("retirement", retirement)]));
        self.finish(0);
    }

    /// A transfer the actor accepted at `ready` (contract §7.5).
    fn start_transfer(&mut self, request_id: String) {
        if !self.hold(
            "transfer-before-commit",
            Pending::TransferCommit {
                request_id: request_id.clone(),
            },
        ) {
            self.commit_transfer(request_id);
        }
    }

    /// Commit, then release.
    fn commit_transfer(&mut self, request_id: String) {
        if self.retiring.is_some() || self.stage == Stage::Done {
            return self.reject(RejectCode::NotHeld, Some(request_id));
        }
        let mut committed = self.record.clone();
        committed.state = State::Transferred;
        committed.transfer = Some(Transfer {
            request_id: request_id.clone(),
            committed_at: registry::now_iso8601(),
            released: None,
        });
        if !self.write(committed, "transfer-commit") {
            self.reject(RejectCode::NotHeld, Some(request_id));
            return self.abandon("transfer-commit");
        }
        self.transferred.store(true, Ordering::SeqCst);
        let _ = self.msgs.send(Msg::OperationTransferred);
        if !self.hold(
            "transfer-committed-before-release",
            Pending::TransferRelease,
        ) {
            self.release_transfer();
        }
    }

    fn release_transfer(&mut self) {
        let released = self
            .child
            .take()
            .is_some_and(|child| held::release(&child.gate));
        if !self.hold("released-before-ack", Pending::TransferAck { released }) {
            self.finish_transfer(released);
        }
    }

    fn finish_transfer(&mut self, released: bool) {
        let generation = self.generation.clone();
        let marked = registry::mutate_metadata_under_lock_with_wait(
            &self.runtime_id,
            |metadata| {
                let Some(marker) = metadata.extra.get_mut(MARKER_FIELD) else {
                    return false;
                };
                marker["phase"] = json!("transferred");
                true
            },
            &MutateOptions {
                expected_generation: Some(generation),
                expected_metadata: None,
            },
            |_| {},
            Duration::from_secs(5),
        );
        if !matches!(
            marked,
            MutateStatus::Changed(_) | MutateStatus::Unchanged(_)
        ) {
            super::daemon_warn!(
                "pty daemon: the transferred marker was not written ({marked:?}); the record is authoritative"
            );
        }
        let mut done = self.record.clone();
        done.writer = Writer::none();
        if let Some(transfer) = done.transfer.as_mut() {
            transfer.released = Some(released);
        }
        // The commit is durable either way; if this second write fails, the
        // frame reports the record as it stands (`released: null`).
        self.write(done, "transfer-released");
        let transfer = json!(self.record.transfer);
        self.frame("transferred", extra(&[("transfer", transfer)]));
        self.io.close();
        self.lock = None;
        self.stage = Stage::Done;
    }
}

/// After the gate is shut, the child must be gone by its exact identity. A
/// held or preparing child leaves at once on end of file; one that does not
/// (it is stopped, say) is killed through the substrate, whose signals are
/// fenced against reaping, so its pid cannot name anyone else. Both waits
/// end on the child's exit (a pidfd, a kqueue), not on a timer; the timer
/// only bounds them. `watch` was armed before the gate shut, so the exit it
/// observes is the child's (see [`ExitWatch`]); then the zombie must show,
/// read again for at most [`AFTER_EXIT_CAP`]. A child still exiting after
/// that is reported as such, never gone. A held child never forks, so there
/// is no group to signal.
fn verify_child_gone(child: &ChildRecord, watch: &ExitWatch) -> &'static str {
    let Some(id) = &child.id else {
        return "unrecorded";
    };
    let boot = pty_core::boot::read_boot_identity();
    let after_exit = || match id.liveness_after_exit(boot.as_deref(), AFTER_EXIT_CAP) {
        Liveness::Gone => "gone",
        Liveness::Live => "live",
        Liveness::Unknown(reason) => reason,
    };
    let gone = || id.liveness(&ProcTable::read(), boot.as_deref()) == Liveness::Gone;
    let _ = watch.wait(Duration::from_secs(1));
    if watch.observed() {
        return after_exit();
    }
    if gone() {
        return "gone";
    }
    let _ = child.session.kill();
    let waited = watch.wait(Duration::from_secs(5));
    if watch.observed() {
        after_exit()
    } else if gone() {
        "gone"
    } else if waited == Exit::Unsupported {
        "exit-wait-unavailable"
    } else {
        "live"
    }
}

/// Remove only this generation's row, fenced under both session locks.
fn unwind_publication(name: &str, generation: &str, pid: i32) -> &'static str {
    let owner = registry::SessionGenerationOwner {
        generation: generation.to_string(),
        pid,
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let outcome = registry::with_both_locks(name, || {
            let metadata = registry::read_metadata(name);
            let present = metadata.is_some()
                || registry::socket_path(name).exists()
                || registry::pid_path(name).exists();
            if !present {
                return "absent";
            }
            if !registry::is_current_generation_owner(name, &owner)
                || metadata
                    .as_ref()
                    .is_some_and(|m| m.generation.as_deref() != Some(generation))
            {
                return "other-generation";
            }
            registry::cleanup_all_while_locked(name);
            "removed"
        });
        match outcome {
            Ok(outcome) => return outcome,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => return "locks-busy",
        }
    }
}
