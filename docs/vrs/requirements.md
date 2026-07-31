# pty requirements

## Context

These requirements define the durable contract of the `pty` project described
in the [README](../../README.md): persistent terminal sessions and the reusable
libraries built on the same terminal model. The README remains the product
purpose and user guide; this tree owns the testable system constraints. There
is intentionally no `vision.md` in this tree.

Child requirements refine this contract by scoped ID:

- [session runtime](./01-session-runtime/requirements.md)
- [session stream](./02-session-stream/requirements.md)
- [registry](./03-registry/requirements.md)
- [surfaces](./04-surfaces/requirements.md)

## Assumptions

- **PTY-A01 Unix host:** Supported hosts provide Unix PTYs, Unix-domain sockets,
  process signals, and atomic same-filesystem rename. The supported products are
  macOS and Linux.
- **PTY-A02 Trusted user boundary:** A registry belongs to one trusted OS user.
  Socket and filesystem permissions are the access boundary; readonly clients
  are a behavior mode, not an authorization mechanism.
- **PTY-A03 Terminal byte stream:** A child process expresses terminal state as
  ordered bytes and terminal control sequences. Reconstructing that state
  requires a terminal emulator, not line-oriented logging.

## Acceptable tradeoffs

- **PTY-T01 Per-session daemon:** Each persistent session pays the resource cost
  of an independent daemon in exchange for failure isolation and no central
  lifetime owner.
- **PTY-T02 Shared grid:** All writable clients share one effective PTY grid.
  The smallest requested row and column dimensions win, preserving a complete
  view for every writable client at the cost of reducing larger clients.
- **PTY-T03 Pre-1.0 evolution:** Public storage and package APIs may change
  before 1.0 when the documented compatibility tier permits it. Changes remain
  explicit and version-pinned consumers can retain the old contract.

## Requirements

### Must preserve sessions independently of observers

- **PTY-R01 — Persistent execution.** A session's child process and terminal state
  continue independently of the client that created, attached to, or detached
  from it.
- **PTY-R02 — Isolated failure domains.** Failure or replacement of one session,
  client, or control-plane invocation does not implicitly terminate unrelated
  sessions.

### Must preserve terminal meaning

- **PTY-R03 — Reconstructable terminal.** A newly attached client can reconstruct
  a causally valid terminal state, including style, cursor, modes, alternate
  screen, scrollback, and subsequent output, without a byte-loss or reordering
  window.
- **PTY-R04 — Deterministic shared geometry.** Concurrent writable clients produce
  one explicit effective geometry, and every reconstructing client learns that
  geometry in causal order with the terminal state it describes. Readonly
  observation does not alter it.

### Must make lifecycle and state inspectable

- **PTY-R05 — Durable launch and lifecycle.** A preserved session retains enough
  launch definition for an equivalent explicit or policy-driven restart, while
  explicit removal and cleanup cannot delete a replacement generation.
- **PTY-R06 — Observational state.** Callers can inspect session identity,
  lifecycle, metadata, events, clients, resources, and effective geometry
  without attaching or mutating the session.
- **PTY-R07 — Explicit isolation and identity.** Registry selection and stable
  session identity are explicit. Presentation labels and tags do not silently
  become durable identity or hard isolation boundaries.

### Must compose through supported surfaces

- **PTY-R08 — Contract-equivalent surfaces.** The CLI, exported libraries,
  testing API, local socket transport, machine attach stream, and remote route
  preserve the same session, stream, geometry, and lifecycle contracts where
  they expose those capabilities.
- **PTY-R09 — Bounded compatibility.** Protocol and storage readers reject
  unbounded or structurally invalid input, while documented extension and
  compatibility paths preserve older clients and metadata where safe.
