# Synchronization requirements

> **Role.** Define the causally exact initial and reconnect baseline. These
> requirements refine the parent session-stream contract.

## Requirements

- **PTY.STREAM.SYNC-R01 — Geometry first.** An attach or peek generation emits
  effective geometry before screen, data, or session exit for that generation.
  A local machine detach outcome may terminate before terminal state begins.
  _refines: PTY.STREAM-R01._
- **PTY.STREAM.SYNC-R02 — Exact parser cut.** Screen serialization runs only
  after an ordered terminal-parser marker; post-marker data and exit are queued
  until after that screen. _refines: PTY.STREAM-R02._
- **PTY.STREAM.SYNC-R03 — No lost settling data.** Data accepted while waiting
  for resize settling remains represented in the later screen baseline rather
  than being emitted ahead of it. _refines: PTY.STREAM-R02._
- **PTY.STREAM.SYNC-R04 — Source-ordered exit.** Post-cut data is flushed before
  its queued exit. A child that exited before the cut receives one synthesized
  exit only after the baseline and any final post-cut data. _refines:
  PTY.STREAM-R01, PTY.STREAM-R02._
- **PTY.STREAM.SYNC-R05 — Supersession.** A later attach or peek request on the
  same socket atomically replaces its client role and invalidates the prior
  delayed cut and queue. A malformed attach changes neither. _refines:
  PTY.STREAM-R02._
- **PTY.STREAM.SYNC-R06 — Reconnect equivalence.** Every reconnect begins a new
  generation satisfying the same geometry-screen-live order. _refines:
  PTY.STREAM-R06._
