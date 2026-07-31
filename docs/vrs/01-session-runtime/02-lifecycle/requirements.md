# Lifecycle requirements

> **Role.** Define exit, preservation, cleanup, and restart ownership for one
> session. These requirements refine the parent session-runtime contract.

## Requirements

- **PTY.RUN.LIFE-R01 — Derived lifecycle state.** `running`, `exited`, and
  `vanished` are derived from process/socket evidence and exit metadata, not a
  mutable stored status field. _refines: PTY.RUN-R06._
- **PTY.RUN.LIFE-R02 — Ordered finalization.** The runtime drains child output,
  records one exit result and final screen lines, emits one exit event, then
  applies preservation or reap policy. _refines: PTY.RUN-R06._
- **PTY.RUN.LIFE-R03 — Explicit policy precedence.** Explicit removal wins;
  `keep=true` forces preservation; ephemeral forces reap except when keep wins;
  permanent and explicit kill preserve state for restart or inspection.
  _refines: PTY.RUN-R06, PTY.RUN-R07._
- **PTY.RUN.LIFE-R04 — Generation-safe mutation.** Cleanup, replacement, and
  permanent respawn compare the observed generation while holding the per-id
  creation lock. A changed generation remains untouched. _refines:
  PTY.RUN-R07._
- **PTY.RUN.LIFE-R05 — Stateless permanent reconciliation.** `gc` can respawn a
  dead permanent session, reap parent-orphans or abandoned sessions, and hold a
  repeatedly fast-failing session as flapping. A dry run reports the same plan
  without lifecycle writes. _refines: PTY.RUN-R07._
- **PTY.RUN.LIFE-R06 — Observational purity.** Listing and state reads do not
  trigger cleanup, respawn, or metadata repair. _refines: PTY.RUN-R07._
