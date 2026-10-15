# 0010 — `pty kill` finishes the job

**Status:** accepted

**Node behavior.** The daemon tears down the child's process tree on its way
out, with exact per-pid TERM then KILL signals. `pty kill` waits for the daemon
and then returns. Whatever the daemon did not manage is nobody's work after
that.

**Rust behavior.** After the daemon has gone, `pty kill` re-reads the process
table. If anything from its pre-kill snapshot is still alive, it pins and signals
those exact processes, waits, escalates to SIGKILL, then reads the table again
and reports what is still there. It exits non-zero unless it verified the tree
is empty.

**Why the command and not the daemon.** The daemon does its teardown inside a
process that is trying to die, so the teardown races its own exit. The command
outlives the daemon, which puts it in the only position from which the job can
be finished.

**Identity is required before teardown.** A retained PID and socket do not
prove ownership: reboot or PID reuse can bind that number to an unrelated
process (or, on Linux, another process's thread). Before taking the tree
snapshot or signalling, `kill` independently checks matching recorded boot
identity and start token on Linux and macOS, plus thread-group leadership on
Linux. The registry generation alone is not proof. Missing boot identity or
start token is Unknown, never a legacy Match, and grants no signal authority.
Unreadable identity also refuses teardown.

**Unknown is not gone.** Legacy live records without a boot id, and records
whose evidence is transiently unreadable, stay listed as `running` with their
named PID. The additive `identity` list JSON/remote field preserves the separate
ownership result; `running` is not signal or cleanup authority. Such legacy
sessions cannot be stopped through `pty kill` or `stop_in` until a new daemon
publishes complete identity evidence. They cannot be removed or replaced on
the strength of that conservative projection either. Readable differing start
tokens independently prove Mismatch before boot evidence is required.
An explicit `exitedAt` or `exitCode` keeps the listing `exited`, including an
Unknown owner; its named PID and identity remain available, and Unknown still
refuses removal. Legacy metadata with no named PID and no PID/socket companion
is Gone and can be removed; a malformed or unreadable companion is not absence.
Listing, removal, GC, name replacement and exact-generation evidence all use
the same companion-aware identity check for these legacy records.

**The existing `Exact` abstraction pins the owner.** The client uses
`operation::exact::Exact` rather than a separate daemon pinning implementation.
It pins with a pidfd on Linux or an audit token on macOS, rechecks recorded
identity after pinning, sends SIGTERM through that exact-owner capability, and
waits for that process's exit. A reused numeric PID cannot redirect the signal
or hold the wait open. Unavailable or refused pinning fails closed, including
Linux kernels without pidfds and macOS without the audit-token signalling
capability; there is no raw-PID compatibility fallback.

**Process groups are not signal authority.** The pre-signal tree snapshot
records exact process identities. TERM and KILL escalation target individually
pinned snapshot owners through `Exact`, not raw group signals whose numeric
PGID can be reused between an ownership check and delivery. Unknown identities,
newly joined processes, or reused PIDs cannot broaden the kill. Processes that
cannot be pinned remain in the report and cannot count as verified cleanup.

**Socket cleanup is generation-owned.** After waiting, the client removes
socket/PID sidecars only under the creation lock and with the original
generation and PID owner. A busy lock or a same-name replacement refuses
cleanup and returns an error without unlinking the replacement's socket.

**GC uses the same authority.** Parent-orphan decisions and permanent
reconciliation require proven loss of the old owner, not a failed PID read.
Gone and Mismatch authorize generation-fenced, signal-free retirement or
permanent respawn; only Match authorizes signalling. Unknown refuses both.
GC's lock-held reap shares the client's identity-proven daemon signal
capability and waits on its pinned process after releasing locks. Reachable
sockets never supply independent signal authority. Runtime debris without
recorded identity is retained; malformed metadata can be reclaimed only when
no socket or PID sidecars accompany it.
Layout-tag pruning also requires Match; an Unknown row's conservative Running
projection grants no authority to change its metadata, even in a dry-run plan.

**Calendar time is not boot identity.** Startup retirement never compares
`createdAt` with `kern.boottime` or Linux `btime`: these dates can move when
the clock is stepped. A Gone record without a stable recorded boot identity
keeps its history, however old its calendar date looks. The legacy
`darwin:{ sec = S, usec = U }` value cannot prove a boot mismatch either.
macOS requires the stable boot-session UUID; legacy values stay Unknown
unless a different start token independently proves loss.

**Removal does not wait on reused PIDs.** `pty rm` skips daemon waiting when
Gone or Mismatch already proves the old incarnation is lost. For a matching
exited record it waits by repeatedly checking that recorded identity, not raw
PID existence. Unknown refuses removal; cleanup retains its existing
generation/PID ownership fence.

**The daemon's own group is not a target.** Measured on Linux for both tools on
2026-09-03: the pty child calls `setsid`, so it starts a new session and a new
group, and the daemon sits alone in its own group. Signalling the daemon's group
would reach the daemon and nothing else. The child's group held exactly the
child and its descendants in that observation, but that snapshot does not
authorize a later raw group signal; the sweep pins each exact owner.

**A zombie is not a survivor.** `ps` lists a zombie with its process group, so
counting it makes the sweep report a process that has already exited. The state
column is read for this reason. Found by a test against a real process group,
not by inspection.

**One TERM and hope is known not to work.** A coding agent was measured ignoring
SIGTERM for ten seconds on 2026-09-02. The sweep therefore escalates, and the
escalation is verified rather than assumed.

**Exit code.** Set by [decision 0009], which this record does not change: it is
non-zero when anything survived and when the outcome is undecided. Escalation
makes the zero mean more, because the command now clears what it found rather
than only naming it.

**No opt-out flag.** `kill` kills. An opt-out was considered and left out.

**What this does not reach.** A descendant that calls `setsid` leaves the
session and the group. Neither the tree walk nor the group sweep can see it,
and this record does not change that. A process group created after the
snapshot, by a process that had already left the tree, is likewise unreachable.

**What is tested and what is not.** Client unit regressions fence reused,
unknown, newly joined, and incompletely observed group membership. A Linux
regression owns a sacrificial sleep child, binds a stale socket, and records
the child's PID with an old start token: inventory must not call it running,
and stop must refuse without signalling it. Direct handle tests independently
prove refusal of mismatched and missing identity even without inventory.
The base-compatible stop suite covers token reuse, a nonleader thread, boot
mismatch, missing identity and a matching owner. Unreadable-boot injection
tests run only in debug builds, where `PTY_TEST_BOOT_ID` exists; release tests
use real kernel evidence and do not treat a disabled injection as unreadable.
The sweep is also pinned against a **real process group that ignores SIGTERM**,
which is checked against the process table afterwards rather than against the
signals sent. **The path from
`pty kill` to a survivor is not pinned end to end**, because a survivor that
`pty kill` alone produces cannot be manufactured on Linux: the daemon's exact
teardown succeeds when start tokens are readable, and the terminal hangup clears
the foreground group for free.
