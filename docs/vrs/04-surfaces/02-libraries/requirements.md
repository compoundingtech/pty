# Library requirements

> **Role.** Define the embedding and real-terminal testing surfaces. These
> requirements refine the parent surface contract.

## Requirements

- **PTY.SURF.LIB-R01 — Explicit module boundaries.** The package exports client,
  server, protocol, keys, testing, and TUI modules from compiled JavaScript with
  matching declarations. _refines: PTY.SURF-R07._
- **PTY.SURF.LIB-R02 — Shared client semantics.** `SessionConnection`, direct
  client helpers, and `attachPty` use the shared packet codec, stable-id
  registry paths, and effective-geometry events. _refines: PTY.SURF-R01._
- **PTY.SURF.LIB-R03 — Real-process tests.** `Session.spawn` starts a real PTY;
  `Session.server` starts a real persistent runtime; neither substitutes a mock
  terminal transport. _refines: PTY.SURF-R02._
- **PTY.SURF.LIB-R04 — Reconstructable assertions.** Testing screenshots expose
  trimmed lines, joined text, and ANSI serialization; bounded waits report the
  current screen when their predicate fails. _refines: PTY.SURF-R02._
- **PTY.SURF.LIB-R05 — Multi-client geometry fidelity.** Server-mode test
  sessions and TUI panes distinguish requested and effective geometry, resize
  their emulator before later data, and update across peer attach, resize, and
  disconnect. _refines: PTY.SURF-R02._
- **PTY.SURF.LIB-R06 — Daemon strategy equivalence.** Direct server-module
  launch, explicit server-module override, and installed-CLI fallback carry the
  same launch definition or reject unsupported options explicitly. _refines:
  PTY.SURF-R01, PTY.SURF-R03._
