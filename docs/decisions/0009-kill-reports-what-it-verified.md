# 0009 — `pty kill` reports what it verified

**Status:** accepted

**Node behavior.** `pty kill` sends SIGTERM to the daemon, waits up to seven
seconds for that one pid to disappear, and prints `Session "X" killed.` It
never looks at the child or at anything the child started. The word "killed"
is therefore a claim about a session, made on evidence about a daemon.

**Rust behavior.** `pty kill` takes a snapshot of the daemon's process tree
before it sends the signal. After the daemon exits, it re-checks that snapshot.
It prints `Session "X" killed.` only when every process in it is gone.
Otherwise it prints `Session "X" daemon stopped.` on standard output, which is
the part it did verify, and names the survivors on standard error.

**Why.** Decision 0008 gave the daemon a place to record what it could not kill, and
ended by saying it added a record and not a guarantee. The record still did not
reach the person running the command. This closes that: the command itself
checks, and its own output carries the answer.

**How it was found.** On 2026-09-02 a coding agent survived a `pty kill` on a
Mac. The operator read `killed`, started the session again, and ended with two
processes writing to one 14.7 MB transcript. The word is what made the second
start look reasonable.

**The snapshot is taken before the signal, and that is the point.** Once the
daemon exits, its children reparent away and the links that identify them as
this session's processes are gone. The pre-kill snapshot is also taken at a
calm moment, while the daemon's own snapshot is taken during shutdown under
whatever load the machine is carrying. So the command's view can be wider than
the daemon's, which is exactly the case where the daemon's teardown silently
skipped a process.

**Three outcomes, not two.** A pid in the snapshot is reported as surviving
only when its start token still matches. A different token is a pid the kernel
reissued. A token that cannot be read at all, on a process that has not exited,
is reported separately as undecided. Folding that third case into either of the
others would repeat the defect this record exists to remove.

**A zombie is not a survivor.** An unreaped process answers `kill(pid, 0)` and
keeps a readable start token, so the two obvious predicates both call it alive.
The check uses `has_process_exited_for_reap`, which reads the process state.
Measured on Linux on 2026-09-03: state `Z`, `/proc/<pid>/stat` readable, token
unchanged, `kill(pid, 0)` succeeds.

**This adds no signals.** The command sends the same single SIGTERM it always
sent. It kills nothing extra and waits no longer. Widening the kill is a
separate question, and an unverified wider signal would leave the same orphans.

**Exit code.** Non-zero when anything survived, and non-zero when a start token
could not be read so the outcome is undecided. The first was decided on
2026-09-03: "kill that doesn't actually kill everything is def a non-zero."
The second was decided on the grounds that "I could not confirm the
tree is empty" is not success and a caller that reads 0 as done would be wrong.

**This is a compatibility break.** A script that runs `pty kill` and checks the
status will now fail where it used to pass. That is the point rather than a side
effect: it was passing on a false success. The fix for such a caller is to stop
treating an unverified kill as a completed one.

**What is tested and what is not.** The classification is pinned by unit tests,
including two that run against the real process table: one live process and one
real zombie. The end-to-end contract that `killed` implies an empty tree is
pinned by a conformance test that both binaries run. **The survivor branch is
not proven end to end.** Reaching it needs a descendant that outlives a SIGKILL,
or a start token that cannot be read while the process is alive, and neither can
be manufactured on Linux without fault injection this change does not add.

**What this does not fix.** A process that leaves the daemon's tree before the
snapshot is invisible to the command, exactly as it is invisible to the daemon.
A descendant whose start token cannot be read is still dropped from the
teardown snapshot by `snapshot_from_listing`, so it is never signalled; this
record makes such a process visible in the report, and does not make it die.
Those remain open and are tracked separately.
