# 0006 — Two client waits end promptly instead of running to their timeout

**Status:** accepted

These are deliberate improvements, not gaps. Neither is a parity bug and
neither should be "fixed" later by restoring the Node timing.

**Node behavior.**

1. `queryStats` asks the daemon for a STATUS reply with a 2 second timeout.
   If the daemon closes the connection without answering, Node does not
   notice the close: it waits the whole 2 seconds and then reports a
   timeout (`src/client.ts`, the stats query).
2. `peek -f` (follow) on a session whose socket closes cleanly, with no
   EXIT frame, keeps waiting for output that can never arrive. The command
   hangs until the caller kills it.

**Rust behavior.**

1. `query_stats` treats the close as the answer. It reports
   `Session "<name>" not found or not running.` at once, using the same text
   Node uses for a socket that was never reachable
   (`crates/pty-client/src/lib.rs`).
2. `peek -f` treats a plain close as the end of the stream and exits 0.

**Why.** In both cases the daemon has already told the client everything it
is going to. Waiting out a timer after the connection is gone spends a
caller's time to reach the same conclusion, and in the `peek -f` case it
never reaches a conclusion at all. A supervisor that peeks a session on a
2 second budget pays Node's full timeout for every dead session it meets.

**Client effect.** A caller sees the same result sooner. A script that
relied on `pty stats` taking about two seconds against a closing daemon, or
on `peek -f` blocking forever, sees it return earlier. Nothing that a
caller can read changes: the stats error text is Node's, and `peek -f`
exits 0 the way it does when a session ends normally.

**Test.** `crates/pty-conformance/tests/peek_wait.rs` and
`crates/pty-conformance/tests/stats_cli.rs` cover the outcomes. Neither
needs a gated `_node` / `_rust` pair, because the observable result is the
same on both binaries — only the time taken differs, and the suite does not
assert on a wait it does not want.

**Migration / negotiation.** None.
