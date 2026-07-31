# Lifecycle specification

This document specifies session lifecycle transitions. It builds on
[requirements.md](./requirements.md) and the parent
[runtime specification](../spec.md).

## Status

Active.

## State and actions

```text
          child exits cleanly
running ------------------------> exited
   |                                |
   | daemon disappears              | restart / permanent gc
   v                                v
vanished ----------------------> running (new generation)
   |                                ^
   +------- explicit cleanup -------+
```

`exited` requires recorded `exitCode`/`exitedAt`; a dead daemon without that
record is `vanished`. Both are non-live and may be inspected, removed, swept, or
used as restart input. `listSessions` derives the state and remains
observational (PTY.RUN.LIFE-R01, R06).

## Exit policy

| Condition | Result |
| --- | --- |
| explicit `rm` | remove after generation-safe daemon shutdown |
| `keep=true` | preserve until explicit `rm` |
| `--ephemeral` | reap on shutdown unless keep is set |
| `strategy=permanent` | preserve for reconciliation unless ephemeral |
| explicit `kill` | stop and preserve unless ephemeral |
| ordinary exit | configured `PTY_REAP_ON_EXIT`, default reap |

Before applying the table, the runtime's public exit callback is ordered after
the PTY read stream, terminal metadata is snapshotted, and `session_exit` is
emitted (PTY.RUN.LIFE-R02–R03).

## Ownership and reconciliation

A stable id has an exclusive creation lock. Cleanup checks the opaque metadata
generation it observed before deleting socket or all artifacts. Permanent
respawn holds that lock across compare, stale cleanup, and replacement socket
publication. A replacement generation therefore cannot be deleted by an older
daemon or stale `gc` plan (PTY.RUN.LIFE-R04).

`gc` applies parent-orphan removal before permanent respawn, optionally detects
cwd disappearance or idle abandonment, tracks bounded fast-failure state, and
then sweeps eligible finished records. `--dry-run` plans without mutation
(PTY.RUN.LIFE-R05).
