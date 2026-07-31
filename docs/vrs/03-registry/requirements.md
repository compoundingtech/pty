# Registry requirements

> **Role.** The durable identity and observation realization of the root
> contract. Every requirement refines a `PTY-R*` requirement.

## Assumptions

- **PTY.REG-A01 Single writer authority:** The `pty` implementation is the
  canonical writer. External tools may read tier-1 files directly.
- **PTY.REG-A02 Same-filesystem publication:** Temporary and target metadata
  files share a filesystem, so rename publishes atomically.

## Acceptable tradeoffs

- **PTY.REG-T01 Last-write-wins metadata:** Individual file replacement is
  atomic, but concurrent read-modify-write metadata updates are not
  transactionally merged.
- **PTY.REG-T02 Bounded events:** Event history truncates from 1,000 to the most
  recent 500 lines rather than growing without bound.

## Requirements

- **PTY.REG-R01 — Root isolation.** Every command resolves one registry from
  explicit `PTY_ROOT` or the documented default; distinct roots share no
  sessions, sockets, events, or cleanup. _refines: PTY-R02, PTY-R07._
- **PTY.REG-R02 — Durable launch metadata.** Tier-1 metadata records stable
  identity-adjacent launch, generation, presentation, exit, and restart state
  and is published atomically. _refines: PTY-R05._
- **PTY.REG-R03 — Pure inventory.** Listing derives `running`, `exited`, or
  `vanished` from bounded process/socket evidence and never cleans, restarts,
  migrates, or repairs state. _refines: PTY-R06._
- **PTY.REG-R04 — Observable live state.** Status exposes the effective terminal
  geometry, cursor/scrollback, process and daemon resources, terminal modes,
  and anonymous client counts plus requested geometry and constraints without
  attaching. Events expose timestamped lifecycle, terminal, presentation, tag, and
  `user.*` changes. _refines: PTY-R06._
- **PTY.REG-R05 — Stable identity.** Filename-safe stable ids are unique in a
  registry. Display names are mutable and non-unique; exact id wins resolution,
  and an ambiguous display name refuses with candidates. Tags support
  composable filtering but do not create hard isolation. _refines: PTY-R07._
- **PTY.REG-R06 — Documented compatibility tiers.** Tier-1 metadata and events
  are documented and storage changes are called out; temporary files are
  ignored; tier-2 socket, pid, lock, and UI files remain internal. Older
  metadata fields retain documented defaults. _refines: PTY-R09._
- **PTY.REG-R07 — Generation-safe ownership.** A daemon or lifecycle operation
  removes registry artifacts only when generation evidence still matches its
  observation. Creation uses an exclusive, dead-owner-recoverable id lock.
  _refines: PTY-R02, PTY-R05._
- **PTY.REG-R08 — External-readable events.** Each event is one JSONL
  envelope with session, type, timestamp, and typed payload; external followers
  can reopen after truncation and subscribe instead of polling metadata.
  _refines: PTY-R06, PTY-R09._
