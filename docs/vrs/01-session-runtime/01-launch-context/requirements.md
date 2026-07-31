# Launch context requirements

> **Role.** Preserve the complete child launch across runtime creation and
> restart. These requirements refine the parent session-runtime contract.

## Requirements

- **PTY.RUN.ENV-R01 — Complete launch record.** Persisted launches retain
  command, args, display command, cwd, initial rows and columns, lifetime flags,
  tags, display name, and the chosen environment mode. _refines: PTY.RUN-R04._
- **PTY.RUN.ENV-R02 — Exclusive environment modes.** A caller chooses either an
  explicit replacement environment base or
  inherited/isolate-plus-removals-and-assignments; combining them is rejected
  before spawn. Exactness applies to ordinary caller-owned keys, subject to the
  runtime-owned invariants below. _refines: PTY.RUN-R05._
- **PTY.RUN.ENV-R03 — Ordered inherited policy.** In inherited and isolated
  modes, named removals are applied before explicit assignments, so assignment
  wins independently of CLI flag order. _refines: PTY.RUN-R05._
- **PTY.RUN.ENV-R04 — Runtime invariants.** `PTY_SESSION` is always the stable
  id. `TERM` is terminal capability metadata: an absent or empty value
  selects the runtime's `xterm-256color` terminal name, while a nonempty value
  is preserved through node-pty's public terminal-name contract. Ordinary keys
  retain exact assigned values, including an empty `NO_COLOR`. _refines:
  PTY.RUN-R05._
- **PTY.RUN.ENV-R05 — Restart durability.** Explicit restart and metadata-based
  permanent respawn preserve environment removals and assignments. A
  manifest-backed permanent respawn re-reads its manifest declaration, while a
  missing or unreadable manifest falls back to persisted metadata. _refines:
  PTY.RUN-R04._
- **PTY.RUN.ENV-R06 — Historical compatibility.** Metadata without an
  `unsetEnv` field retains historical ambient inheritance rather than gaining
  an implicit removal policy. _refines: PTY.RUN-R04._
