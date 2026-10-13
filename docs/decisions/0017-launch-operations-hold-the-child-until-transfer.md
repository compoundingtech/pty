# 0017 — Launch operations hold the child until transfer

**Status:** accepted

**Node behavior.** None. `pty run -d` publishes the session and starts its
command; a launcher that is cancelled or dies part-way leaves whatever it got
to. Nothing records which launch a session came from, so an owner cannot prove
that a cancelled launch never ran its command.

**Rust behavior.** A launch operation is a launch an owner can cancel before
the command starts, and settle afterwards to a positive answer.

1. The owner reserves it: `pty operation reserve --json` reads the envelope (a
   one-line JSON declaration with `operationId`, `runtimeId`, the expected
   prior incarnation and bindings) on stdin and creates
   `<root>/operations/<h>.json` exclusively, `<h>` being the SHA-256 of the
   `operationId`. Records are never deleted, so an `operationId` launches at
   most once.
2. The owner launches it:
   `pty run -d --operation-fd N --launch-operation <h> --id <runtimeId> -- …`,
   with `N` a connected `AF_UNIX` stream socket carrying the envelope as its
   first line. The launcher claims the reservation, looks once more for a
   queued cancel, and forks `pty __daemon --launch-operation <h>` held at a
   gate. It records the daemon and hands it the writer role and the session's
   creation lock before opening the gate.
3. The daemon makes one decision under the operation lock and the creation
   lock: a queued cancel, end of file or malformed input wins; then an
   occupied name or a predecessor that is not exactly the expected ended
   incarnation cancels; otherwise it launches. Publication and the child's
   fork are effects behind tickets the daemon's actor commits, so a cancel
   either revokes them before they happen or waits for them to finish.
4. The child is a native fork of the daemon. It resets its signals, closes
   every descriptor but its gate, takes the terminal, applies the umask and
   working directory, acknowledges `prepared`, and blocks. `ready` means
   published, prepared and held, not that the command started.
5. `transfer` commits `transferred` to the record, then writes the release
   byte, and the child calls `execve` with today's exact argv and environment.
   A transfer accepted while the child is held but before `ready` is
   committed right after `ready`, so the daemon is already serving. The
   session's metadata carries `launchOperation: {digest, phase}`, with phase
   `startup` until the transfer and `transferred` after it.
6. A cancel, the owner's end of file, or a startup failure while launched
   first records `retirement.requested` durably. Only then is the cause
   acknowledged, and only once every committed ticket has completed are the
   gate shut, the child ended and the row unwound. If any record write does
   not land, the daemon stops: no further effect and no positive frame. The
   held child reads end of file on its gate and leaves without exec, and the
   operation is left to `settle` and `retire`.

Until transfer every process involved is one of pty's own, and each carries
`--launch-operation <h>` in its argv, inherited across fork. `pty operation
settle` therefore reads the record, the process table with argv, and the
registry once each under a shared lock, and answers `closed`, `transferred`,
`live`, `retire-required` or `unknown`, naming the component and its evidence.
It never signals and never creates anything. `noFurtherEffects` is true only
when every component is verified, the registry included: a row that is
unreadable, or live and carrying this digest, can still have effects. On
macOS a pid listing that fills its buffer is read again with more room, and
one that stays full makes the table and the argv scan incomplete, never
whole. `pty operation retire` checks
the exact operation, envelope and incarnation first. It refuses while the
writer or any recorded launcher, producer or daemon is alive or unprovable,
and resumes its own interrupted request. It tears down only the recorded
child and only the recorded generation's row, then returns the same
snapshot. Liveness is per boot: without a readable boot identity every
recorded identity is unknown, and nothing is signalled. `pty capabilities
--json` advertises `launch-operation` version 1.

Without `--operation-fd` nothing changes: no record, no held child, no
`launchOperation` field.

**Why.** An owner that replaces or retries a session must know whether the
previous launch's command ever ran. Holding the command until transfer makes
that question answerable on every platform: before transfer there is no user
code and no user descendant, only pty processes that are recorded, marked, or
both. A cancel before transfer is complete when those are gone.

**Signal authority.**

- The daemon ends its own child only through the session substrate, which
  holds its reap lock across the signal, so the pid cannot have been reused.
  It waits for that exit on a pidfd (Linux) or a kqueue (macOS).
- Recovery, which is not the child's parent, pins the process first and
  checks the pinned process is the recorded identity, then sends `SIGKILL`
  through the pin:
  - **Linux:** a `pidfd`; a signal through it cannot reach a later holder of
    the pid.
  - **macOS:** an audit token from the task name port; the kernel delivers
    `proc_signal_with_audittoken` only while the token's pid version still
    names the same process.
- **Where neither exists, nothing is signalled.** That covers a Linux kernel
  without `pidfd_open`, a macOS without the call, and a task port that is
  refused. The child is reported unknown, the retirement incomplete, and the
  operation stays unclosed. No raw pid is ever signalled.
- A stopped held child that leads its own session is ended by job control
  when its daemon dies: the terminal hangs up, and the kernel sends it
  `SIGHUP` and `SIGCONT`. Its `SIGHUP` is at its default. A child stopped
  before it made its own session is left stopped, and recovery ends it.

**The operation lock** is a POSIX record lock (`fcntl`), not `flock`. An
`flock` belongs to the open file description and so is shared by a forked
child. A held child stopped before shedding its descriptors would then keep a
dead daemon's lock for ever. A record lock belongs to the process and is never
inherited by a fork. A reader that finds the lock held names its holder
(`F_GETLK`).

**Descriptors.** The launcher's `pre_exec` marks every descriptor from 3 up
close-on-exec, then clears the flag on exactly the config, readiness, control
and gate descriptors. A launcher handed other descriptors, including a stray
copy of the owner's own end, cannot pass them to the daemon. The gate pipe,
the held child's gate and the control wake-up pipe are close-on-exec from
creation on Linux.

**Details the contract left open.** Recorded here so a consumer can rely on
them:

- Refusal and rejection codes beyond the contract's table:
  - `not-held`: a transfer before the child is held, during retirement, or
    one that lost to a cancel or end of file before its commit;
  - `already-decided`: a second cancel, or a second transfer;
  - `already-transferred`: any frame after the commit;
  - for retire, `live-participant`, `participant-unknown` and
    `record-write`;
  - for the launcher, `digest-mismatch`,
  `runtime-id-mismatch`, `session-busy`, `session-unavailable`,
  `operation-unavailable` and `identity-unavailable`. A launcher that cannot
  read a valid envelope refuses with `envelope-invalid` and null operation
  fields. A launcher that meets a held operation lock reports `not-claimable`
  with the state it read.
- Operation mode requires `-d` and `--id`, and refuses `-a` and
  `--lifecycle-tag`. It does not carry settings over from an ended session of
  the same name.
- Exit status: 0 when a verb printed its answer, 1 for a refusal, 64 for a
  usage error or an invalid descriptor (nothing read, nothing recorded). The
  launcher exits 0 only after `ready`; its status is informational.
- The launcher and the daemon discard unread owner input before closing the
  control socket, so the owner reads every frame and then end of file, never
  a reset.
- glibc keeps signals 32 and 33 for itself and refuses `sigaction` on them, so
  the held child keeps the C library's own handlers for those two until its
  `execve`, as today's spawned child does. Every other disposition is what
  an `exec` from today's spawn leaves: caught signals at their default, and
  ignored ones still ignored, except `SIGCHLD`, `SIGHUP`, `SIGINT`,
  `SIGQUIT`, `SIGTERM`, `SIGALRM` and `SIGPIPE`, which are reset.
- `expectedIncarnation` and a transfer's incarnation id are decimal digits, a
  colon, and a non-empty rest, inside the limits of an `S`. They are compared
  by exact bytes with the registry's own incarnation text.
- `reserve` creates `<h>.lock` together with the record. A missing lock reads
  as a missing operation.
- The argv scan reports a process of the operation's user whose argv cannot
  be read, including one caught exiting, as unreadable, and the verdict as
  unknown. It does not retry. A caller wanting a later answer settles again
  after that process exits; the tests do exactly that.
- The argv scan's domain is processes whose real and effective uid are both
  the operation's. No participant changes uid before its release. On macOS
  a setuid process such as `login` refuses its argv to the user; inside the
  domain it would make every scan unknown.
- A process may not open the operation lock file to observe it (settle, or
  the holder query) while it holds the write lock. Closing that descriptor
  would drop its own record lock, so the call is refused instead.

**Residuals, stated plainly.**

- A held child stopped by someone else in the instant between its fork and
  the daemon recording it, whose daemon then dies, is never recorded. It
  stays stopped. Settle reports the operation `live` (the marker scan names
  its pid) and never closes it. Recovery signals only recorded identities,
  so a person must end that process.
- Where identity-safe signalling is unavailable (see above), recovery sends
  nothing and the operation stays unclosed with a live child, until a
  person ends it.
- The daemon handles SIGTERM and SIGINT from its start: before the transfer
  they retire the operation.

**Tests.** `crates/pty/tests/launch_operation.rs` plays the owner. Debug
builds honor `PTY_TEST_OPERATION_BARRIERS=<dir>`: a barrier writes
`<name>.reached` and, when the test created `<name>.hold`, waits for
`<name>.release`. The launcher and the startup worker block at their
barriers. The daemon's actor never blocks: at `before-decision` and at the
three transfer barriers it parks the step and keeps answering, so the tests
see decisions, rejections and retirements arrive while the barrier holds.
`PTY_TEST_RECORD_WRITE_FAIL=<transition>` fails one record write, and
`PTY_TEST_BOOT_ID=unavailable` hides the boot identity, and
`PTY_TEST_EXACT_SIGNAL=unsupported` makes identity-safe signalling
unavailable. `PTY_TEST_HELD_CHILD=stop-after-fork|stop-before-ack` stops the
held child at either end of its preparation. The tests cancel, close or kill a participant at every
barrier and check the frames, the record and the exact `settle` verdict; they
cover descriptor validation, a stopped child, a group stop, the daemon's
handlers, a replaced binary, decoys with the same and another digest,
unreadable evidence (`PTY_TEST_PROCESS_TABLE`, `PTY_TEST_ARGV_UNREADABLE`) and
a same-name replacement surviving `retire`. One test runs the same probe
through today's spawn and through a transfer and requires the environment,
umask, working directory, session, process group, terminal, descriptors,
signal mask and dispositions to match. Each of these was reverted once to see its test fail:
- the queued-cancel rule;
- the held child's descriptor cleanup and its disposition reset;
- the marker match and the ticket commit;
- the launcher's descriptor sweep;
- the deferral of a transfer to `ready`;
- the request-before-effect rule;
- the boot-identity rule;
- retire's tuple check;
- the refusal to signal without a pidfd;
- the record lock.
