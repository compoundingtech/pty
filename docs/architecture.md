# Architecture

How the workspace is laid out, and the shape decisions that hold across it.
What differs from the Node `pty`, surface by surface, is in
[parity.md](parity.md); single decisions with their reasons are in
[decisions/](decisions/).

## Crates

One Cargo workspace; every crate is under `crates/`.

- `pty` — the `pty` binary: the CLI (one module per command under
  `src/cli/`), the per-session daemon (`__daemon`), `remote-serve`, and the
  interactive session manager. Its library is the `TerminalHandle` that spawns
  a child or attaches to a daemon.
- `pty-core` — the wire protocol, session registry, locks, events log,
  metadata, names and tags, key/paste/duration/input parsing, and `pty.toml`
  manifests. No terminal emulator, no Zig.
- `pty-client` — the typed operations over a session's socket: list, attach,
  peek and screen reads, send, stats, signal, stop and remove, readiness, and
  the remote dial.
- `pty-lifecycle` — daemon launch and the registry lifecycle operations shared
  by the binary and embedders.
- `pty-spawn` — allocates a pseudo-terminal and starts a child in it. Nothing
  else, so it stays free of the terminal emulator.
- `pty-terminal` — libghostty-backed terminal state: bytes in; screen, cells,
  cursor, modes, graphics, input encoding, query answers and replay
  serialization out. It spawns no process.
- `pty-testkit` — terminal test sessions: spawn a process in a real PTY, feed
  it to libghostty, take screenshots, wait for text, send named keys.
- `pty-tui` — the TUI library on ratatui and crossterm that the session
  manager is built on.
- `pty-conformance` — the black-box suite that runs against any `pty` binary
  named by `PTY_TEST_BIN`, plus the mixed Node and Rust rig. See
  [conformance.md](conformance.md).

## Shape decisions

**The CLI parser is hand-rolled**, one module per command over a shared `Argv`
cursor (`crates/pty/src/cli/argv.rs`). Node's grammar is irregular: `--root`
anywhere, flags-then-ref loops, the `--with-delay` position rule,
`--flag=value` only in `gc`, tokens `list` silently ignores. Its error texts
name exact tokens. Mirroring `cli.ts` loop for loop reproduces all of that; a
declarative parser would fight it. Help text and completion scripts are
vendored from Node rather than generated (`crates/pty/src/cli/help.rs`).

**The daemon runs one actor thread.** Everything that touches the terminal
runs on it. The PTY reader, the child waiter, the listener, each client socket
and the signal handler only send it messages, and timers are deadlines the
loop wakes for with `recv_timeout` (`crates/pty/src/daemon/lifecycle.rs`).
Writing to the terminal is synchronous, so a `SCREEN` cut always reflects every
byte received before it. There is no `Arc<Mutex<Terminal>>`, and the
libghostty `Terminal`, which is not `Send`, never leaves that thread.

Client output queues are bounded by 64 MiB of queued and in-flight buffer
capacity plus 64 bytes of metadata per item, not packet count. A large initial
`SCREEN` and live output can wait behind a socket write without small writes
exhausting an unrelated packet budget. Exceeding the byte budget immediately
closes only that client's socket. The session and its child continue.

The server makes one non-blocking, best-effort attempt to send
`ConnectionError` (wire tag 12, payload `[0]` for `ClientTooSlow`) before
closing a client that exceeds that budget. A frame-wide writer lock prevents
this reason from interleaving with a partially written `SCREEN`; a busy
writer or full socket can prevent delivery. Older clients ignore the
unrecognized frame. Attach and following peek clients treat bare EOF,
including a truncated reason frame, as connection loss and exit non-zero.
Only a received `EXIT` establishes the child's exit status. Intentional
detach still succeeds without ending the child.

Unknown reason payloads in frame 12 produce a generic attachment-rejection
diagnostic. Established socket read errors also report connection loss,
not that the session is missing. Initial connection failures keep their
existing diagnostics. Embedded callers must adopt the updated client;
replacing a server alone does not correct an old client's EOF reporting.
`AttachParams` no longer carries the obsolete remote-error-wording flag;
the peer and summary still identify remote trailer targets.

| Server socket close path | Final signal | Interactive client result |
| --- | --- | --- |
| Outbound memory budget exceeded | Best-effort frame 12, then immediate shutdown | Connection loss, exit 1 |
| Oversize/malformed inbound packet | Socket shutdown without EXIT | Connection loss, exit 1 |
| Writer I/O error | Socket shutdown without EXIT | Connection loss, exit 1 |
| Server shutdown, including fatal accept failure | EXIT only if already delivered | Received child status, otherwise connection loss and exit 1 |
| Reader EOF/error | Client removed; writer drains/ends | No fabricated child status; intentional local detach remains successful |
| Client DETACH | Write half closed; client removed | Detach succeeds without ending the child |
| Child exit | EXIT with the child status | Received child status |

**Embedding handles publish immutable frames only while observed.**
`TerminalHandle::observe_frames()` returns an owned lease; lifecycle
`subscribe()` receivers alone never trigger cell or pixel captures. The
handle's actor publishes an `Arc<Frame>` before `Dirty` when its queue drains,
and checks a 16 ms publication interval after each applied message so a
sustained backlog still advances frames. There is no capture timer thread;
one message, its capture, or scheduling can extend the interval.
`frame()` atomically reads the latest published grid, modes, graphics and
owned image generations without waiting for the actor. It is initially empty
and may be stale without a lease. `request_frame(0)` is an asynchronous
initial/admission barrier after previously queued messages; nonzero requests
capture a consistent history window. Unchanged pixels share an `Arc`, and
retained frames survive image replacement or deletion. The synchronous
snapshot/graphics/image reads remain explicit actor APIs; clipping, texture
allocation and compositing stay with the renderer.

**Locks keep Node's file protocol where the two share a root**: a no-replace
claim, the holder's decimal pid, one stale steal, release by unlink, and the
event lock taken before the creation lock. Rust publishes the lock complete by
hard-linking a finished inode into place, and steals a stale lock only after
confirming the path still names the inode it inspected, so Rust contenders
never both win. See `crates/pty-core/src/registry/lock.rs` and
[hardening.md](hardening.md).

**Terminal query answers are fixed in Rust, not recorded.** DA1, DA2,
XTVERSION and the OSC 10, 11 and 4 color queries answer with Node's constants
through libghostty callbacks (`crates/pty-terminal/src/queries.rs`). A
decision record is only for a difference the command line cannot hide.

**Conformance first.** A behavior is ported by making the conformance suite
state it, confirming the Node binary passes, and then driving the Rust binary
from red to green against the same test.
