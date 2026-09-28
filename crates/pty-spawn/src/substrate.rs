//! Typed two-plane PTY substrate primitives.
//!
//! [`SessionRef`] is shareable identity only. A [`SessionOwner`] is the sole
//! reader of a generation's PTY master and the sole reaper of its child. Each
//! [`SessionClient`] is an equal attachment: it can submit the same typed data
//! and lifecycle operations, but it never receives the master or child handle.
//! The actor serializes every operation and fences attachments by generation.
//!
//! Nothing the actor does can block on the child or on a client:
//!
//! - **Output is never dropped for being late.** Bytes the child wrote before
//!   it exited are forwarded until the PTY closes, however the reader and the
//!   reaper race, and the exit is published after them (see
//!   [`EXIT_OUTPUT_DRAIN`] for the one bound on that wait).
//! - **Every attachment queue is bounded.** A stream from
//!   [`SessionOwner::attach`] that falls [`DEFAULT_ATTACHMENT_CAPACITY`] bytes
//!   behind is detached with [`SessionError::Overflowed`]. The owner's
//!   pre-registered stream from [`external_owned_pair_attached`] is never
//!   detached; when it is full the owner stops reading the PTY until it drains,
//!   so the child waits the way it would on a real terminal and no output is
//!   lost.
//! - **Input is written by its own thread, without blocking.** A child that
//!   never reads stdin stalls only the input queued behind it, never
//!   termination, resizing, output or lifecycle queries, and the input is
//!   failed rather than left hanging once the child exits.
//! - **Signals are fenced against reaping.** The reaper waits for the child
//!   without collecting it, then collects it under the same lock every signal
//!   takes, so a signal can reach the child or its unreaped corpse and never a
//!   process that reused its pid.

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{Child, ExitStatus as PortableExitStatus, MasterPty, PtyPair, PtySize};

/// How many undelivered bytes one attachment may hold. An attached stream
/// that falls further behind than this is detached; the owner's own stream
/// holds the PTY reader back instead.
pub const DEFAULT_ATTACHMENT_CAPACITY: usize = 8 * 1024 * 1024;

/// The smallest capacity [`SessionOwner::attach_with_capacity`] accepts, so a
/// stream always has room for its first lifecycle event and one read.
pub const MIN_ATTACHMENT_CAPACITY: usize = 64 * 1024;

/// How long a reaped child's exit waits for the PTY to close before it is
/// published anyway. A descendant that inherited the terminal can hold it open
/// indefinitely; output that arrives after this is still forwarded, but after
/// the exit event rather than before it.
pub const EXIT_OUTPUT_DRAIN: Duration = Duration::from_millis(250);

/// How many input writes may wait behind one the child has not read yet.
const INPUT_QUEUE_DEPTH: usize = 64;
/// The longest a pending input write sleeps before retrying a full PTY.
const INPUT_RETRY_MAX: Duration = Duration::from_millis(20);
const READ_CHUNK: usize = 16 * 1024;
/// How many times the end of the master's output is re-read, a millisecond
/// apart, before it is believed. See [`read_master`].
const END_RETRIES: u32 = 10;

/// Immutable identity for one exact PTY generation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionRef {
    root: PathBuf,
    id: String,
    generation: String,
}

impl SessionRef {
    /// Creates an identity. The generation must be non-empty and opaque.
    pub fn new(root: impl Into<PathBuf>, id: impl Into<String>, generation: impl Into<String>) -> Self {
        Self { root: root.into(), id: id.into(), generation: generation.into() }
    }

    pub fn root(&self) -> &Path { &self.root }
    pub fn id(&self) -> &str { &self.id }
    pub fn generation(&self) -> &str { &self.generation }
    pub fn socket_path(&self) -> PathBuf { self.root.join(format!("{}.sock", self.id)) }
}

/// The only lifecycle outcomes a client can observe from the substrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl ExitStatus {
    fn from_portable(status: PortableExitStatus) -> Self {
        if status.success() {
            return Self { code: Some(0), signal: None };
        }
        let signal = status.signal().and_then(signal_number);
        Self {
            code: signal.is_none().then_some(status.exit_code() as i32),
            signal,
        }
    }

    fn from_wait_status(status: i32) -> Self {
        if libc::WIFSIGNALED(status) {
            Self { code: None, signal: Some(libc::WTERMSIG(status)) }
        } else if libc::WIFEXITED(status) {
            Self { code: Some(libc::WEXITSTATUS(status)), signal: None }
        } else {
            Self { code: None, signal: None }
        }
    }
}

fn signal_number(name: &str) -> Option<i32> {
    let name = name.strip_prefix("SIG").unwrap_or(name);
    Some(match name {
        "ABRT" | "Aborted" => libc::SIGABRT,
        "ALRM" | "Alarm clock" => libc::SIGALRM,
        "BUS" | "Bus error" => libc::SIGBUS,
        "CHLD" => libc::SIGCHLD,
        "CONT" => libc::SIGCONT,
        "FPE" | "Floating point exception" => libc::SIGFPE,
        "HUP" | "Hangup" => libc::SIGHUP,
        "ILL" | "Illegal instruction" => libc::SIGILL,
        "INT" | "Interrupt" => libc::SIGINT,
        "KILL" | "Killed" => libc::SIGKILL,
        "PIPE" | "Broken pipe" => libc::SIGPIPE,
        "QUIT" | "Quit" => libc::SIGQUIT,
        "SEGV" | "Segmentation fault" => libc::SIGSEGV,
        "STOP" | "Stopped" => libc::SIGSTOP,
        "TERM" | "Terminated" => libc::SIGTERM,
        "TRAP" | "Trace/breakpoint trap" => libc::SIGTRAP,
        "TSTP" | "Stopped (signal)" => libc::SIGTSTP,
        "TTIN" => libc::SIGTTIN,
        "TTOU" => libc::SIGTTOU,
        "USR1" | "User defined signal 1" => libc::SIGUSR1,
        "USR2" | "User defined signal 2" => libc::SIGUSR2,
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    Running,
    Exited(ExitStatus),
    /// The owner actor disappeared before an authoritative child result.
    OwnerLost,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// Child output. It can follow a lifecycle event when a descendant keeps
    /// the PTY open past the child's exit; it never follows `OutputClosed`.
    Data(Vec<u8>),
    /// The PTY master reached EOF. No more [`SessionEvent::Data`] follows.
    OutputClosed,
    Lifecycle(Lifecycle),
    Geometry(PtySize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    StaleGeneration { expected: String, actual: String },
    OwnerLost,
    Exited(ExitStatus),
    Closed,
    Io(String),
    /// This stream fell more than `capacity` bytes behind the session and was
    /// detached. Everything queued before that was delivered first; attach
    /// again to keep observing the session.
    Overflowed {
        capacity: usize,
    },
    /// Earlier input is still waiting for the child to read it and the input
    /// queue is full. Nothing from this request was written.
    InputBackpressure,
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleGeneration { expected, actual } => write!(f, "stale PTY generation (expected {expected}, current {actual})"),
            Self::OwnerLost => f.write_str("PTY owner was lost"),
            Self::Exited(status) => write!(f, "PTY child exited ({status:?})"),
            Self::Closed => f.write_str("PTY session is closed"),
            Self::Io(message) => f.write_str(message),
            Self::Overflowed { capacity } => write!(
                f,
                "PTY attachment fell more than {capacity} bytes behind and was detached"
            ),
            Self::InputBackpressure => {
                f.write_str("PTY input queue is full; the child is not reading its input")
            }
        }
    }
}

impl std::error::Error for SessionError {}

/// What one queued event costs against an attachment's capacity.
fn event_cost(event: &SessionEvent) -> usize {
    std::mem::size_of::<SessionEvent>()
        + match event {
            SessionEvent::Data(bytes) => bytes.len(),
            _ => 0,
        }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnFull {
    /// Detach the stream with [`SessionError::Overflowed`].
    Detach,
    /// Hold the PTY reader until the stream drains.
    HoldReader,
}

/// One attachment's bounded event queue, shared by the actor that fills it,
/// the stream that drains it, and (for the owner's stream) the PTY reader.
struct Queue {
    state: Mutex<QueueState>,
    changed: Condvar,
    capacity: usize,
    on_full: OnFull,
}

#[derive(Default)]
struct QueueState {
    events: VecDeque<SessionEvent>,
    /// The cost of `events`.
    queued: usize,
    /// Bytes the reader has read for this queue that the actor has not
    /// queued yet. Only a `HoldReader` queue has any.
    reserved: usize,
    overflowed: bool,
    sender_gone: bool,
    receiver_gone: bool,
}

impl Queue {
    fn new(capacity: usize, on_full: OnFull) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
            changed: Condvar::new(),
            capacity,
            on_full,
        })
    }

    fn lock(&self) -> MutexGuard<'_, QueueState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn wait<'a>(&self, state: MutexGuard<'a, QueueState>) -> MutexGuard<'a, QueueState> {
        self.changed
            .wait(state)
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Queue one event. `false` means this attachment is finished — its
    /// stream was dropped or it overflowed — and the actor forgets it.
    fn push(&self, event: SessionEvent) -> bool {
        let mut state = self.lock();
        if state.receiver_gone || state.overflowed {
            return false;
        }
        if let SessionEvent::Data(bytes) = &event {
            state.reserved = state.reserved.saturating_sub(bytes.len());
        }
        let cost = event_cost(&event);
        if self.on_full == OnFull::Detach && state.queued + cost > self.capacity {
            state.overflowed = true;
            self.changed.notify_all();
            return false;
        }
        state.queued += cost;
        state.events.push_back(event);
        self.changed.notify_all();
        true
    }

    /// The PTY reader's gate for a `HoldReader` queue: wait until the stream
    /// has room for another read, or nobody is left to fill or drain it.
    fn wait_for_room(&self) {
        let mut state = self.lock();
        while !state.receiver_gone
            && !state.sender_gone
            && state.queued + state.reserved >= self.capacity
        {
            state = self.wait(state);
        }
    }

    fn reserve(&self, bytes: usize) {
        self.lock().reserved += bytes;
    }

    fn pop(&self, deadline: Option<Instant>) -> Result<SessionEvent, SessionError> {
        let mut state = self.lock();
        loop {
            if let Some(event) = state.events.pop_front() {
                state.queued -= event_cost(&event);
                self.changed.notify_all();
                return Ok(event);
            }
            if state.overflowed {
                return Err(SessionError::Overflowed {
                    capacity: self.capacity,
                });
            }
            if state.sender_gone {
                return Err(SessionError::Closed);
            }
            state = match deadline {
                None => self.wait(state),
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(SessionError::Io("attachment event timed out".into()));
                    }
                    self.changed
                        .wait_timeout(state, deadline - now)
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .0
                }
            };
        }
    }
}

/// The actor's end of one attachment. Dropping it closes the stream once the
/// stream has drained what was queued.
struct Attachment(Arc<Queue>);

impl Drop for Attachment {
    fn drop(&mut self) {
        self.0.lock().sender_gone = true;
        self.0.changed.notify_all();
    }
}

/// The read side of one attachment. It contains no PTY descriptor and cannot
/// become a second master reader.
pub struct AttachStream {
    queue: Arc<Queue>,
}

impl AttachStream {
    pub fn recv(&self) -> Result<SessionEvent, SessionError> {
        self.queue.pop(None)
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<SessionEvent, SessionError> {
        self.queue.pop(Some(Instant::now() + timeout))
    }

    /// How many bytes this stream may fall behind before the owner acts.
    pub fn capacity(&self) -> usize {
        self.queue.capacity
    }
}

impl Drop for AttachStream {
    fn drop(&mut self) {
        let mut state = self.queue.lock();
        state.receiver_gone = true;
        state.events.clear();
        state.queued = 0;
        drop(state);
        self.queue.changed.notify_all();
    }
}

/// A per-client attachment. Cloning it does not create lifecycle authority or
/// another reader; all commands still pass through the owner actor.
#[derive(Clone)]
pub struct SessionClient {
    session: SessionRef,
    tx: Sender<Command>,
}

impl SessionClient {
    pub fn session(&self) -> &SessionRef { &self.session }

    /// Write `bytes` to the child's input. Returns once they are written, or
    /// with the reason they never will be: the child exited or the owner was
    /// lost while they waited, or [`SessionError::InputBackpressure`] when
    /// too much earlier input is still unread. Waiting here never delays any
    /// other operation on the session.
    pub fn input(&self, bytes: impl Into<Vec<u8>>) -> Result<(), SessionError> {
        self.request(CommandKind::Input(bytes.into()))
    }

    pub fn resize(&self, size: PtySize) -> Result<(), SessionError> {
        self.request(CommandKind::Resize(size))
    }

    pub fn terminate(&self) -> Result<(), SessionError> {
        self.request(CommandKind::Signal(libc::SIGTERM))
    }

    /// Ask the owner actor to send the daemon's traditional hangup.
    pub fn hangup(&self) -> Result<(), SessionError> {
        self.request(CommandKind::Signal(libc::SIGHUP))
    }

    /// SIGKILL the child. A no-op once it has been reaped.
    pub fn kill(&self) -> Result<(), SessionError> {
        self.request(CommandKind::Signal(libc::SIGKILL))
    }

    /// Send any signal to the child, fenced against its reaping like every
    /// substrate signal: once the child is collected nothing is sent.
    pub fn signal(&self, signal: i32) -> Result<(), SessionError> {
        self.request(CommandKind::Signal(signal))
    }

    /// Signal the process group the child leads. Like every substrate signal
    /// it is sent only while the child is unreaped, which is what keeps the
    /// group id from naming anyone else.
    pub fn signal_process_group(&self, signal: i32) -> Result<(), SessionError> {
        self.request(CommandKind::SignalGroup(signal))
    }

    pub fn lifecycle(&self) -> Result<Lifecycle, SessionError> {
        let (reply, rx) = mpsc::channel();
        self.tx.send(Command::Query { session: self.session.clone(), reply }).map_err(|_| SessionError::Closed)?;
        rx.recv().map_err(|_| SessionError::Closed)?
    }

    fn request(&self, kind: CommandKind) -> Result<(), SessionError> {
        let (reply, rx) = reply();
        self.tx.send(Command::Request { session: self.session.clone(), kind, reply }).map_err(|_| SessionError::Closed)?;
        rx.recv().map_err(|_| SessionError::Closed)?
    }
}

/// The sole owner/serializer for one externally-owned PTY generation.
pub struct SessionOwner {
    inner: Arc<OwnerInner>,
}

impl Clone for SessionOwner {
    fn clone(&self) -> Self { Self { inner: Arc::clone(&self.inner) } }
}

struct OwnerInner {
    tx: Sender<Command>,
}

impl Drop for OwnerInner {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::OwnerLost);
    }
}

/// A PTY master and child already created by an external consumer. Ownership
/// moves into the substrate; no raw descriptor is exposed or duplicated.
pub fn external_owned(
    session: SessionRef,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
) -> io::Result<SessionOwner> {
    start_owner(session, master, child, None)
}

/// Convenience constructor for callers that already opened a pair and spawned
/// its child on the slave. Dropping the slave here is part of the ownership
/// transfer, ensuring this owner is the only component that reads the master.
pub fn external_owned_pair(
    session: SessionRef,
    pair: PtyPair,
    child: Box<dyn Child + Send + Sync>,
) -> io::Result<SessionOwner> {
    drop(pair.slave);
    start_owner(session, pair.master, child, None)
}

/// Transfer a pair and atomically pre-register its first equal attachment.
/// Output produced immediately after spawn is queued for this stream rather
/// than falling into the gap between owner startup and a later attach request.
///
/// This stream is the owner's: it is never detached for falling behind.
/// Instead, once [`DEFAULT_ATTACHMENT_CAPACITY`] bytes are waiting in it, the
/// PTY is not read again until it drains.
pub fn external_owned_pair_attached(
    session: SessionRef,
    pair: PtyPair,
    child: Box<dyn Child + Send + Sync>,
) -> io::Result<(SessionOwner, SessionClient, AttachStream)> {
    external_owned_pair_attached_with_capacity(session, pair, child, DEFAULT_ATTACHMENT_CAPACITY)
}

/// [`external_owned_pair_attached`] with an explicit bound on the owner's
/// stream.
pub fn external_owned_pair_attached_with_capacity(
    session: SessionRef,
    pair: PtyPair,
    child: Box<dyn Child + Send + Sync>,
    capacity: usize,
) -> io::Result<(SessionOwner, SessionClient, AttachStream)> {
    drop(pair.slave);
    let queue = Queue::new(capacity.max(MIN_ATTACHMENT_CAPACITY), OnFull::HoldReader);
    let owner = start_owner(
        session.clone(),
        pair.master,
        child,
        Some(Arc::clone(&queue)),
    )?;
    let client = SessionClient {
        session,
        tx: owner.inner.tx.clone(),
    };
    Ok((owner, client, AttachStream { queue }))
}

fn start_owner(
    session: SessionRef,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    owner_stream: Option<Arc<Queue>>,
) -> io::Result<SessionOwner> {
    let reader = master.try_clone_reader().map_err(io::Error::other)?;
    let writer = master.take_writer().map_err(io::Error::other)?;
    let input_fd = duplicate_master(master.as_ref())?;
    let signals = Arc::new(ChildSignals::new(child.process_id()));
    let (tx, rx) = mpsc::channel();
    let (input, jobs) = mpsc::sync_channel(INPUT_QUEUE_DEPTH);
    let input_gate = Arc::new(InputGate::default());
    let writer_gate = Arc::clone(&input_gate);
    thread::Builder::new()
        .name("pty-substrate-writer".into())
        .spawn(move || write_input(input_fd, writer, jobs, writer_gate))
        .map_err(io::Error::other)?;
    let reader_tx = tx.clone();
    let reader_flow = owner_stream.clone();
    thread::Builder::new()
        .name("pty-substrate-reader".into())
        .spawn(move || read_master(reader, reader_tx, reader_flow))
        .map_err(io::Error::other)?;
    let reaper_tx = tx.clone();
    let reaper_signals = Arc::clone(&signals);
    thread::Builder::new()
        .name("pty-substrate-reaper".into())
        .spawn(move || reap_child(child, reaper_signals, reaper_tx))
        .map_err(io::Error::other)?;
    let actor = Actor {
        session,
        master,
        signals,
        input,
        input_gate,
        attachments: owner_stream.map(Attachment).into_iter().collect(),
        lifecycle: Lifecycle::Running,
        published: Lifecycle::Running,
        publish_exit_at: None,
        output_closed: false,
    };
    thread::Builder::new()
        .name("pty-substrate-owner".into())
        .spawn(move || actor.run(rx))
        .map_err(io::Error::other)?;
    Ok(SessionOwner {
        inner: Arc::new(OwnerInner { tx }),
    })
}

/// A descriptor for the same open master, so the input thread can switch it
/// to non-blocking around each write. It is never read from.
fn duplicate_master(master: &(dyn MasterPty + Send)) -> io::Result<OwnedFd> {
    let fd = master
        .as_raw_fd()
        .ok_or_else(|| io::Error::other("PTY master has no descriptor"))?;
    // SAFETY: fcntl(F_DUPFD_CLOEXEC) on a descriptor the master owns; the
    // result is a new descriptor that nothing else owns.
    let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `dup` was just returned by fcntl and is owned by nobody else.
    Ok(unsafe { OwnedFd::from_raw_fd(dup) })
}

fn reply() -> (Sender<Result<(), SessionError>>, Receiver<Result<(), SessionError>>) { mpsc::channel() }

enum Command {
    Request {
        session: SessionRef,
        kind: CommandKind,
        reply: Sender<Result<(), SessionError>>,
    },
    Query {
        session: SessionRef,
        reply: Sender<Result<Lifecycle, SessionError>>,
    },
    Attach {
        session: SessionRef,
        attachment: Attachment,
        reply: Sender<Result<(), SessionError>>,
    },
    Output(Vec<u8>),
    Reaped(ExitStatus),
    ReaderClosed,
    OwnerLost,
}

enum CommandKind {
    Input(Vec<u8>),
    Resize(PtySize),
    Signal(libc::c_int),
    SignalGroup(libc::c_int),
}

impl SessionOwner {
    /// Attach an equal client and its independent event stream, detached with
    /// [`SessionError::Overflowed`] if it falls [`DEFAULT_ATTACHMENT_CAPACITY`]
    /// bytes behind.
    pub fn attach(&self, session: &SessionRef) -> Result<(SessionClient, AttachStream), SessionError> {
        self.attach_with_capacity(session, DEFAULT_ATTACHMENT_CAPACITY)
    }

    /// [`SessionOwner::attach`] with an explicit bound, raised to at least
    /// [`MIN_ATTACHMENT_CAPACITY`].
    pub fn attach_with_capacity(
        &self,
        session: &SessionRef,
        capacity: usize,
    ) -> Result<(SessionClient, AttachStream), SessionError> {
        let queue = Queue::new(capacity.max(MIN_ATTACHMENT_CAPACITY), OnFull::Detach);
        let (reply, result) = reply();
        let attachment = Attachment(Arc::clone(&queue));
        self.inner
            .tx
            .send(Command::Attach {
                session: session.clone(),
                attachment,
                reply,
            })
            .map_err(|_| SessionError::Closed)?;
        result.recv().map_err(|_| SessionError::Closed)??;
        Ok((
            SessionClient {
                session: session.clone(),
                tx: self.inner.tx.clone(),
            },
            AttachStream { queue },
        ))
    }

    /// Explicitly revoke this generation without granting authority to a client.
    pub fn lose_ownership(&self) { let _ = self.inner.tx.send(Command::OwnerLost); }
}

fn check(session: &SessionRef, current: &SessionRef) -> Result<(), SessionError> {
    if session != current {
        return Err(SessionError::StaleGeneration { expected: session.generation.clone(), actual: current.generation.clone() });
    }
    Ok(())
}

struct Actor {
    session: SessionRef,
    master: Box<dyn MasterPty + Send>,
    signals: Arc<ChildSignals>,
    input: SyncSender<InputJob>,
    input_gate: Arc<InputGate>,
    attachments: Vec<Attachment>,
    /// The authoritative lifecycle: what requests and queries see.
    lifecycle: Lifecycle,
    /// The lifecycle attachments have been told. It trails `lifecycle` only
    /// while a reaped child's output is still draining.
    published: Lifecycle,
    publish_exit_at: Option<Instant>,
    output_closed: bool,
}

impl Actor {
    fn run(mut self, rx: Receiver<Command>) {
        self.broadcast(SessionEvent::Lifecycle(self.published));
        loop {
            let command = match self.publish_exit_at {
                Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                    Ok(command) => command,
                    Err(RecvTimeoutError::Timeout) => {
                        self.publish_lifecycle();
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                },
                None => match rx.recv() {
                    Ok(command) => command,
                    Err(_) => break,
                },
            };
            self.handle(command);
        }
        self.publish_lifecycle();
        self.input_gate.close(SessionError::Closed);
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Attach {
                session: requested,
                attachment,
                reply,
            } => {
                let result = check(&requested, &self.session).map(|_| {
                    let mut live = attachment.0.push(SessionEvent::Lifecycle(self.published));
                    if self.output_closed {
                        live = live && attachment.0.push(SessionEvent::OutputClosed);
                    }
                    if live {
                        self.attachments.push(attachment);
                    }
                });
                let _ = reply.send(result);
            }
            Command::Request { session: requested, kind, reply } => {
                if let Err(error) = check(&requested, &self.session).and_then(|_| self.running()) {
                    let _ = reply.send(Err(error));
                    return;
                }
                let io = |e: io::Error| SessionError::Io(e.to_string());
                let result = match kind {
                    CommandKind::Input(bytes) => return self.submit_input(bytes, reply),
                    CommandKind::Resize(size) => self
                        .master
                        .resize(size)
                        .map_err(|e| SessionError::Io(e.to_string())),
                    CommandKind::Signal(signal) => self.signals.signal(signal).map_err(io),
                    CommandKind::SignalGroup(signal) => {
                        self.signals.signal_group(signal).map_err(io)
                    }
                };
                let _ = reply.send(result);
            }
            Command::Query { session: requested, reply } => {
                let _ = reply.send(check(&requested, &self.session).map(|_| self.lifecycle));
            }
            // Forwarded in every lifecycle state: bytes the child wrote before
            // it was reaped are still its output. Only the PTY closing ends it.
            Command::Output(bytes) => {
                if !self.output_closed {
                    self.broadcast(SessionEvent::Data(bytes));
                }
            }
            Command::Reaped(status) => {
                if matches!(self.lifecycle, Lifecycle::Running) {
                    self.lifecycle = Lifecycle::Exited(status);
                    self.input_gate.close(SessionError::Exited(status));
                    if self.output_closed {
                        self.publish_lifecycle();
                    } else {
                        self.publish_exit_at = Some(Instant::now() + EXIT_OUTPUT_DRAIN);
                    }
                }
            }
            Command::ReaderClosed => {
                self.output_closed = true;
                self.broadcast(SessionEvent::OutputClosed);
                self.publish_lifecycle();
            }
            Command::OwnerLost => {
                if matches!(self.lifecycle, Lifecycle::Running) {
                    self.lifecycle = Lifecycle::OwnerLost;
                    self.input_gate.close(SessionError::OwnerLost);
                    self.publish_lifecycle();
                    let _ = self.signals.signal(libc::SIGTERM);
                }
            }
        }
    }

    fn running(&self) -> Result<(), SessionError> {
        match self.lifecycle {
            Lifecycle::Running => Ok(()),
            Lifecycle::Exited(status) => Err(SessionError::Exited(status)),
            Lifecycle::OwnerLost => Err(SessionError::OwnerLost),
        }
    }

    /// Hand input to the writer thread. The writer replies; the actor never
    /// waits for the child to read.
    fn submit_input(&self, bytes: Vec<u8>, reply: Sender<Result<(), SessionError>>) {
        match self.input.try_send(InputJob { bytes, reply }) {
            Ok(()) => {}
            Err(TrySendError::Full(job)) => {
                let _ = job.reply.send(Err(SessionError::InputBackpressure));
            }
            Err(TrySendError::Disconnected(job)) => {
                let _ = job.reply.send(Err(SessionError::Closed));
            }
        }
    }

    fn publish_lifecycle(&mut self) {
        self.publish_exit_at = None;
        if self.published != self.lifecycle {
            self.published = self.lifecycle;
            self.broadcast(SessionEvent::Lifecycle(self.published));
        }
    }

    fn broadcast(&mut self, event: SessionEvent) {
        self.attachments
            .retain(|attachment| attachment.0.push(event.clone()));
    }
}

fn read_master(
    mut reader: Box<dyn Read + Send>,
    tx: Sender<Command>,
    owner_stream: Option<Arc<Queue>>,
) {
    let mut buf = vec![0_u8; READ_CHUNK];
    let mut end_retries = 0;
    loop {
        if let Some(stream) = &owner_stream {
            stream.wait_for_room();
        }
        match reader.read(&mut buf) {
            // The end of the output, if it repeats. Linux reports a closed
            // slave as EIO (portable-pty's reader turns that into `Ok(0)`),
            // but under load it can say so before the child's last write has
            // reached the master: a read a millisecond later still returns
            // those bytes. Measured 2026-09-23 on Linux with every CPU busy: a
            // bare portable-pty reader lost the tail in 4 of 1500 rounds, and
            // none with this retry, which got the bytes about 1 ms after the
            // first end. So only an end that repeats, with nothing read in
            // between, is believed.
            Ok(0) if end_retries < END_RETRIES => {
                end_retries += 1;
                thread::sleep(Duration::from_millis(1));
            }
            Ok(0) => break,
            Ok(n) => {
                end_retries = 0;
                if let Some(stream) = &owner_stream {
                    stream.reserve(n);
                }
                if tx.send(Command::Output(buf[..n].to_vec())).is_err() {
                    return;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            // The input thread makes the shared master non-blocking for the
            // length of one write; a read that lands inside it just retries.
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1))
            }
            // The same early end, from a reader that reports EIO itself.
            Err(error) if error.raw_os_error() == Some(libc::EIO) && end_retries < END_RETRIES => {
                end_retries += 1;
                thread::sleep(Duration::from_millis(1));
            }
            Err(_) => break,
        }
    }
    let _ = tx.send(Command::ReaderClosed);
}

/// The only way the substrate signals its child. See the module docs: the
/// reaper collects the child while holding `reaped`, so a signal sent while
/// holding it and seeing `false` can only reach the child itself.
struct ChildSignals {
    pid: Option<u32>,
    reaped: Mutex<bool>,
    /// `kill(2)`. A seam, so a test can see exactly what was sent.
    kill: fn(libc::pid_t, libc::c_int) -> libc::c_int,
}

fn os_kill(target: libc::pid_t, signal: libc::c_int) -> libc::c_int {
    // SAFETY: kill(2) has no memory-safety preconditions. Which process it
    // may target is `ChildSignals::send`'s job.
    unsafe { libc::kill(target, signal) }
}

impl ChildSignals {
    fn new(pid: Option<u32>) -> Self {
        Self {
            pid,
            reaped: Mutex::new(false),
            kill: os_kill,
        }
    }

    fn lock(&self) -> MutexGuard<'_, bool> {
        self.reaped
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn signal(&self, signal: libc::c_int) -> io::Result<()> {
        self.send(signal, false)
    }

    /// The child leads the process group a pty spawn gives it (`setsid`), so
    /// its pid is the group id. If the child made no group of its own there
    /// is nothing by that id to reach.
    fn signal_group(&self, signal: libc::c_int) -> io::Result<()> {
        self.send(signal, true)
    }

    fn send(&self, signal: libc::c_int, group: bool) -> io::Result<()> {
        let Some(pid) = self.pid else { return Ok(()) };
        let reaped = self.lock();
        if *reaped {
            return Ok(());
        }
        let target = if group {
            -(pid as libc::pid_t)
        } else {
            pid as libc::pid_t
        };
        // The child is unreaped while `reaped` is held and false, so its pid,
        // and a group id equal to it, cannot have been reused.
        let result = (self.kill)(target, signal);
        drop(reaped);
        if result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

fn reap_child(
    mut child: Box<dyn Child + Send + Sync>,
    signals: Arc<ChildSignals>,
    tx: Sender<Command>,
) {
    let status = match child.process_id() {
        Some(pid) => {
            wait_until_exited(pid);
            collect_fenced(pid, &signals)
        }
        None => {
            let status = child.wait().ok().map(ExitStatus::from_portable);
            *signals.lock() = true;
            status
        }
    };
    let _ = tx.send(Command::Reaped(status.unwrap_or(ExitStatus {
        code: None,
        signal: None,
    })));
}

/// Block until the child has exited, leaving it unreaped (`WNOWAIT`): its pid
/// stays reserved until [`collect`] runs under the signal lock.
///
/// **Only an exit ends the wait.** macOS's `waitid` answers `WEXITED |
/// WNOWAIT` for a *stopped* child too, with `si_code` `CLD_STOPPED`, and
/// `WNOWAIT` leaves that report in place, so it answers the same way every
/// time. Linux keeps waiting. Measured on macOS 27 on 2026-09-28. Returning on
/// the stop sent the reaper into a blocking `waitpid` under the signal lock
/// while the child was only stopped, and the SIGKILL or SIGCONT that would
/// have ended the stop waited on that lock forever. A startup deadline
/// freezes its child with SIGSTOP, so on a Mac the daemon hung right there.
///
/// A stop, continue or trap report is consumed instead, with `WSTOPPED |
/// WCONTINUED` and no `WEXITED`, which can never reap. Then the wait
/// resumes, and it blocks again on both platforms.
fn wait_until_exited(pid: u32) {
    loop {
        // SAFETY: an all-zero siginfo_t is a valid value to be overwritten.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waitid on our own child; `info` is writable storage.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result != 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return;
        }
        if !matches!(
            info.si_code,
            libc::CLD_STOPPED | libc::CLD_CONTINUED | libc::CLD_TRAPPED
        ) {
            return;
        }
        // SAFETY: as above. No WEXITED, so this cannot collect the child.
        let mut consumed: libc::siginfo_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut consumed,
                libc::WSTOPPED | libc::WCONTINUED | libc::WNOHANG,
            );
        }
    }
}

/// Collect an exited child under the signal lock, so no signal is in flight
/// to its pid when the pid is released.
fn collect_fenced(pid: u32, signals: &ChildSignals) -> Option<ExitStatus> {
    let mut reaped = signals.lock();
    let status = collect(pid);
    *reaped = true;
    status
}

fn collect(pid: u32) -> Option<ExitStatus> {
    let mut raw = 0;
    loop {
        // SAFETY: pid came from the child owned by this substrate actor.
        let result = unsafe { libc::waitpid(pid as libc::pid_t, &mut raw, 0) };
        if result == pid as libc::pid_t {
            return Some(ExitStatus::from_wait_status(raw));
        }
        if result < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return None;
    }
}

struct InputJob {
    bytes: Vec<u8>,
    reply: Sender<Result<(), SessionError>>,
}

/// Why queued input can no longer be delivered, once it cannot.
#[derive(Default)]
struct InputGate {
    closed: Mutex<Option<SessionError>>,
    changed: Condvar,
}

impl InputGate {
    fn lock(&self) -> MutexGuard<'_, Option<SessionError>> {
        self.closed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn close(&self, reason: SessionError) {
        self.lock().get_or_insert(reason);
        self.changed.notify_all();
    }

    fn reason(&self) -> Option<SessionError> {
        self.lock().clone()
    }

    /// Sleep for up to `timeout`, returning early with the reason if input
    /// closes meanwhile.
    fn wait(&self, timeout: Duration) -> Option<SessionError> {
        let closed = self.lock();
        if closed.is_some() {
            return closed.clone();
        }
        self.changed
            .wait_timeout(closed, timeout)
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0
            .clone()
    }
}

fn write_input(
    fd: OwnedFd,
    mut writer: Box<dyn Write + Send>,
    jobs: Receiver<InputJob>,
    gate: Arc<InputGate>,
) {
    for job in jobs {
        let result = match gate.reason() {
            Some(reason) => Err(reason),
            None => write_without_blocking(fd.as_raw_fd(), writer.as_mut(), &job.bytes, &gate),
        };
        let _ = job.reply.send(result);
    }
    // portable-pty's writer sends a final newline and EOF when dropped. That
    // write must not hang on a child that stopped reading either.
    let _ = with_nonblocking(fd.as_raw_fd(), || drop(writer));
}

/// Write all of `bytes`, never parking in the kernel: a full PTY is retried
/// with a short backoff until the child reads or the gate closes.
fn write_without_blocking(
    fd: RawFd,
    writer: &mut (dyn Write + Send),
    mut bytes: &[u8],
    gate: &InputGate,
) -> Result<(), SessionError> {
    let mut backoff = Duration::from_millis(1);
    while !bytes.is_empty() {
        let written = with_nonblocking(fd, || writer.write(bytes)).and_then(|result| result);
        match written {
            Ok(0) => return Err(SessionError::Io("PTY input closed".into())),
            Ok(n) => {
                bytes = &bytes[n..];
                backoff = Duration::from_millis(1);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if let Some(reason) = gate.wait(backoff) {
                    return Err(reason);
                }
                backoff = (backoff * 2).min(INPUT_RETRY_MAX);
            }
            Err(error) => return Err(SessionError::Io(error.to_string())),
        }
    }
    Ok(())
}

/// Run `f` with the master's open file description non-blocking, then put
/// the flags back. The reader shares the description and tolerates the
/// window (see [`read_master`]).
fn with_nonblocking<T>(fd: RawFd, f: impl FnOnce() -> T) -> io::Result<T> {
    // SAFETY: F_GETFL/F_SETFL on a descriptor this thread owns.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let result = f();
    // SAFETY: as above; restores exactly the flags read before.
    unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{open, shell_exec};
    use std::time::Duration;

    fn owner(id: &str, command: &str) -> (SessionRef, SessionOwner) {
        let pair = open(24, 80).unwrap();
        let child = pair.slave.spawn_command(shell_exec(command, &[])).unwrap();
        let session = SessionRef::new("/private", id, "generation-a");
        (session.clone(), external_owned_pair(session, pair, child).unwrap())
    }

    #[test]
    fn pre_registered_attachment_keeps_immediate_output() {
        let pair = open(24, 80).unwrap();
        let args = ["-c", "printf immediate; exec cat"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        let child = pair.slave.spawn_command(shell_exec("sh", &args)).unwrap();
        let session = SessionRef::new("/private", "initial", "generation-a");
        let (owner, client, stream) = external_owned_pair_attached(session, pair, child).unwrap();
        assert_eq!(
            stream.recv_timeout(Duration::from_secs(2)).unwrap(),
            SessionEvent::Lifecycle(Lifecycle::Running)
        );
        assert!(matches!(
            stream.recv_timeout(Duration::from_secs(2)).unwrap(),
            SessionEvent::Data(bytes) if bytes.windows(b"immediate".len()).any(|window| window == b"immediate")
        ));
        client.terminate().unwrap();
        drop(owner);
    }
    #[test]
    fn equal_clients_receive_the_same_typed_output() {
        let (session, owner) = owner("equal", "cat");
        let (a, a_stream) = owner.attach(&session).unwrap();
        let (b, b_stream) = owner.attach(&session).unwrap();
        assert_eq!(a.session(), b.session());
        assert_eq!(a_stream.recv_timeout(Duration::from_secs(2)).unwrap(), SessionEvent::Lifecycle(Lifecycle::Running));
        assert_eq!(b_stream.recv_timeout(Duration::from_secs(2)).unwrap(), SessionEvent::Lifecycle(Lifecycle::Running));
        a.input(b"equal\n".to_vec()).unwrap();
        let a_event = a_stream.recv_timeout(Duration::from_secs(2)).unwrap();
        let b_event = b_stream.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(a_event, b_event);
        assert!(matches!(a_event, SessionEvent::Data(bytes) if !bytes.is_empty()));
    }

    #[test]
    fn one_reaper_publishes_output_close_and_child_exit() {
        // Pre-registered: `true` can exit before a later `attach` is served,
        // and then the stream would start at the exit instead of `Running`.
        let (_owner, _client, stream) =
            sh_attached("reaper", "exec true", DEFAULT_ATTACHMENT_CAPACITY);
        assert_eq!(
            stream.recv_timeout(Duration::from_secs(2)).unwrap(),
            SessionEvent::Lifecycle(Lifecycle::Running)
        );
        let first = stream.recv_timeout(Duration::from_secs(2)).unwrap();
        let second = stream.recv_timeout(Duration::from_secs(2)).unwrap();
        let events = [first, second];
        assert!(events.contains(&SessionEvent::OutputClosed));
        assert!(events.contains(&SessionEvent::Lifecycle(Lifecycle::Exited(ExitStatus {
            code: Some(0),
            signal: None,
        }))));
    }

    #[test]
    fn generation_is_fenced_before_attach() {
        let (session, owner) = owner("fence", "true");
        let stale = SessionRef::new("/private", "fence", "generation-b");
        assert!(matches!(owner.attach(&stale), Err(SessionError::StaleGeneration { .. })));
        let _ = owner.attach(&session).unwrap();
    }

    #[test]
    fn owner_loss_invalidates_all_clients() {
        // `sleep` needs an operand: bare, it prints its usage and exits, and
        // that output and exit race the revocation this test is about.
        let (session, owner) = sh("loss", "exec sleep 30");
        let (client, stream) = owner.attach(&session).unwrap();
        owner.lose_ownership();
        assert_eq!(stream.recv_timeout(Duration::from_secs(2)).unwrap(), SessionEvent::Lifecycle(Lifecycle::Running));
        assert_eq!(stream.recv_timeout(Duration::from_secs(2)).unwrap(), SessionEvent::Lifecycle(Lifecycle::OwnerLost));
        assert_eq!(client.input(Vec::new()), Err(SessionError::OwnerLost));
    }

    #[test]
    fn a_new_generation_does_not_adopt_an_old_owner() {
        let (old, owner) = owner("restart", "sleep");
        let stale = SessionRef::new("/private", "restart", "generation-b");
        assert!(matches!(owner.attach(&stale), Err(SessionError::StaleGeneration { .. })));
        drop(owner);
        assert_eq!(old.generation(), "generation-a");
    }

    // --- Regression coverage for the PR #31 and #32 review findings. ---

    const T: Duration = Duration::from_secs(5);

    fn sh(id: &str, script: &str) -> (SessionRef, SessionOwner) {
        let pair = open(24, 80).unwrap();
        let args = vec!["-c".to_string(), script.to_string()];
        let child = pair.slave.spawn_command(shell_exec("sh", &args)).unwrap();
        let session = SessionRef::new("/private", id, "generation-a");
        (
            session.clone(),
            external_owned_pair(session, pair, child).unwrap(),
        )
    }

    fn sh_attached(
        id: &str,
        script: &str,
        capacity: usize,
    ) -> (SessionOwner, SessionClient, AttachStream) {
        let pair = open(24, 80).unwrap();
        let args = vec!["-c".to_string(), script.to_string()];
        let child = pair.slave.spawn_command(shell_exec("sh", &args)).unwrap();
        let session = SessionRef::new("/private", id, "generation-a");
        external_owned_pair_attached_with_capacity(session, pair, child, capacity).unwrap()
    }

    /// Every event up to and including both `OutputClosed` and an exit.
    fn until_closed_and_exited(stream: &AttachStream) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        let (mut closed, mut exited) = (false, false);
        while !(closed && exited) {
            let event = stream.recv_timeout(T).expect("the session never finished");
            closed |= event == SessionEvent::OutputClosed;
            exited |= matches!(event, SessionEvent::Lifecycle(Lifecycle::Exited(_)));
            events.push(event);
        }
        events
    }

    fn data(events: &[SessionEvent]) -> Vec<u8> {
        events
            .iter()
            .filter_map(|event| match event {
                SessionEvent::Data(bytes) => Some(bytes.as_slice()),
                _ => None,
            })
            .flatten()
            .copied()
            .collect()
    }

    /// Wait until `marker` has appeared in the stream's output.
    fn wait_for_output(stream: &AttachStream, marker: &[u8]) {
        let mut seen = Vec::new();
        while !seen.windows(marker.len()).any(|window| window == marker) {
            if let SessionEvent::Data(bytes) =
                stream.recv_timeout(T).expect("the marker never arrived")
            {
                seen.extend_from_slice(&bytes);
            }
        }
    }

    fn zeros(event: &SessionEvent) -> usize {
        match event {
            SessionEvent::Data(bytes) => bytes.iter().filter(|byte| **byte == 0).count(),
            _ => 0,
        }
    }

    /// Run `f` on its own thread and fail, rather than hang, if it never
    /// returns: a wedged actor is exactly what some of these tests look for.
    fn within<R: Send + 'static>(what: &str, f: impl FnOnce() -> R + Send + 'static) -> R {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(T)
            .unwrap_or_else(|_| panic!("{what} did not complete within {T:?}"))
    }

    /// An actor driven by hand, one command at a time, so the order in which
    /// the reader and the reaper report can be chosen instead of raced.
    fn hand_driven_actor() -> (Actor, Arc<Queue>) {
        let pair = open(24, 80).unwrap();
        let (input, _jobs) = mpsc::sync_channel(INPUT_QUEUE_DEPTH);
        let queue = Queue::new(DEFAULT_ATTACHMENT_CAPACITY, OnFull::Detach);
        let actor = Actor {
            session: SessionRef::new("/private", "hand", "generation-a"),
            master: pair.master,
            signals: Arc::new(ChildSignals::new(None)),
            input,
            input_gate: Arc::default(),
            attachments: vec![Attachment(Arc::clone(&queue))],
            lifecycle: Lifecycle::Running,
            published: Lifecycle::Running,
            publish_exit_at: None,
            output_closed: false,
        };
        (actor, queue)
    }

    fn take_queued(queue: &Queue) -> Vec<SessionEvent> {
        let mut state = queue.lock();
        state.queued = 0;
        state.events.drain(..).collect()
    }

    /// PR #31 P1 / PR #32: the reaper reported before the reader drained the
    /// child's last bytes, and every later `Output` was discarded.
    #[test]
    fn output_the_reader_reports_after_the_reap_is_forwarded_and_the_exit_follows_it() {
        let (mut actor, queue) = hand_driven_actor();
        let status = ExitStatus {
            code: Some(0),
            signal: None,
        };
        actor.handle(Command::Reaped(status));
        actor.handle(Command::Output(b"final bytes".to_vec()));
        assert_eq!(
            take_queued(&queue),
            vec![SessionEvent::Data(b"final bytes".to_vec())]
        );
        // Requests and queries see the exit at once; only the event waits.
        assert_eq!(actor.lifecycle, Lifecycle::Exited(status));
        assert!(actor.publish_exit_at.is_some());
        actor.handle(Command::Output(b"more".to_vec()));
        actor.handle(Command::ReaderClosed);
        assert_eq!(
            take_queued(&queue),
            vec![
                SessionEvent::Data(b"more".to_vec()),
                SessionEvent::OutputClosed,
                SessionEvent::Lifecycle(Lifecycle::Exited(status)),
            ]
        );
        assert_eq!(actor.publish_exit_at, None);
    }

    /// A reader that plays back a script, then reports the end forever.
    struct Scripted(VecDeque<io::Result<&'static [u8]>>);

    impl Read for Scripted {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.0.pop_front() {
                Some(Ok(bytes)) => {
                    buf[..bytes.len()].copy_from_slice(bytes);
                    Ok(bytes.len())
                }
                Some(Err(error)) => Err(error),
                None => Ok(0),
            }
        }
    }

    /// PR #31 P1 / PR #32: an end of output that the kernel reports before
    /// the child's last write has reached the master is not the end. It
    /// comes as `Ok(0)` through portable-pty and as EIO from a raw reader.
    #[test]
    fn an_early_end_does_not_end_the_output() {
        let eio = || Err(io::Error::from_raw_os_error(libc::EIO));
        let eof = || Ok(&b""[..]);
        let reader = Scripted(VecDeque::from([
            Ok(&b"first"[..]),
            eof(),
            eof(),
            Ok(&b"tail"[..]),
            eio(),
            Ok(&b"-and-more"[..]),
        ]));
        let (tx, rx) = mpsc::channel();
        read_master(Box::new(reader), tx, None);
        let mut output = Vec::new();
        let mut closed = false;
        for command in rx.try_iter() {
            match command {
                Command::Output(bytes) => {
                    assert!(!closed, "output after the reader closed");
                    output.extend(bytes);
                }
                Command::ReaderClosed => closed = true,
                _ => panic!("the reader sent something other than output and its close"),
            }
        }
        assert!(closed, "the reader never closed");
        assert_eq!(
            output, b"firsttail-and-more",
            "bytes after an early end were dropped"
        );
    }

    /// PR #31 P1: a child that writes a known payload and exits at once must
    /// deliver the whole payload, and deliver it before its exit event.
    #[test]
    fn a_child_that_prints_and_exits_delivers_everything_before_its_exit() {
        for round in 0..40 {
            let marker = format!("end-of-round-{round}");
            let script = format!("head -c 32768 /dev/zero | tr '\\0' x; printf {marker}");
            let (_owner, _client, stream) =
                sh_attached("final", &script, DEFAULT_ATTACHMENT_CAPACITY);
            let events = until_closed_and_exited(&stream);
            let mut expected = vec![b'x'; 32768];
            expected.extend_from_slice(marker.as_bytes());
            assert_eq!(
                data(&events).len(),
                expected.len(),
                "round {round} lost output"
            );
            assert_eq!(data(&events), expected, "round {round} reordered output");
            let exit = events
                .iter()
                .position(|event| matches!(event, SessionEvent::Lifecycle(Lifecycle::Exited(_))))
                .unwrap();
            let last_data = events
                .iter()
                .rposition(|event| matches!(event, SessionEvent::Data(_)))
                .unwrap();
            assert!(
                last_data < exit,
                "round {round}: output arrived after the exit event: {events:?}"
            );
        }
    }

    /// Every byte of `Data` waiting in `stream`, without taking it.
    fn queued_data(stream: &AttachStream) -> Vec<u8> {
        data(
            &stream
                .queue
                .lock()
                .events
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
        )
    }

    fn wait_for_queued(stream: &AttachStream, marker: &[u8]) {
        assert!(
            (0..500).any(|_| {
                let seen = queued_data(stream);
                seen.windows(marker.len()).any(|window| window == marker) || {
                    thread::sleep(Duration::from_millis(10));
                    false
                }
            }),
            "{:?} never reached the stream",
            String::from_utf8_lossy(marker)
        );
    }

    /// PR #31 P1 / PR #32 with real processes and no luck involved. The owner
    /// stream is held full, so the reader parks before the child's last write
    /// and those bytes are provably still in the PTY when the child is
    /// reaped. Released afterwards, they must all arrive.
    #[test]
    fn output_still_in_the_pty_when_the_child_is_reaped_is_delivered() {
        let (_owner, client, stream) = sh_attached(
            "reaped-unread",
            "read a; read b; printf tail",
            MIN_ATTACHMENT_CAPACITY,
        );
        assert_eq!(
            stream.recv_timeout(T).unwrap(),
            SessionEvent::Lifecycle(Lifecycle::Running)
        );
        // One read short of full, as far as the reader can tell: whether it
        // is already blocked in a read or has not reached the gate yet, it
        // reads once more (the echo of `a`) and then parks at the gate.
        let held = {
            let mut state = stream.queue.lock();
            let held = MIN_ATTACHMENT_CAPACITY - 1 - (state.queued + state.reserved);
            state.reserved += held;
            held
        };
        client.input(b"a\n".to_vec()).unwrap();
        wait_for_queued(&stream, b"a");
        client.input(b"b\n".to_vec()).unwrap();
        assert!(
            (0..500).any(|_| {
                thread::sleep(Duration::from_millis(10));
                matches!(client.lifecycle(), Ok(Lifecycle::Exited(_)))
            }),
            "the child was never reaped"
        );
        assert!(
            !queued_data(&stream).ends_with(b"tail"),
            "the tail was read before the reap, so this proves nothing"
        );
        {
            let mut state = stream.queue.lock();
            state.reserved = state.reserved.saturating_sub(held);
        }
        stream.queue.changed.notify_all();
        let events = until_closed_and_exited(&stream);
        assert!(
            data(&events).ends_with(b"tail"),
            "output unread at the reap was dropped: {events:?}"
        );
    }

    /// PR #32: the reviewer's reproduction, through the owner's pre-registered
    /// stream the daemon uses. An 8 MiB burst must keep its tail marker.
    #[test]
    fn an_eight_mib_burst_keeps_its_tail_marker() {
        for round in 0..12 {
            let marker = format!("sentinel-{round}");
            let script = format!("head -c 8388608 /dev/zero; printf {marker}");
            let (_owner, _client, stream) =
                sh_attached("burst", &script, DEFAULT_ATTACHMENT_CAPACITY);
            let events = until_closed_and_exited(&stream);
            let bytes = data(&events);
            assert_eq!(
                bytes.len(),
                8 * 1024 * 1024 + marker.len(),
                "round {round} lost output"
            );
            assert!(
                bytes.ends_with(marker.as_bytes()),
                "round {round} lost its tail marker"
            );
            let closed = events
                .iter()
                .position(|event| *event == SessionEvent::OutputClosed)
                .unwrap();
            assert!(
                events[closed + 1..]
                    .iter()
                    .all(|event| !matches!(event, SessionEvent::Data(_))),
                "round {round}: output after OutputClosed"
            );
        }
    }

    /// The exit still gets published when a descendant keeps the PTY open,
    /// and output keeps flowing after it until the PTY closes.
    #[test]
    fn an_exit_is_published_after_the_drain_bound_when_a_descendant_holds_the_pty() {
        // The kernel hangs up the foreground group when the session leader
        // exits. Ignoring SIGHUP before the fork, so the descendant inherits
        // it, is what lets the descendant outlive the leader.
        let (_owner, _client, stream) = sh_attached(
            "holder",
            "trap '' HUP; (sleep 1; printf late) & exit 3",
            DEFAULT_ATTACHMENT_CAPACITY,
        );
        assert_eq!(
            stream.recv_timeout(T).unwrap(),
            SessionEvent::Lifecycle(Lifecycle::Running)
        );
        let started = Instant::now();
        let exit = stream.recv_timeout(T).unwrap();
        assert_eq!(
            exit,
            SessionEvent::Lifecycle(Lifecycle::Exited(ExitStatus {
                code: Some(3),
                signal: None
            }))
        );
        assert!(
            started.elapsed() < Duration::from_millis(900),
            "the exit waited for the descendant"
        );
        let rest = until_closed_and_exited_after_exit(&stream);
        assert_eq!(data(&rest), b"late");
    }

    fn until_closed_and_exited_after_exit(stream: &AttachStream) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        loop {
            let event = stream.recv_timeout(T).expect("the PTY never closed");
            let closed = event == SessionEvent::OutputClosed;
            events.push(event);
            if closed {
                return events;
            }
        }
    }

    /// PR #31 P1: a stream that stops reading is cut off at its capacity; it
    /// cannot grow the owner's memory, and it does not slow anyone else.
    #[test]
    fn a_stalled_attachment_is_detached_at_its_capacity_and_others_keep_receiving() {
        let total = 4 * 1024 * 1024;
        let (session, owner) = sh(
            "stall",
            &format!("read go; head -c {total} /dev/zero; exec sleep 30"),
        );
        let (_stalled_client, stalled) = owner
            .attach_with_capacity(&session, MIN_ATTACHMENT_CAPACITY)
            .unwrap();
        let (client, active) = owner.attach(&session).unwrap();
        client.input(b"go\n".to_vec()).unwrap();
        let mut received = 0;
        while received < total {
            received += zeros(&active.recv_timeout(T).unwrap());
        }
        assert_eq!(received, total);
        assert!(
            stalled.queue.lock().queued <= MIN_ATTACHMENT_CAPACITY,
            "the stalled queue outgrew its capacity"
        );
        let mut buffered = 0;
        let outcome = loop {
            match stalled.recv_timeout(T) {
                Ok(SessionEvent::Data(bytes)) => buffered += bytes.len(),
                Ok(_) => {}
                Err(error) => break error,
            }
        };
        assert_eq!(
            outcome,
            SessionError::Overflowed {
                capacity: MIN_ATTACHMENT_CAPACITY
            }
        );
        assert!(
            buffered < MIN_ATTACHMENT_CAPACITY,
            "delivered {buffered} bytes from a {MIN_ATTACHMENT_CAPACITY} byte queue"
        );
        // Detached for good: nothing more is queued for it.
        client.terminate().unwrap();
        assert!(matches!(
            stalled.recv_timeout(Duration::from_millis(200)),
            Err(SessionError::Overflowed { .. })
        ));
    }

    /// The default `attach` is bounded too, at [`DEFAULT_ATTACHMENT_CAPACITY`].
    #[test]
    fn the_default_attachment_is_bounded() {
        let total = DEFAULT_ATTACHMENT_CAPACITY * 2;
        let (session, owner) = sh(
            "default-bound",
            &format!("read go; head -c {total} /dev/zero; exec sleep 30"),
        );
        let (client, stalled) = owner.attach(&session).unwrap();
        // Room for everything, so only the stalled stream can overflow even
        // when this thread counts slower than the PTY produces.
        let (_, active) = owner.attach_with_capacity(&session, total * 2).unwrap();
        client.input(b"go\n".to_vec()).unwrap();
        let mut received = 0;
        while received < total {
            received += zeros(&active.recv_timeout(T).unwrap());
        }
        assert_eq!(stalled.capacity(), DEFAULT_ATTACHMENT_CAPACITY);
        assert!(stalled.queue.lock().queued <= DEFAULT_ATTACHMENT_CAPACITY);
        let outcome = loop {
            if let Err(error) = stalled.recv_timeout(T) {
                break error;
            }
        };
        assert_eq!(
            outcome,
            SessionError::Overflowed {
                capacity: DEFAULT_ATTACHMENT_CAPACITY
            }
        );
        client.terminate().unwrap();
    }

    /// The owner's own stream is never cut off: it holds the reader back, so
    /// memory stays bounded and every byte still arrives.
    #[test]
    fn the_owner_stream_holds_the_reader_instead_of_losing_output() {
        let total = 2 * 1024 * 1024;
        let (_owner, _client, stream) = sh_attached(
            "owner-flow",
            &format!("head -c {total} /dev/zero; printf tail"),
            MIN_ATTACHMENT_CAPACITY,
        );
        thread::sleep(Duration::from_millis(500));
        {
            let state = stream.queue.lock();
            assert!(
                state.queued + state.reserved
                    <= MIN_ATTACHMENT_CAPACITY
                        + READ_CHUNK
                        + event_cost(&SessionEvent::OutputClosed) * 2,
                "the owner stream grew to {} bytes",
                state.queued + state.reserved
            );
        }
        let events = until_closed_and_exited(&stream);
        let bytes = data(&events);
        assert_eq!(bytes.len(), total + 4);
        assert!(bytes.ends_with(b"tail"));
    }

    /// Dropping a stream releases it: the actor forgets the queue.
    #[test]
    fn a_dropped_stream_is_forgotten() {
        let (session, owner) = sh("dropped", "while :; do printf tick; sleep 0.01; done");
        let (_client, stream) = owner.attach(&session).unwrap();
        let queue = Arc::clone(&stream.queue);
        drop(stream);
        assert!(
            (0..200).any(|_| {
                thread::sleep(Duration::from_millis(10));
                Arc::strong_count(&queue) == 1
            }),
            "the actor still holds a dropped stream"
        );
    }

    /// PR #31 P1: a child that never reads stdin, and more input than the PTY
    /// holds. Lifecycle queries, resize and termination must still complete,
    /// and the blocked input must be released once the child is gone.
    #[test]
    fn a_child_that_never_reads_input_cannot_wedge_lifecycle_control() {
        let (session, owner) = sh(
            "deaf",
            "read go; stty raw -echo; printf raw-mode; exec sleep 30",
        );
        let (client, stream) = owner.attach(&session).unwrap();
        client.input(b"go\n".to_vec()).unwrap();
        wait_for_output(&stream, b"raw-mode");
        let writer = client.clone();
        let (done, input_result) = mpsc::channel();
        thread::spawn(move || {
            let _ = done.send(writer.input(vec![b'x'; 1024 * 1024]));
        });
        assert!(
            input_result
                .recv_timeout(Duration::from_millis(300))
                .is_err(),
            "the input finished, so it never filled the PTY and this test proves nothing"
        );
        let query = client.clone();
        assert_eq!(
            within("a lifecycle query", move || query.lifecycle()),
            Ok(Lifecycle::Running)
        );
        let resize = client.clone();
        let size = PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        };
        assert_eq!(within("a resize", move || resize.resize(size)), Ok(()));
        let terminate = client.clone();
        assert_eq!(within("terminate", move || terminate.terminate()), Ok(()));
        let exited = ExitStatus {
            code: None,
            signal: Some(libc::SIGTERM),
        };
        let events = until_closed_and_exited(&stream);
        assert!(
            events.contains(&SessionEvent::Lifecycle(Lifecycle::Exited(exited))),
            "{events:?}"
        );
        assert_eq!(
            input_result
                .recv_timeout(T)
                .expect("the blocked input was never released"),
            Err(SessionError::Exited(exited))
        );
    }

    /// Dropping the owner recovers a session whose input is stuck too.
    #[test]
    fn dropping_the_owner_recovers_a_session_with_stuck_input() {
        let (session, owner) = sh(
            "deaf-owner",
            "read go; stty raw -echo; printf raw-mode; exec sleep 30",
        );
        let (client, stream) = owner.attach(&session).unwrap();
        client.input(b"go\n".to_vec()).unwrap();
        wait_for_output(&stream, b"raw-mode");
        let writer = client.clone();
        let (done, input_result) = mpsc::channel();
        thread::spawn(move || {
            let _ = done.send(writer.input(vec![b'x'; 1024 * 1024]));
        });
        assert!(
            input_result
                .recv_timeout(Duration::from_millis(300))
                .is_err()
        );
        drop(owner);
        let revoked = loop {
            match stream.recv_timeout(T).unwrap() {
                SessionEvent::Data(_) => {}
                event => break event,
            }
        };
        assert_eq!(revoked, SessionEvent::Lifecycle(Lifecycle::OwnerLost));
        assert_eq!(
            input_result.recv_timeout(T).unwrap(),
            Err(SessionError::OwnerLost)
        );
        let events = until_closed_after(&stream);
        assert!(
            events.contains(&SessionEvent::OutputClosed),
            "the owner-loss SIGTERM never ended the child"
        );
    }

    fn until_closed_after(stream: &AttachStream) -> Vec<SessionEvent> {
        until_closed_and_exited_after_exit(stream)
    }

    /// Input beyond the queue is refused with a typed error, not queued
    /// without limit.
    #[test]
    fn input_beyond_the_queue_is_refused_as_backpressure() {
        let (session, owner) = sh(
            "deaf-queue",
            "read go; stty raw -echo; printf raw-mode; exec sleep 30",
        );
        let (client, stream) = owner.attach(&session).unwrap();
        client.input(b"go\n".to_vec()).unwrap();
        wait_for_output(&stream, b"raw-mode");
        let (done, results) = mpsc::channel();
        for _ in 0..INPUT_QUEUE_DEPTH + 8 {
            let writer = client.clone();
            let done = done.clone();
            thread::spawn(move || {
                let _ = done.send(writer.input(vec![b'x'; 256 * 1024]));
            });
        }
        drop(done);
        let refused = results
            .recv_timeout(T)
            .expect("every input waited, so the queue is unbounded");
        assert_eq!(refused, Err(SessionError::InputBackpressure));
        client.terminate().unwrap();
        let released: Vec<_> = results.iter().collect();
        assert_eq!(released.len(), INPUT_QUEUE_DEPTH + 7);
        assert!(released.iter().all(|result| result.is_err()));
    }

    fn sent_to(pid: libc::pid_t) -> Vec<(libc::pid_t, libc::c_int)> {
        SENT.lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|(target, _)| target.abs() == pid)
            .collect()
    }

    static SENT: Mutex<Vec<(libc::pid_t, libc::c_int)>> = Mutex::new(Vec::new());

    fn record_kill(target: libc::pid_t, signal: libc::c_int) -> libc::c_int {
        SENT.lock().unwrap().push((target, signal));
        0
    }

    fn exists(pid: u32) -> bool {
        // SAFETY: signal 0 only asks whether the pid names a process.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    /// PR #31 P1: the reaper must not release the child's pid while a signal
    /// is in flight to it. Holding the signal lock, the exited child stays an
    /// unreaped zombie; once collected, no signal is sent to its pid again.
    #[test]
    // The child is reaped by `collect_fenced`, which is what this tests.
    #[allow(clippy::zombie_processes)]
    fn a_signal_cannot_race_the_reaper_onto_a_recycled_pid() {
        let child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        let signals = Arc::new(ChildSignals {
            kill: record_kill,
            ..ChildSignals::new(Some(pid))
        });
        wait_until_exited(pid);
        assert!(
            exists(pid),
            "waiting for the exit released the pid before the fence"
        );
        let in_flight = signals.lock();
        let reaper_signals = Arc::clone(&signals);
        let reaper = thread::spawn(move || collect_fenced(pid, &reaper_signals));
        thread::sleep(Duration::from_millis(200));
        assert!(
            exists(pid),
            "the reaper collected the child while a signal held the lock"
        );
        drop(in_flight);
        assert_eq!(
            reaper.join().unwrap(),
            Some(ExitStatus {
                code: Some(0),
                signal: None
            })
        );
        assert!(!exists(pid));
        signals.signal(libc::SIGTERM).unwrap();
        signals.signal_group(libc::SIGTERM).unwrap();
        assert!(
            sent_to(pid as libc::pid_t).is_empty(),
            "a signal went to a released pid"
        );
    }

    /// A stopped child has not exited. On macOS `waitid` reports the stop to
    /// a `WEXITED | WNOWAIT` wait; a reaper that took it for the exit blocked
    /// in `waitpid` under the signal lock, and the SIGKILL below could never
    /// be sent. That is how a startup deadline, which freezes the child with
    /// SIGSTOP, hung the daemon on a Mac.
    #[test]
    // The child is reaped by `collect_fenced` on the reaper thread.
    #[allow(clippy::zombie_processes)]
    fn a_stopped_child_does_not_hold_the_signal_lock() {
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let signals = Arc::new(ChildSignals::new(Some(pid)));
        let reaper_signals = Arc::clone(&signals);
        let (reaped_tx, reaped_rx) = mpsc::channel();
        thread::spawn(move || {
            wait_until_exited(pid);
            let _ = reaped_tx.send(collect_fenced(pid, &reaper_signals));
        });
        signals.signal(libc::SIGSTOP).unwrap();
        // Long enough for the reaper to see the stop, as it did on macOS.
        thread::sleep(Duration::from_millis(300));
        assert!(
            reaped_rx.try_recv().is_err(),
            "the reaper reported a stopped child as reaped"
        );
        let killer_signals = Arc::clone(&signals);
        let (sent_tx, sent_rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = sent_tx.send(killer_signals.signal(libc::SIGKILL));
        });
        assert!(
            sent_rx.recv_timeout(T).is_ok_and(|sent| sent.is_ok()),
            "SIGKILL could not take the signal lock: the reaper is holding it"
        );
        let status = reaped_rx.recv_timeout(T).expect("the killed child was reaped");
        assert_eq!(status.and_then(|status| status.signal), Some(libc::SIGKILL));
    }

    /// Until the child is reaped, signals reach it and its process group.
    #[test]
    // The child is reaped by `collect_fenced`, which is what this tests.
    #[allow(clippy::zombie_processes)]
    fn signals_reach_the_child_until_it_is_reaped() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let signals = ChildSignals {
            kill: record_kill,
            ..ChildSignals::new(Some(pid))
        };
        signals.signal(libc::SIGTERM).unwrap();
        signals.signal_group(libc::SIGHUP).unwrap();
        let pid_t = pid as libc::pid_t;
        assert_eq!(
            sent_to(pid_t),
            vec![(pid_t, libc::SIGTERM), (-pid_t, libc::SIGHUP)]
        );
        child.kill().unwrap();
        wait_until_exited(pid);
        assert!(
            collect_fenced(pid, &signals)
                .is_some_and(|status| status.signal == Some(libc::SIGKILL))
        );
        signals.signal(libc::SIGKILL).unwrap();
        assert_eq!(sent_to(pid_t).len(), 2, "signalled after the reap");
    }

    /// PR #31 P1, end to end: once the substrate has reported the exit,
    /// `terminate` and `kill` are refused rather than sent to the old pid.
    #[test]
    fn signals_after_the_exit_are_refused() {
        let (session, owner) = sh("after-exit", "exit 0");
        let (client, stream) = owner.attach(&session).unwrap();
        until_closed_and_exited(&stream);
        let exited = SessionError::Exited(ExitStatus {
            code: Some(0),
            signal: None,
        });
        assert_eq!(client.terminate(), Err(exited.clone()));
        assert_eq!(client.kill(), Err(exited.clone()));
        assert_eq!(client.signal_process_group(libc::SIGTERM), Err(exited));
    }

    /// PR #31 P2: a signal death is reported as the signal, not as an
    /// ordinary nonzero exit.
    #[test]
    fn a_signalled_child_reports_its_signal() {
        for (signal, send) in [
            (
                libc::SIGTERM,
                SessionClient::terminate as fn(&SessionClient) -> Result<(), SessionError>,
            ),
            (libc::SIGKILL, SessionClient::kill),
            (libc::SIGHUP, SessionClient::hangup),
        ] {
            let (session, owner) = sh("signalled", "exec sleep 30");
            let (client, stream) = owner.attach(&session).unwrap();
            assert_eq!(
                stream.recv_timeout(T).unwrap(),
                SessionEvent::Lifecycle(Lifecycle::Running)
            );
            send(&client).unwrap();
            let expected = ExitStatus {
                code: None,
                signal: Some(signal),
            };
            assert!(
                until_closed_and_exited(&stream)
                    .contains(&SessionEvent::Lifecycle(Lifecycle::Exited(expected)))
            );
            assert_eq!(client.lifecycle(), Ok(Lifecycle::Exited(expected)));
        }
        let (session, owner) = sh("self-signalled", "kill -USR1 $$");
        let (_client, stream) = owner.attach(&session).unwrap();
        let expected = ExitStatus {
            code: None,
            signal: Some(libc::SIGUSR1),
        };
        assert!(
            until_closed_and_exited(&stream)
                .contains(&SessionEvent::Lifecycle(Lifecycle::Exited(expected)))
        );
    }

    #[test]
    fn exit_statuses_keep_the_signal_from_every_source() {
        assert_eq!(
            ExitStatus::from_wait_status(libc::SIGKILL),
            ExitStatus {
                code: None,
                signal: Some(libc::SIGKILL)
            }
        );
        assert_eq!(
            ExitStatus::from_wait_status(5 << 8),
            ExitStatus {
                code: Some(5),
                signal: None
            }
        );
        assert_eq!(
            ExitStatus::from_portable(PortableExitStatus::with_signal("Terminated")),
            ExitStatus {
                code: None,
                signal: Some(libc::SIGTERM)
            }
        );
        assert_eq!(
            ExitStatus::from_portable(PortableExitStatus::with_signal("SIGKILL")),
            ExitStatus {
                code: None,
                signal: Some(libc::SIGKILL)
            }
        );
        assert_eq!(
            ExitStatus::from_portable(PortableExitStatus::with_exit_code(3)),
            ExitStatus {
                code: Some(3),
                signal: None
            }
        );
        assert_eq!(
            ExitStatus::from_portable(PortableExitStatus::with_exit_code(0)),
            ExitStatus {
                code: Some(0),
                signal: None
            }
        );
    }
}
