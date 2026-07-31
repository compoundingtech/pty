# Geometry requirements

> **Role.** Define the single grid shared by writable clients. These
> requirements refine the parent session-stream contract.

## Requirements

- **PTY.STREAM.GEO-R01 — Independent dimensions.** Effective rows are the
  minimum requested rows and effective columns are the minimum requested
  columns across connected writable clients. _refines: PTY.STREAM-R03._
- **PTY.STREAM.GEO-R02 — Writable membership.** A writable client joins on
  attach, updates its request on resize, and leaves on detach, error, or close.
  _refines: PTY.STREAM-R03._
- **PTY.STREAM.GEO-R03 — Readonly neutrality.** Peek and status clients do not
  constrain geometry. Peek clients still receive every effective-geometry
  update needed to parse the state they observe. _refines: PTY.STREAM-R03,
  PTY.STREAM-R05._
- **PTY.STREAM.GEO-R04 — Ordered application.** On change, the runtime resizes
  its terminal model, broadcasts geometry, then resizes the child PTY before
  child redraw output can be delivered. _refines: PTY.STREAM-R03._
- **PTY.STREAM.GEO-R05 — Explicit requested/effective split.** Client APIs retain
  their requested size separately from the effective size reported by the
  runtime and resize their local terminal grid before parsing affected bytes.
  _refines: PTY.STREAM-R03._
- **PTY.STREAM.GEO-R06 — Zero-writable stability.** Removing the last writable
  client does not invent a new size; the runtime retains its last effective
  grid until a later writable request changes it. _refines: PTY.STREAM-R03._
