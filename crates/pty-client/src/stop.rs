//! Stop a session's daemon and verify what it left behind: the operation
//! behind `pty kill`.
//!
//! node: src/cli.ts:2618-2671 (`cmdKill`)

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pty_core::process_tree::{
    ProcessIdentity, groups_in_tree, members_of_groups, own_process_group, signal_group,
    snapshot_from_table, sweep_groups,
};
use pty_core::proctable::{Answer, LiveIdentity, ProcTable};
use pty_core::registry::{self, SessionStatus};

/// How long the daemon gets to finish its shutdown. It re-flushes the exit
/// record on the way out, so returning early would let a following
/// [`crate::remove()`] race that write.
pub const SHUTDOWN_WAIT: Duration = Duration::from_secs(7);

/// How long the escalation gives a group to answer SIGTERM before it stops
/// asking. A coding agent was measured ignoring SIGTERM for ten seconds, so
/// this grace is a courtesy, not a plan.
const ESCALATE_TERM_WAIT: Duration = Duration::from_millis(2_000);
/// How long to wait after SIGKILL before reporting what is still there.
const ESCALATE_KILL_WAIT: Duration = Duration::from_millis(1_000);

/// Why a session could not be stopped. `Display` is the `pty kill` text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopError {
    /// No session by that name.
    NotFound { name: String },
    /// The session has no running daemon; remove it instead.
    NotRunning { name: String },
    /// SIGTERM could not be sent to the daemon.
    SignalFailed { name: String },
    /// The daemon did not exit within [`SHUTDOWN_WAIT`]. Its socket is left
    /// in place as the evidence of what still holds the session.
    DaemonStillRunning {
        name: String,
        pid: i32,
        socket: PathBuf,
    },
}

impl fmt::Display for StopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StopError::NotFound { name } => write!(f, "Session \"{name}\" not found."),
            StopError::NotRunning { name } => write!(
                f,
                "Session \"{name}\" is not running. Use \"pty rm {name}\" to remove it."
            ),
            StopError::SignalFailed { name } => write!(f, "Failed to kill session \"{name}\"."),
            StopError::DaemonStillRunning { name, pid, socket } => write!(
                f,
                "Failed to kill session \"{name}\": daemon PID {pid} is still running after 7s. \
                 Socket {} may still be owned.",
                socket.display()
            ),
        }
    }
}

impl std::error::Error for StopError {}

/// A stopped daemon, and what its process tree looks like afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stopped {
    pub name: String,
    /// The processes that were in the tree before the signal and are not
    /// verifiably gone now.
    pub aftermath: Aftermath,
    /// `None` when nothing survived the daemon. Otherwise the escalation
    /// signalled the tree's process groups, and these pids outlived its
    /// SIGKILL.
    pub escalated: Option<Vec<i32>>,
    /// The session was `strategy=permanent`. Its `strategy` tag was dropped
    /// first, so `pty gc` does not start it again.
    pub was_permanent: bool,
    /// The `ptyfile` tag of a permanent session: the manifest that will put
    /// the strategy back on the next `pty up`.
    pub ptyfile: Option<String>,
}

impl Stopped {
    /// Did the stop verify that nothing is left?
    ///
    /// **Both halves are required.** [`Aftermath`] only describes the
    /// processes in the pre-signal snapshot, and the snapshot drops anything
    /// whose start token could not be read. A process the sweep found and
    /// could not kill may therefore be absent from `aftermath` entirely.
    /// Reading `aftermath` alone would call a process that just survived
    /// SIGKILL a success.
    pub fn verified_empty(&self) -> bool {
        verified_empty(&self.aftermath, self.escalated.as_deref())
    }
}

/// What the pre-signal snapshot looks like once the daemon has gone.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Aftermath {
    /// The start token still matches, so this is the same process and it is
    /// still running.
    pub survived: Vec<i32>,
    /// The pid has not exited but its start token could not be read. We cannot
    /// tell whether it is the same process or a pid the kernel has reused.
    ///
    /// This case gets its own list rather than joining either side. Folding it
    /// into `survived` would invent a survivor; dropping it would be a failure
    /// to measure reported as an answer.
    pub unknown: Vec<i32>,
}

impl Aftermath {
    pub fn all_gone(&self) -> bool {
        self.survived.is_empty() && self.unknown.is_empty()
    }
}

/// Stop the session `name` under `$PTY_ROOT`: drop a permanent session's
/// `strategy` tag, SIGTERM its daemon, wait up to [`SHUTDOWN_WAIT`] for it to
/// exit, remove the socket, then check every process that was in the tree
/// before the signal and escalate over the tree's process groups when any
/// of them is still there.
///
/// The daemon keeps the session's exit record, so the session reads as
/// exited afterwards; [`crate::remove()`] deletes it.
pub fn stop(name: &str) -> Result<Stopped, StopError> {
    let Some(session) = registry::get_session_by_name(name) else {
        return Err(StopError::NotFound {
            name: name.to_string(),
        });
    };
    let (SessionStatus::Running, Some(pid)) = (session.status, session.pid) else {
        return Err(StopError::NotRunning {
            name: name.to_string(),
        });
    };

    // Drop the `strategy` tag so `pty gc` does not start the session again
    // on its next pass.
    let tags = session.metadata.as_ref().and_then(|m| m.tags.as_ref());
    let was_permanent =
        tags.and_then(|t| t.get("strategy")).map(String::as_str) == Some("permanent");
    if was_permanent {
        let _ = registry::update_tags(name, &Default::default(), &["strategy".to_string()]);
    }
    let ptyfile = was_permanent
        .then(|| tags.and_then(|t| t.get("ptyfile")).cloned())
        .flatten();

    // Take the tree BEFORE the signal. After the daemon exits its children are
    // reparented to init, so the parent links that identify them as this
    // session's processes are gone. This snapshot is the only chance to learn
    // which processes the word "killed" would be a claim about.
    // One table read serves both. The snapshot drops a descendant whose start
    // identity could not be read; the group list does not, because a group
    // needs no identity. So a process the snapshot cannot name is still inside
    // a group this operation will signal.
    let table = ProcTable::read();
    let before = snapshot_from_table(pid, &table);
    let groups = groups_in_tree(pid, &table);

    // SAFETY: kill(2) with a pid from the registry.
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        return Err(StopError::SignalFailed {
            name: name.to_string(),
        });
    }

    if !wait_for_process_exit(pid, SHUTDOWN_WAIT) {
        return Err(StopError::DaemonStillRunning {
            name: name.to_string(),
            pid,
            socket: registry::socket_path(name),
        });
    }
    registry::cleanup_socket(name);
    let mut after = aftermath(&before);
    let mut escalated = None;
    if !after.all_gone() {
        escalated = Some(escalate_over_groups(&groups));
        // Re-measure. The result must describe the machine now, not the
        // signals that were sent at it.
        after = aftermath(&before);
    }
    Ok(Stopped {
        name: name.to_string(),
        aftermath: after,
        escalated,
        was_permanent,
        ptyfile,
    })
}

/// [`stop`] in the registry at `root` instead of `$PTY_ROOT`.
pub fn stop_in(root: &Path, name: &str) -> Result<Stopped, StopError> {
    registry::with_root(root, || stop(name))
}

/// Re-check a snapshot against the live process table.
///
/// `exited` must be `has_process_exited_for_reap` rather than `!pid_alive`.
/// A zombie answers `kill(pid, 0)` and keeps a readable start token, so the
/// two cheaper predicates both call it a survivor. It is a dead process
/// waiting to be reaped, and reporting it as still running would be
/// over-claiming again, only in the other direction. Measured on Linux
/// 2026-09-03: state `Z`, `/proc/<pid>/stat` readable, token unchanged,
/// `kill(pid, 0)` succeeds.
fn aftermath_with(
    before: &[ProcessIdentity],
    read_identity: impl Fn(i32) -> Option<LiveIdentity>,
    exited: impl Fn(i32) -> bool,
) -> Aftermath {
    let mut out = Aftermath::default();
    for id in before {
        if exited(id.pid) {
            continue;
        }
        match read_identity(id.pid) {
            Some(found) if found == id.identity => out.survived.push(id.pid),
            // A different identity is a pid the kernel handed to somebody else.
            Some(_) => {}
            None => out.unknown.push(id.pid),
        }
    }
    out
}

fn aftermath(before: &[ProcessIdentity]) -> Aftermath {
    let table = ProcTable::read();
    aftermath_with(
        before,
        |pid| table.identity(pid).known(),
        // Exited means: the table read, and it said either "not there" or
        // "there but a zombie". An unreadable table says neither, so it does
        // not answer this question and the identity check decides instead.
        |pid| match table.is_running(pid) {
            Answer::Known(running) => !running,
            Answer::NotPresent => true,
            Answer::Unknown(_) => false,
        },
    )
}

/// TERM the groups, wait, KILL what is left, wait, then re-read the process
/// table. Returns the pids still alive in those groups.
fn escalate_over_groups(groups: &[i32]) -> Vec<i32> {
    sweep_groups(
        groups,
        own_process_group(),
        ESCALATE_TERM_WAIT,
        ESCALATE_KILL_WAIT,
        |targets| members_of_groups(targets, &ProcTable::read()),
        signal_group,
    )
}

fn verified_empty(after: &Aftermath, escalated: Option<&[i32]>) -> bool {
    after.all_gone() && escalated.is_none_or(<[i32]>::is_empty)
}

fn wait_for_process_exit(pid: i32, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if !registry::pid_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !registry::pid_alive(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(pid: i32, token: &str) -> ProcessIdentity {
        ProcessIdentity {
            pid,
            identity: LiveIdentity::new(token),
            depth: 1,
        }
    }

    #[test]
    fn a_matching_token_is_a_survivor() {
        let before = vec![id(10, "tok:10")];
        let after = aftermath_with(&before, |_| Some(LiveIdentity::new("tok:10")), |_| false);
        assert_eq!(after.survived, vec![10]);
        assert!(after.unknown.is_empty());
        assert!(!after.all_gone());
    }

    #[test]
    fn a_reused_pid_is_not_a_survivor() {
        let before = vec![id(10, "tok:10")];
        let after = aftermath_with(
            &before,
            |_| Some(LiveIdentity::new("tok:different")),
            |_| false,
        );
        assert!(after.all_gone());
    }

    #[test]
    fn a_gone_process_is_gone() {
        let before = vec![id(10, "tok:10")];
        let after = aftermath_with(&before, |_| None, |_| true);
        assert!(after.all_gone());
    }

    /// The whole point of the `unknown` list: a pid we can see but cannot
    /// identify is reported as undecided, never silently as dead.
    #[test]
    fn an_unreadable_token_on_a_live_pid_is_undecided() {
        let before = vec![id(10, "tok:10")];
        let after = aftermath_with(&before, |_| None, |_| false);
        assert!(after.survived.is_empty());
        assert_eq!(after.unknown, vec![10]);
        assert!(!after.all_gone());
    }

    #[test]
    fn an_empty_snapshot_is_all_gone() {
        assert!(aftermath_with(&[], |_| None, |_| false).all_gone());
    }

    /// The mocked cases above prove the branching. This one proves the
    /// branching is about real processes: it spawns one, classifies it with
    /// the real token reader and the real liveness check, kills it, and
    /// classifies it again.
    #[test]
    fn a_real_process_is_classified_from_the_real_process_table() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;
        let table = ProcTable::read();
        let identity = table
            .identity(pid)
            .known()
            .expect("live pid has an identity");
        let before = vec![ProcessIdentity {
            pid,
            identity,
            depth: 1,
        }];

        let alive = aftermath(&before);
        assert_eq!(
            alive.survived,
            vec![pid],
            "a running process reads as a survivor"
        );
        assert!(alive.unknown.is_empty());

        let _ = child.kill();
        let _ = child.wait();

        let dead = aftermath(&before);
        assert!(
            dead.all_gone(),
            "a reaped process must not read as a survivor, got {dead:?}"
        );
    }

    /// An unreaped child must not be reported as a survivor.
    ///
    /// **The two platforms get there differently.** On Linux the corpse keeps
    /// a row and a matching identity, so the identity alone cannot decide and
    /// the exit check does. On macOS libproc stops listing it at once, so it
    /// is simply absent. The test asserts the conclusion rather than either
    /// mechanism; an earlier version asserted the Linux one and failed on a
    /// real Mac.
    #[test]
    fn a_real_zombie_is_not_a_survivor() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id() as i32;
        let table = ProcTable::read();
        let identity = table
            .identity(pid)
            .known()
            .expect("live pid has an identity");
        let before = vec![ProcessIdentity {
            pid,
            identity: identity.clone(),
            depth: 1,
        }];

        // Let it exit. It stays a zombie because nothing has waited on it.
        for _ in 0..200 {
            if registry::has_process_exited_for_reap(pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // On Linux the corpse is still listed with its identity intact, which
        // is exactly why the identity check alone is not enough. On macOS it
        // is not listed at all. Both are fine; neither is asserted.
        let after = aftermath(&before);
        assert!(
            after.all_gone(),
            "a zombie must not be reported, got {after:?}"
        );

        let _ = child.wait();
    }

    /// A process the sweep could not kill need not appear in `Aftermath` at
    /// all: the snapshot drops anything whose start token could not be read,
    /// and a process spawned after the snapshot was never in it. Reading the
    /// snapshot alone would call a process that just survived SIGKILL a
    /// success.
    #[test]
    fn a_survivor_of_the_escalation_is_never_a_verified_empty_tree() {
        let clean = Aftermath::default();
        assert!(
            clean.all_gone(),
            "precondition: the snapshot says nothing is left"
        );
        assert!(
            !verified_empty(&clean, Some(&[4321])),
            "a process that survived SIGKILL to its group must not read as success"
        );
        assert!(
            verified_empty(&clean, Some(&[])),
            "an escalation that cleared everything is success"
        );
        assert!(
            verified_empty(&clean, None),
            "no escalation needed is success"
        );
        assert!(
            !verified_empty(
                &Aftermath {
                    survived: vec![1],
                    unknown: vec![]
                },
                Some(&[])
            ),
            "the snapshot still decides when the sweep found nothing"
        );
    }
}
