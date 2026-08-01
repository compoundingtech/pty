# pty requirements

## Context

The [README](../../README.md) is the concise purpose and user guide for `pty`.
These requirements define its durable, testable system constraints. The
implementation contract and validation map live in [spec.md](./spec.md).

## Assumptions

- **A01 Unix host:** Supported hosts provide Unix PTYs, Unix-domain sockets,
  process signals, and atomic same-filesystem rename. The supported products are
  macOS and Linux.
- **A02 Trusted user boundary:** One registry belongs to one trusted OS user.
  Filesystem permissions are the access boundary; readonly mode is behavior,
  not authorization.
- **A03 Terminal semantics:** Child output is an ordered terminal byte stream.
  Reconstructing it requires a terminal emulator rather than line-oriented logs.

## Acceptable tradeoffs

- **T01 Per-session daemon:** Each session pays for an independent daemon in
  exchange for client-independent lifetime and failure isolation.
- **T02 Shared grid:** All writable-attached clients share the minimum requested
  rows and minimum requested columns so every attached writer can represent the
  complete grid.
- **T03 Pre-1.0 compatibility:** Public storage and package APIs may evolve
  before 1.0, but readers remain bounded and documented compatibility tiers are
  preserved deliberately.

## Requirements

### Must preserve runtime meaning

- **R01 Independent session lifetime:** A session child and terminal state
  continue independently of creating, attached, detached, or observing clients;
  failure of one session or client does not implicitly terminate another.
- **R02 Durable launch context:** Initial launch, explicit restart, and
  policy-driven respawn preserve command, arguments, working directory, initial
  geometry, lifetime policy, labels, tags, and child-environment policy.
  Exact-environment mode is exclusive with inherited/isolate policy; inherited
  removals precede assignments. Ordinary assigned values, including an empty
  `NO_COLOR`, remain exact. `PTY_SESSION` and its generation token are
  runtime-owned; absent or empty `TERM` selects `xterm-256color`, while a
  nonempty terminal name is preserved. Historical metadata without removals
  retains ambient-inheritance behavior.
- **R03 Ordered, generation-safe lifecycle:** Child output is drained before
  exit is finalized. Restart, permanent reconciliation, abandonment, explicit
  removal, and cleanup follow explicit policy and cannot mutate or delete a
  replacement generation. Recovery after external registry unlink rebinds the
  same supporting daemon and child without signaling, restarting, relaunching,
  duplicating the provider, or disconnecting existing clients.

### Must reconstruct one shared terminal

- **R04 Ordered reconstruction:** Every valid attach or recognized peek that
  emits terminal state sends effective geometry, exactly one screen baseline,
  then post-cut data and at most one process exit in source order. A later mode
  request supersedes an unfinished generation; reconnect starts a new one.
- **R05 Replaceable client roles:** A fresh command socket accepts input and
  status requests but starts no screen baseline, has no geometry membership,
  and cannot resize. A complete `ATTACH` makes it writable-attached, retaining
  input and status capabilities while installing requested geometry, enabling
  resize, and starting ordered baseline synchronization. A recognized `PEEK`
  makes it readonly and removes its geometry constraint. A malformed attach
  changes neither role nor synchronization generation.
- **R06 Deterministic geometry:** Effective rows and columns are the independent
  minima requested by writable-attached clients. Attach, resize, and disconnect
  recompute them; readonly observation never constrains them. Geometry changes
  are visible before terminal bytes produced for the new size.
- **R07 Bounded stream protocol:** Packets are length-delimited, fragmented
  input is reassembled, oversized input is rejected without unbounded
  buffering, and reconnect or unsupported capability failure is explicit.
  Machine admission verifies the exact stable id and generation on the same
  connection it attaches; rejection causes no attach, resize, or redraw
  mutation. Capability support is negotiated behaviorally rather than inferred
  from version strings, record shapes, separate preflight observations, or
  timeouts.
- **R08 Machine attach stream:** Machine attach is an exclusively framed,
  bidirectional stdin/stdout protocol distinct from interactive attach. One
  `OPEN` receives either exactly one `ADMISSION_FAILURE` then EOF, or `HELLO`,
  an optional atomic `READY` baseline with ordered updates, exactly one typed
  bound-stream outcome, then EOF. `READY` contains effective
  geometry, the complete revisioned child-input mode snapshot, and screen
  bytes. `INPUT` framing preserves its byte payload; the child boundary accepts
  valid UTF-8 plus all C0 and escape bytes without replacement or reserved-byte
  interpretation and rejects invalid UTF-8 explicitly. Detach is its own frame,
  never inferred from input. EOF before a typed outcome is truncation, and
  administrative destruction is truncation unless process exit was observed.

### Must expose durable state through one behavioral core

- **R09 Stable, inspectable registry:** Registry root and filename-safe stable
  id are explicit. Display names and tags are presentation metadata. Inventory
  and status expose lifecycle, clients, requested/effective geometry, resources,
  metadata, and any live-recovery capability without attaching or mutating the
  session.
- **R10 Durable, compatible records:** Metadata retains the launch and lifecycle
  fields needed for inspection and restart, preserves unknown fields on update,
  and uses generation-aware atomic replacement. Events are external-readable
  JSONL records with bounded retention. Readers reject structurally invalid or
  unbounded input while retaining documented legacy fallbacks. Live recovery
  authenticates the current private registry root, recovery directory,
  generation, daemon process, launch identity, and metadata revision; it fails
  closed on stale, replayed, interrupted-publication, tampered, wrong-root, or
  path-replacement attempts while allowing an authenticated interrupted lock to
  resume.
- **R11 Equivalent supported surfaces:** CLI commands, exported client/server/
  protocol/testing APIs, the shipped package entrypoint, local transport, and
  remote routing preserve the applicable runtime, stream, geometry, registry,
  and lifecycle contracts. A surface rejects unsupported capabilities instead
  of silently weakening them; machine attach never falls back to an older
  protocol after v2 admission fails. Tests use real PTYs and processes.
