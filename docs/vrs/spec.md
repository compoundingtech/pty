# pty specification

This document specifies the composition of the current `pty` system. It builds
on [requirements.md](./requirements.md).

## Status

Active. The implementation and executable tests are the behavioral evidence;
this tree is their durable contract map.

## Scope

This tree defines persistent session execution, terminal-state transport,
registry state, and the supported package surfaces. It does not define a window
manager, shell, multi-tenant security boundary, or orchestration policy for
products that use `pty`.

## System composition

```text
04 surfaces (CLI, package APIs, testing, remote)
        |
        +-----------------------+
        v                       v
02 ordered session stream   03 durable registry
        |                       |
        +-----------+-----------+
                    v
             01 session runtime
                    |
                    v
             child process + PTY
```

The dependency direction follows the numeric tree. The runtime owns the child
and terminal emulator. The stream projects ordered terminal state to clients.
The registry projects durable identity and observations. Surfaces compose these
contracts without redefining them.

## Core invariants

1. A session is identified by its stable registry id, not by a client, process
   label, display name, or socket connection (PTY-R01, PTY-R07).
2. Terminal reconstruction is an ordered state transfer: effective geometry,
   then one screen baseline, then post-baseline data or exit (PTY-R03, PTY-R04).
3. Durable metadata describes a launch and an observed generation; live status
   is derived from metadata plus process/socket evidence (PTY-R05, PTY-R06).
4. Every surface either preserves the relevant underlying contract or rejects
   the unsupported capability explicitly (PTY-R08, PTY-R09).

## Subsystem ownership

| Subsystem | Owns | Does not own |
| --- | --- | --- |
| [session runtime](./01-session-runtime/spec.md) | child, PTY, emulator, launch context, exit and restart primitives | client presentation, registry discovery |
| [session stream](./02-session-stream/spec.md) | framing, connection roles, synchronization, geometry and exit order | session identity, restart policy |
| [registry](./03-registry/spec.md) | roots, stable ids, metadata, events, inventory and cleanup ownership | terminal parsing, CLI rendering |
| [surfaces](./04-surfaces/spec.md) | CLI/package/API composition, packaging, remote routing | alternate semantics for the three lower layers |
