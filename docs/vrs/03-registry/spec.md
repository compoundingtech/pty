# Registry specification

This document specifies registry identity, storage, observation, and ownership.
It builds on [requirements.md](./requirements.md).

## Status

Active. The complete public field and event schemas are maintained in
[the disk-layout reference](../../disk-layout.md).

## Root and artifacts

```text
$PTY_ROOT/                         mode 0700
  <id>.json                        tier 1 metadata
  <id>.events.jsonl                tier 1 events
  <id>.sock                        tier 2 live socket
  <id>.pid                         tier 2 daemon pid
  <id>.lock                        tier 2 creation lock
  <target>.tmp.<pid>.<random>      ignored publication temporary
  theme, gc.log                    tier 2 operator/UI state
```

`PTY_ROOT` is canonical. Deprecated `PTY_SESSION_DIR` is accepted only when the
canonical variable is absent and emits a one-time warning unless explicitly
silenced. When both exist, `PTY_ROOT` wins visibly. Every operation uses the
same resolution (PTY.REG-R01, R06).

## Identity and resolution

Stable ids use `[a-zA-Z0-9._-]+`, fit both the filename and the smallest
supported Unix socket path, and are protected by a per-id lock. Display names
allow printable presentation text but are not path material or identity.

```text
reference
  -> exact stable id match
  -> exactly one displayName match
  -> otherwise absent or explicit ambiguity
```

Repeated `--filter-tag key=value` predicates all must match. A reserved tag can
drive lifecycle or outer-tool bookkeeping, but tags remain metadata within one
registry (PTY.REG-R05).

## Metadata publication and observation

Metadata is pretty-printed JSON written to a randomized sibling temporary and
renamed into place. Readers see the old or new complete file. The public schema
includes opaque generation and daemon pid, launch definition, timestamps, exit
and last-screen state, tags/display name, and last writable attach time
(PTY.REG-R02, R06).

Inventory combines metadata with bounded pid/socket probes:

| State | Evidence |
| --- | --- |
| `running` | daemon alive and socket reachable |
| `exited` | no live daemon and recorded exit details |
| `vanished` | no live daemon and no recorded exit details |

The inventory path performs no cleanup. `STATUS` queries a live daemon and
reports terminal, process, daemon, modes, uptime, and client connections. Client
rows contain no user identity; they expose readonly/writable role, requested
geometry, request sequence, and whether each dimension constrains the effective
grid (PTY.REG-R03–R04).

## Events

Each event line has `{ session, type, ts, ...payload }`. System types cover
terminal signals, lifecycle, exec, respawn/abandon/flapping, and metadata
changes. User types are namespaced `user.<suffix>` and reject empty,
whitespace, control characters, and system-name collisions.

Appends are serialized within a runtime. Periodic bounded truncation atomically
replaces the file with the latest 500 lines when it reaches 1,000, so followers
detect inode change and reopen. Session lifecycle cleanup removes the event file
with the session (PTY.REG-R08).

## Ownership

Metadata generation is opaque to readers. Daemon self-cleanup, explicit remove,
and reconciliation compare their observed generation before deleting. Creation
locks are exclusive and may be stolen only when the recorded owner process is
dead (PTY.REG-R07).
