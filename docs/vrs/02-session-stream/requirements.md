# Session stream requirements

> **Role.** The ordered transport realization of the root terminal contract.
> It projects one runtime's terminal state to ephemeral clients. Every
> requirement refines a `PTY-R*` requirement.

## Assumptions

- **PTY.STREAM-A01 Ordered byte transport:** A local Unix socket and a routed
  remote byte stream preserve byte order for one connection.
- **PTY.STREAM-A02 Parser cut:** The headless terminal's write callback is an
  ordered marker after all earlier parser writes.

## Acceptable tradeoffs

- **PTY.STREAM-T01 Redraw settling:** After an effective resize, initial screen
  capture may wait for a bounded redraw-settle interval so the baseline is not
  a known transient mid-redraw frame.
- **PTY.STREAM-T02 Unversioned base framing:** The base packet header has no
  protocol-version field. Bounded unknown packet types are ignored for additive
  compatibility; capability-specific surfaces fail closed on missing required
  packets.

## Requirements

- **PTY.STREAM-R01 — Ordered reconstruction.** Every `ATTACH` and `PEEK`
  generation that emits terminal state starts with `GEOMETRY`, then exactly one
  `SCREEN` baseline, then post-cut `DATA` and at most one `EXIT` in source order.
  A local machine `DETACH` may terminate its client before the baseline.
  _refines: PTY-R03._
- **PTY.STREAM-R02 — Exact parser boundary.** Output accepted before the screen
  cut is represented by `SCREEN`; output and exit after the cut are queued and
  cannot overtake the baseline. A newer mode request on the same connection
  invalidates the unfinished generation. _refines: PTY-R03._
- **PTY.STREAM-R03 — Causal effective geometry.** Writable attach/resize/disconnect
  recomputes the minimum requested rows and columns. Changed `GEOMETRY` is
  broadcast before terminal bytes produced for that size; readonly clients
  receive updates but never constrain the grid. _refines: PTY-R04._
- **PTY.STREAM-R04 — Bounded framing.** Every packet is length-delimited, partial
  reads are reassembled, and a declared payload above 32 MiB drops the
  connection without unbounded buffering. _refines: PTY-R09._
- **PTY.STREAM-R05 — Replaceable client roles.** Every attach frame with complete
  geometry replaces the socket's role with writable, installs its requested
  geometry, and authorizes input and resize. Every recognized peek frame
  replaces the role with readonly and removes its geometry constraint. A
  malformed attach changes neither role nor synchronization generation. Status
  observes without joining geometry, and detach closes the client without
  ending the session. _refines: PTY-R01, PTY-R06._
- **PTY.STREAM-R06 — Reconnect is a new baseline.** Reconnecting to a living
  session creates a fresh ordering generation and resets protocol parsing;
  observed session `EXIT` is terminal and is never reconnected past. _refines:
  PTY-R03, PTY-R05._
- **PTY.STREAM-R07 — Machine stream fidelity.** Machine attach reframes original
  `GEOMETRY`, `SCREEN`, `DATA`, and `EXIT` packets on a caller-owned inherited
  descriptor and uses the existing empty `DETACH` frame for the local-detach
  outcome. It keeps stdin/stdout as the controlling terminal, honors
  backpressure, and ends with exactly one framed outcome: `EXIT` when the
  session process ended or `DETACH` when this client intentionally detached.
  EOF after either is clean; EOF without either is truncation or transport
  loss. _refines: PTY-R08, PTY-R09._

See [synchronization](./01-synchronization/requirements.md) and
[geometry](./02-geometry/requirements.md) for concrete refinements.
