//! `pty run -d --operation-fd N --launch-operation <h> --id <runtimeId> …`:
//! the launcher half of a launch operation.
//!
//! The owner reserved the operation and holds the other end of `N`. The
//! launcher validates `N`, reads exactly the envelope line from it, claims
//! the reservation, looks once more for a queued withdrawal and for an
//! occupied name, and only then forks the daemon, gated. It records the
//! daemon's identity and hands it the writer role before opening the gate,
//! so at every instant either the launcher or the daemon is the recorded
//! writer, and a crash leaves an operation that `settle` can classify.
//!
//! Every refusal and every decision here goes to the owner as a frame. A
//! refusal describes this launcher only; it is never a statement about the
//! operation's closure.

use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use pty_core::operation::barrier::barrier;
use pty_core::operation::envelope::ENVELOPE_MAX_BYTES;
use pty_core::operation::frames::{
    FRAME_MAX_BYTES, OwnerCommand, OwnerFrame, parse_owner_frame, producer_frame,
};
use pty_core::operation::settle::{observe_occupancy, occupancy_rule};
use pty_core::operation::{
    Cause, Decision, Envelope, OperationLock, OperationRecord, OperationStore, Phase, ProcessId,
    State, Writer, WriterRole, parse_envelope,
};
use pty_core::registry;

use super::{CliResult, SpawnParams};

/// The launcher's exit status for a refusal or a decision it made itself.
const REFUSED: i32 = 1;
/// A descriptor or argument that fails validation: nothing was read.
const USAGE: i32 = 64;
/// How long the launcher waits for the envelope line.
const ENVELOPE_WAIT: Duration = Duration::from_secs(30);

/// Check `N` exactly as the contract lists: a connected `AF_UNIX`
/// `SOCK_STREAM` socket with no pending error, at 3 or above.
pub fn validate_control_fd(fd: RawFd) -> Result<(), String> {
    if fd < 3 {
        return Err(format!("--operation-fd {fd}: must be 3 or above"));
    }
    // SAFETY: each call fills only the storage passed with its length.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) != 0 {
            return Err(format!(
                "--operation-fd {fd}: {}",
                std::io::Error::last_os_error()
            ));
        }
        if st.st_mode & libc::S_IFMT != libc::S_IFSOCK {
            return Err(format!("--operation-fd {fd}: not a socket"));
        }
        let int_opt = |name: libc::c_int| -> Option<libc::c_int> {
            let mut value: libc::c_int = 0;
            let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            (libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                name,
                (&mut value as *mut libc::c_int).cast(),
                &mut len,
            ) == 0)
                .then_some(value)
        };
        if int_opt(libc::SO_TYPE) != Some(libc::SOCK_STREAM) {
            return Err(format!("--operation-fd {fd}: not SOCK_STREAM"));
        }
        let mut addr: libc::sockaddr_storage = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        if libc::getsockname(
            fd,
            (&mut addr as *mut libc::sockaddr_storage).cast(),
            &mut len,
        ) != 0
            || addr.ss_family as libc::c_int != libc::AF_UNIX
        {
            return Err(format!("--operation-fd {fd}: not AF_UNIX"));
        }
        let mut peer: libc::sockaddr_storage = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        if libc::getpeername(
            fd,
            (&mut peer as *mut libc::sockaddr_storage).cast(),
            &mut len,
        ) != 0
        {
            return Err(format!("--operation-fd {fd}: not connected"));
        }
        if int_opt(libc::SO_ERROR) != Some(0) {
            return Err(format!("--operation-fd {fd}: socket has a pending error"));
        }
    }
    Ok(())
}

/// Move `N` to a close-on-exec descriptor at 10 or above and close `N`.
fn rehome(fd: RawFd) -> Result<RawFd, String> {
    // SAFETY: fcntl and close on the descriptor the owner named.
    unsafe {
        let moved = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10);
        if moved < 0 {
            return Err(format!(
                "--operation-fd {fd}: {}",
                std::io::Error::last_os_error()
            ));
        }
        libc::close(fd);
        // Blocking reads for the envelope; the daemon makes its own copy
        // non-blocking once it owns the channel.
        let flags = libc::fcntl(moved, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(moved, libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
        Ok(moved)
    }
}

/// The owner channel, from the launcher's side.
struct Channel {
    fd: RawFd,
    seq: u64,
}

/// Discard what the owner queued and the launcher will never read. A unix
/// stream socket closed with unread input resets its peer, and the owner
/// could lose the frames already sent to it; closed empty, it reads every
/// frame and then end of file.
fn discard_input(fd: RawFd) {
    let mut buf = [0u8; 16384];
    // SAFETY: draining our own descriptor into a local buffer.
    while unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) } > 0 {}
}

impl Drop for Channel {
    fn drop(&mut self) {
        if self.fd >= 0 {
            discard_input(self.fd);
        }
    }
}

impl Channel {
    fn send(&mut self, frame: Vec<u8>) {
        let mut at = 0;
        while at < frame.len() {
            // SAFETY: writing from a live buffer to our own descriptor.
            let n = unsafe {
                libc::send(
                    self.fd,
                    frame[at..].as_ptr().cast(),
                    frame.len() - at,
                    send_flags(),
                )
            };
            if n > 0 {
                at += n as usize;
            } else if n < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            } else {
                // The owner is gone. Nothing here depends on it hearing us.
                return;
            }
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Read the envelope line byte by byte, so every later frame stays queued
    /// in the socket for the daemon.
    fn read_envelope_line(&self) -> Result<Vec<u8>, &'static str> {
        let deadline = Instant::now() + ENVELOPE_WAIT;
        let mut line = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("envelope-timeout");
            }
            let mut pfd = libc::pollfd {
                fd: self.fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one pollfd.
            let ready =
                unsafe { libc::poll(&mut pfd, 1, left.as_millis().min(i32::MAX as u128) as i32) };
            if ready < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err("envelope-unreadable");
            }
            if ready == 0 {
                continue;
            }
            let mut byte = 0u8;
            // SAFETY: one byte into a local.
            let n = unsafe { libc::read(self.fd, (&mut byte as *mut u8).cast(), 1) };
            match n {
                1 if byte == b'\n' => return Ok(line),
                1 => {
                    line.push(byte);
                    if line.len() > ENVELOPE_MAX_BYTES {
                        return Err("too-large");
                    }
                }
                0 => return Err("owner-eof"),
                _ if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {}
                _ => return Err("envelope-unreadable"),
            }
        }
    }

    /// A non-consuming look at what the owner has already queued: a complete
    /// valid cancel, end of file, or malformed input is a withdrawal.
    fn peek_withdrawal(&self, envelope: &Envelope) -> Option<(Cause, Option<String>)> {
        let mut buf = vec![0u8; FRAME_MAX_BYTES * 2];
        // SAFETY: peeking into a buffer of the length passed.
        let n = unsafe {
            libc::recv(
                self.fd,
                buf.as_mut_ptr().cast(),
                buf.len(),
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        if n == 0 {
            return Some((Cause::OwnerEof, None));
        }
        if n < 0 {
            return None;
        }
        let queued = &buf[..n as usize];
        let mut rest = queued;
        while let Some(end) = rest.iter().position(|b| *b == b'\n') {
            match parse_owner_frame(&rest[..end], &envelope.operation_id, &envelope.sha256) {
                OwnerFrame::Command(OwnerCommand::Cancel { request_id }) => {
                    return Some((Cause::Cancel, Some(request_id)));
                }
                OwnerFrame::Malformed => return Some((Cause::Malformed, None)),
                _ => {}
            }
            rest = &rest[end + 1..];
        }
        if rest.len() >= FRAME_MAX_BYTES {
            return Some((Cause::Malformed, None));
        }
        None
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

/// Everything a frame needs besides its type and extra fields.
struct Context<'a> {
    envelope: &'a Envelope,
}

impl Context<'_> {
    fn frame(
        &self,
        channel: &mut Channel,
        kind: &str,
        record: Option<&OperationRecord>,
        extra: Map<String, Value>,
    ) {
        let seq = channel.next_seq();
        let line = producer_frame(
            kind,
            seq,
            &self.envelope.operation_id,
            &self.envelope.sha256,
            &self.envelope.value,
            record,
            &[],
            extra,
        );
        channel.send(line);
    }
}

fn extra(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.clone()))
        .collect()
}

/// A refusal before the envelope could be read: there is no operation to
/// name, so only the code is sent.
fn refuse_unidentified(channel: &mut Channel, code: &str, path: &str, reason: &str) -> CliResult {
    let seq = channel.next_seq();
    let mut frame = json!({
        "type": "refused",
        "version": 1,
        "seq": seq,
        "operationId": null,
        "envelopeSha256": null,
        "envelope": null,
        "recordRevision": null,
        "state": null,
        "phase": null,
        "participants": null,
        "incarnation": null,
        "tickets": [],
        "code": code,
    });
    if !path.is_empty() || !reason.is_empty() {
        frame["path"] = json!(path);
        frame["reason"] = json!(reason);
    }
    let mut line = serde_json::to_vec(&frame).expect("a JSON value serializes");
    line.push(b'\n');
    channel.send(line);
    eprintln!("pty run: refused: {code} {path} {reason}");
    Ok(REFUSED)
}

/// The launcher. `name` is `--id`, `digest` is `--launch-operation`.
pub fn launch(name: &str, params: SpawnParams, control_fd: RawFd, digest: &str) -> CliResult {
    if let Err(message) = validate_control_fd(control_fd) {
        eprintln!("pty run: {message}");
        return Ok(USAGE);
    }
    let fd = match rehome(control_fd) {
        Ok(fd) => fd,
        Err(message) => {
            eprintln!("pty run: {message}");
            return Ok(USAGE);
        }
    };
    let mut channel = Channel { fd, seq: 0 };
    let line = match channel.read_envelope_line() {
        Ok(line) => line,
        Err(reason) => return refuse_unidentified(&mut channel, "envelope-invalid", "", reason),
    };
    let envelope = match parse_envelope(&line) {
        Ok(envelope) => envelope,
        Err(error) => {
            return refuse_unidentified(
                &mut channel,
                "envelope-invalid",
                &error.path,
                &error.reason,
            );
        }
    };
    let ctx = Context {
        envelope: &envelope,
    };
    let refuse = |channel: &mut Channel,
                  code: &str,
                  record: Option<&OperationRecord>,
                  more: &[(&str, Value)]| {
        let mut fields = vec![("code", json!(code))];
        fields.extend(more.iter().cloned());
        ctx.frame(channel, "refused", record, extra(&fields));
        eprintln!("pty run: refused: {code}");
        Ok(REFUSED)
    };
    if envelope.digest() != digest {
        return refuse(&mut channel, "digest-mismatch", None, &[]);
    }
    if envelope.runtime_id != name {
        return refuse(&mut channel, "runtime-id-mismatch", None, &[]);
    }

    // The session's creation and event locks, as `pty run` takes them.
    let event_lock = match registry::lock_or_refusal(&registry::event_lock_path(name)) {
        Ok(guard) => guard,
        Err(registry::LockRefusal::Busy) => return refuse(&mut channel, "session-busy", None, &[]),
        Err(registry::LockRefusal::Unavailable(cause)) => {
            return refuse(
                &mut channel,
                "session-unavailable",
                None,
                &[("detail", json!(cause))],
            );
        }
    };
    let creation_lock = match registry::lock_or_refusal(&registry::lock_path(name)) {
        Ok(guard) => guard,
        Err(registry::LockRefusal::Busy) => {
            event_lock.release();
            return refuse(&mut channel, "session-busy", None, &[]);
        }
        Err(registry::LockRefusal::Unavailable(cause)) => {
            event_lock.release();
            return refuse(
                &mut channel,
                "session-unavailable",
                None,
                &[("detail", json!(cause))],
            );
        }
    };

    // Claim the reservation.
    barrier("launcher-before-claim");
    let store = OperationStore::current();
    let op_lock = match OperationLock::exclusive(&store, digest, Duration::from_secs(2)) {
        Ok(Some(lock)) => lock,
        // A writer keeps the lock for its whole tenure; a reservation has
        // none. Whatever the record says now, it is not claimable.
        Ok(None) => {
            let observed = match store.read(digest) {
                Ok(Some(record)) => json!(record.state),
                _ => json!("locked"),
            };
            return refuse(
                &mut channel,
                "not-claimable",
                None,
                &[("observed", observed)],
            );
        }
        Err(error) => {
            return refuse(
                &mut channel,
                "operation-unavailable",
                None,
                &[("detail", json!(error.to_string()))],
            );
        }
    };
    let reserved = match store.read(digest) {
        Ok(Some(record)) => record,
        Ok(None) => {
            return refuse(
                &mut channel,
                "not-claimable",
                None,
                &[("observed", json!("absent"))],
            );
        }
        Err(error) => {
            return refuse(
                &mut channel,
                "not-claimable",
                None,
                &[("observed", json!(format!("unreadable:{error}")))],
            );
        }
    };
    if reserved.operation_id != envelope.operation_id || reserved.envelope_sha256 != envelope.sha256
    {
        return refuse(&mut channel, "wrong-envelope", Some(&reserved), &[]);
    }
    if reserved.state != State::Reserved || reserved.writer.role != WriterRole::None {
        return refuse(
            &mut channel,
            "not-claimable",
            Some(&reserved),
            &[("observed", json!(reserved.state))],
        );
    }
    let me = match ProcessId::current() {
        Ok(me) => me,
        Err(error) => {
            return refuse(
                &mut channel,
                "identity-unavailable",
                Some(&reserved),
                &[("detail", json!(error))],
            );
        }
    };
    let mut claim = reserved.clone();
    claim.state = State::Pending;
    claim.phase = Phase::Claimed;
    claim.writer = Writer::of(WriterRole::Launcher, me.clone());
    claim.participants.launcher = Some(me.clone());
    let claimed = match store.write_cas(&op_lock, digest, &reserved, claim) {
        Ok(record) => record,
        Err(error) => {
            return refuse(
                &mut channel,
                "not-claimable",
                Some(&reserved),
                &[("detail", json!(error.to_string()))],
            );
        }
    };

    // From here this launcher is the writer: every exit below records a
    // decision or hands the role on.
    let decide = |channel: &mut Channel,
                  record: &OperationRecord,
                  cause: Cause,
                  request_id: Option<String>,
                  observed: Option<String>|
     -> CliResult {
        let mut cancelled = record.clone();
        cancelled.state = State::Cancelled;
        cancelled.writer = Writer::none();
        cancelled.decision = Some(Decision::now(cause, request_id, observed));
        match store.write_cas(&op_lock, digest, record, cancelled) {
            Ok(written) => {
                let decision = json!(written.decision);
                ctx.frame(
                    channel,
                    "decision",
                    Some(&written),
                    extra(&[("decision", decision)]),
                );
            }
            Err(error) => {
                eprintln!("pty run: cannot record the decision: {error}");
            }
        }
        Ok(REFUSED)
    };

    barrier("launcher-before-peek");
    if let Some((cause, request_id)) = channel.peek_withdrawal(&envelope) {
        return decide(&mut channel, &claimed, cause, request_id, None);
    }
    let occupancy = observe_occupancy(&registry::session_dir(), name);
    if let Err((cause, observed)) =
        occupancy_rule(&occupancy, envelope.expected_incarnation.as_deref())
    {
        return decide(&mut channel, &claimed, cause, None, Some(observed));
    }
    // The decision is the daemon's from here; today's `pty run` drops the
    // event lock before it spawns the daemon too.
    event_lock.release();

    barrier("launcher-before-fork");
    let mut forking = claimed.clone();
    forking.phase = Phase::DaemonForking;
    let forking = match store.write_cas(&op_lock, digest, &claimed, forking) {
        Ok(record) => record,
        Err(error) => {
            eprintln!("pty run: cannot record daemon-forking: {error}");
            return Ok(REFUSED);
        }
    };
    let executable = match super::daemon_executable() {
        Ok(executable) => executable,
        Err(message) => {
            return decide(&mut channel, &forking, Cause::Refused, None, Some(message));
        }
    };
    let mut params = params;
    params.command = match pty_core::spawn::resolve_command(&params.command) {
        Ok(command) => command,
        Err(message) => return decide(&mut channel, &forking, Cause::Refused, None, Some(message)),
    };
    // Exactly what today's `pty run` hands its daemon, so the session's
    // environment is today's too.
    params.creation_lock_owner_pid = Some(std::process::id() as i32);
    let gated = match pty_lifecycle::spawn_gated_daemon(
        &executable,
        params,
        pty_lifecycle::OperationLaunch {
            control_fd: fd,
            digest: digest.to_string(),
        },
    ) {
        Ok(gated) => gated,
        Err(error) => {
            return decide(
                &mut channel,
                &forking,
                Cause::Refused,
                None,
                Some(error.to_string()),
            );
        }
    };
    let daemon = match ProcessId::of(gated.pid()) {
        Ok(daemon) => daemon,
        Err(error) => {
            gated.abandon();
            return decide(
                &mut channel,
                &forking,
                Cause::Refused,
                None,
                Some(format!("daemon identity: {error}")),
            );
        }
    };
    let mut gated_record = forking.clone();
    gated_record.phase = Phase::DaemonGated;
    gated_record.writer = Writer::of(WriterRole::Producer, daemon.clone());
    gated_record.participants.producer = Some(daemon.clone());
    if let Err(error) = store.write_cas(&op_lock, digest, &forking, gated_record) {
        gated.abandon();
        eprintln!("pty run: cannot hand the writer role to the daemon: {error}");
        return Ok(REFUSED);
    }
    // The daemon's decision runs under this session's creation lock; it is
    // its owner from here, keyed by its pid in the lock record and by its
    // full identity in the operation record.
    if let Err(error) = creation_lock.hand_over(gated.pid()) {
        // The record already names the daemon as writer. Without the lock
        // it must not decide, so it is never released: it exits at the
        // closed gate, and the operation is left for recovery.
        gated.abandon();
        eprintln!("pty run: cannot hand the creation lock to the daemon: {error}");
        return Ok(REFUSED);
    }
    drop(op_lock);
    barrier("daemon-gated");
    // SAFETY: the launcher's copy of the control end; the daemon has its own
    // and reads what is queued.
    unsafe { libc::close(fd) };
    channel.fd = -1;
    match gated.release() {
        Ok(_) => {
            println!("Session \"{name}\" created.");
            Ok(0)
        }
        Err(error) => {
            eprintln!("pty run: {error}");
            Ok(REFUSED)
        }
    }
}
