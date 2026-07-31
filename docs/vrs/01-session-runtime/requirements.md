# Session runtime requirements

> **Role.** The execution realization of the root
> [session contract](../requirements.md): one independently surviving daemon,
> child PTY, and terminal model per session. Every requirement refines a
> `PTY-R*` requirement.

## Assumptions

- **PTY.RUN-A01 Child semantics:** The spawned command obeys ordinary Unix PTY,
  signal, and exit-status semantics.
- **PTY.RUN-A02 Recoverable declaration:** A permanent session backed by a
  readable `pty.toml` may take its next launch declaration from that manifest;
  otherwise persisted metadata is the last-known-good declaration.

## Acceptable tradeoffs

- **PTY.RUN-T01 Bounded startup wait:** Daemon startup has a finite socket-ready
  wait. Immediate process failure is surfaced earlier, while a living but slow
  daemon may consume the full bound.
- **PTY.RUN-T02 Stateless permanent reconciliation:** Permanent respawn has no
  resident supervisor. `pty gc` invocations provide cadence and rate limiting.

## Requirements

- **PTY.RUN-R01 — Client-independent lifetime.** The daemon and child continue
  after the creating or attached client exits. Binding daemon lifetime to a
  spawner is explicit opt-in. _refines: PTY-R01._
- **PTY.RUN-R02 — Terminal authority.** The runtime feeds every child output byte
  through one headless terminal model and uses that model for screen replay,
  terminal modes, cursor, scrollback, and exit snapshots. _refines: PTY-R01,
  PTY-R03._
- **PTY.RUN-R03 — Per-session failure boundary.** Each session has its own
  daemon, child, terminal, and socket. Closing one runtime does not control
  another. _refines: PTY-R02._
- **PTY.RUN-R04 — Restart-equivalent launch.** Initial launch, explicit restart,
  and permanent respawn preserve the applicable command, args, cwd, initial
  geometry, lifetime flags, tags, display name, and child-environment policy.
  _refines: PTY-R05._
- **PTY.RUN-R05 — Explicit environment policy.** Exact environment and inherited
  environment policy are mutually exclusive; removals precede assignments;
  ordinary keys retain explicit empty values, while `PTY_SESSION` and terminal
  capability metadata remain runtime-owned invariants.
  _refines: PTY-R05, PTY-R09._
- **PTY.RUN-R06 — Observable exit.** Child data is drained before one terminal
  exit result is finalized, with signal exits represented by shell-compatible
  status. Final screen metadata and lifecycle policy are applied before daemon
  shutdown completes. _refines: PTY-R05, PTY-R06._
- **PTY.RUN-R07 — Explicit reconciliation.** Restart, permanent respawn,
  abandonment, parent-orphan handling, and cleanup are explicit lifecycle
  actions with generation checks; observation alone never performs them.
  _refines: PTY-R02, PTY-R05._

See [launch context](./01-launch-context/requirements.md) and
[lifecycle](./02-lifecycle/requirements.md) for the concrete refinements.
