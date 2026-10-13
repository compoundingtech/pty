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
   The session's metadata carries `launchOperation: {digest, phase}`, with
   phase `startup` until the transfer and `transferred` after it.

Until transfer every process involved is one of pty's own, and each carries
`--launch-operation <h>` in its argv, inherited across fork. `pty operation
settle` therefore reads the record, the process table with argv, and the
registry once each under a shared lock, and answers `closed`, `transferred`,
`live`, `retire-required` or `unknown`, naming the component and its evidence.
It never signals anything. `pty operation retire` takes over from a writer
that is verifiably gone, tears down only recorded identities and only the
recorded generation's row, and returns the same snapshot. `pty capabilities
--json` advertises `launch-operation` version 1.

Without `--operation-fd` nothing changes: no record, no held child, no
`launchOperation` field.

**Why.** An owner that replaces or retries a session must know whether the
previous launch's command ever ran. Holding the command until transfer makes
that question answerable on every platform: before transfer there is no user
code and no user descendant, only pty processes that are recorded, marked, or
both. A cancel before transfer is complete when those are gone.

**Details the contract left open.** Recorded here so a consumer can rely on
them:

- Refusal and rejection codes beyond the contract's table: `not-held` (a
  transfer before the child is held, or during retirement), `already-decided`
  (a second cancel), and for the launcher `digest-mismatch`,
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
  `execve`, as today's spawned child does.

**Tests.** `crates/pty/tests/launch_operation.rs` plays the owner. Debug
builds honor `PTY_TEST_OPERATION_BARRIERS=<dir>`: a barrier writes
`<name>.reached` and, when the test created `<name>.hold`, waits for
`<name>.release`. The tests cancel, close or kill a participant at every
barrier and check the frames, the record and the exact `settle` verdict; they
cover descriptor validation, a stopped child, a group stop, the daemon's
handlers, a replaced binary, decoys with the same and another digest,
unreadable evidence (`PTY_TEST_PROCESS_TABLE`, `PTY_TEST_ARGV_UNREADABLE`) and
a same-name replacement surviving `retire`. One test runs the same probe
through today's spawn and through a transfer and requires the environment,
umask, working directory, session, process group, terminal, descriptors,
signal mask and dispositions to match. Each of the queued-cancel rule,
descriptor cleanup, disposition reset, marker match and ticket commit was
reverted once to see its test fail.
