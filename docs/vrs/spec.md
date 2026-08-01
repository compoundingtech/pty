# pty specification

This document specifies the current `pty` system. It builds on
[requirements.md](./requirements.md).

## Status

Draft until every mapped contract is present on the default branch. The test
matrix below is the executable validation boundary.

## Scope

This specification defines persistent session execution, ordered terminal
transport, registry state, and supported CLI/package surfaces. It does not
define a shell, window manager, multi-tenant authorization boundary, or product
orchestration policy.

## Composition

```text
CLI / package / testing / remote surfaces
                  |
        +---------+---------+
        |                   |
 ordered client stream   durable registry
        |                   |
        +---------+---------+
                  |
          per-session runtime
                  |
          child process + PTY
```

The runtime owns the child process and headless terminal model. The stream
projects one ordered terminal to ephemeral clients. The registry owns stable
identity and durable observations. Every public surface composes these three
sources rather than defining alternate semantics (R01, R09, R11).

## Runtime and launch

A session is a detached daemon containing one child PTY and one headless
terminal emulator. The daemon owns the socket and survives client disconnects
(R01).

Launch environment assembly is ordered (R02):

```text
replacement mode: copy env

inherited mode:   process.env -> remove internal server config
isolated mode:    allowlisted process.env + LC_*

policy modes only: base -> unsetEnv[] -> extraEnv{}
all modes:         -> force PTY_SESSION + generation token
                   -> TERM absent/empty ? xterm-256color : preserve value
```

Replacement mode and the inherited/isolate policy options are mutually
exclusive. Metadata persists the selected mode and its removals/assignments.
Explicit restart reuses it. Permanent reconciliation re-reads a current
manifest declaration when available and otherwise uses persisted metadata.
Metadata predating `unsetEnv` retains historical ambient inheritance.

On child exit, the runtime drains accepted output, records final screen and exit
state, emits the lifecycle event, and applies cleanup policy. Every mutating
cleanup/restart path compares stable id plus generation so stale work cannot
change a replacement session (R03).

## Ordered client stream

Packets use a five-byte header followed by a bounded payload:

```text
[type: uint8][length: uint32BE][payload: length bytes]
```

The reader reassembles partial input and rejects declared payloads above 32 MiB
(R07). Unknown bounded message types are ignored for additive compatibility;
capability-specific surfaces fail closed when required packets are absent.

### Synchronization

For each admitted attach/peek generation (R04):

```text
parser bytes before cut | parser bytes after cut | process exit
          |                       |                    |
          v                       v                    v
GEOMETRY -> SCREEN -----------> queued DATA -------> EXIT
```

The screen callback is the causal cut: `SCREEN` represents all earlier parser
writes; later data and exit queue behind it. A newer valid mode request
invalidates the unfinished generation. Reconnect creates a fresh generation.
A local machine detach may end with `DETACH` before a baseline is emitted.

### Roles and geometry

Each socket begins in the command role. Role frames replace, rather than
accumulate, socket state (R05):

| Frame | Resulting role | Geometry membership | Input/resize |
| --- | --- | --- | --- |
| complete `ATTACH(rows, cols)` | writable-attached | requested rows/cols | enabled |
| recognized `PEEK(flags)` | readonly | none | disabled |
| malformed `ATTACH` | unchanged | unchanged | unchanged |
| `STATUS` | unchanged | unchanged | unchanged |

Client-to-server data, status, and resize behavior is role-specific:

| Role | `DATA` | `STATUS` | `RESIZE` |
| --- | --- | --- | --- |
| command | accepted | accepted | ignored |
| writable-attached | accepted | accepted | accepted |
| readonly | ignored | accepted | ignored |

Command sockets do not receive a screen baseline and do not participate in
geometry. They may receive baseline-less live `DATA` or `EXIT` broadcasts;
those packets do not constitute reconstructable terminal state. A consumer that
needs reconstructable terminal state must first send `ATTACH` or `PEEK`. Public
stats omit command sockets and expose the writable-attached role with the
compatibility string `"writable"`.

For writable-attached request set `W`, shared geometry is (R06):

```text
rows = min(client.rows for client in W)
cols = min(client.cols for client in W)
```

The dimensions are minimized independently. A changed `GEOMETRY` notification
precedes terminal output produced after the corresponding PTY resize. Removing
the last writable-attached client leaves the last effective geometry stable.

### Machine attach

`attach --attach-stream-fd-v1 <fd> <ref>` requires an inherited writable
descriptor `fd >= 3`. The packaged CLI runs without a wrapper child so the
descriptor, controlling terminal, signals, and process identity reach the
adapter unchanged (R08, R11).

The adapter reframes only `GEOMETRY`, `SCREEN`, `DATA`, and terminal outcomes to
the descriptor; terminal interaction stays on stdin/stdout and diagnostics use
stderr. It flushes exactly one clean outcome before EOF:

| Outcome | Meaning |
| --- | --- |
| `EXIT(code)` | the session process ended |
| empty `DETACH` | this local client intentionally detached |
| EOF without either | transport loss, reconnect give-up, descriptor failure, or abrupt administrative destruction |

The last row is a non-zero truncation, not a third clean outcome (R07, R08).

## Registry and lifecycle state

`PTY_ROOT` selects one registry. A stable id owns socket, metadata, events, and
generation locks; display names and tags remain mutable lookup/presentation
fields (R09).

Inventory is observational: it derives running/exited/vanished state and enriches
it with live status when available, but does not restart, reap, or attach.
Status reports client roles, requested/effective geometry, process resources,
and terminal modes.

Metadata and events form two compatibility tiers (R10):

| Record | Contract |
| --- | --- |
| metadata JSON | durable launch/lifecycle source; atomic generation-aware updates preserve unknown fields |
| event JSONL | externally readable observation stream; serialized append and bounded retention |
| socket packets | internal bounded protocol with documented legacy decoding fallbacks |

Explicit lifecycle commands and `gc` own mutation. Cleanup is authorized by the
observed generation; removal wins over late daemon finalization, and permanent
respawn cannot overwrite a replacement (R03, R10).

### Live registry recovery

A supporting daemon may publish an opaque recovery capability only when it can
prove its process-start identity and both `PTY_ROOT` and `.recovery` are private
directories owned by the daemon user. If an external cleanup unlinks that live
session's socket, pid, and metadata paths, `recover --snapshot` authenticates a
complete retained metadata snapshot and asks the original daemon to rebind its
listener. It preserves the daemon generation, child process, provider launch,
terminal state, and attached clients; it does not probe by signal, restart,
relaunch, or replace an occupied pathname (R03, R09).

The request/result exchange binds stable id, daemon pid and process-start token,
generation, launch identity, root and recovery-directory device/inode identity,
and the daemon's signed metadata revision. Metadata mutation advances the signed
revision before publishing the replacement record. Recovery therefore fails
closed after a partial publication and rejects missing, legacy, stale, replayed,
tampered, wrong-root, permission-downgraded, or path-replacement state. Success
republishes the socket, pid, and metadata with no-replace and owned-rollback
semantics and rotates the recovery secret. An
authenticated lock left by an interrupted recoverer may resume; other creation
locks remain authoritative and are never displaced (R10).

## Surfaces

The CLI, package entrypoint, exported client/server/protocol modules, testing
library, and remote route call the same behavioral core (R11). Completion
schemas preserve required option values. Remote streaming preserves local
packet order and fails explicitly when the peer lacks a capability. The testing
library drives real processes and PTYs and exposes screen, cursor, scrollback,
input, resize, and multi-client geometry without mocks.

## Ownership and validation matrix

| Requirement | Owning source | Primary executable evidence |
| --- | --- | --- |
| R01 | [server](../../src/server.ts), [spawn](../../src/spawn.ts) | [integration](../../tests/integration.test.ts), [exit reap](../../tests/exit-reap.test.ts), [shutdown](../../tests/shutdown-backstop.test.ts) |
| R02 | [server](../../src/server.ts), [spawn](../../src/spawn.ts), [sessions](../../src/sessions.ts), [ptyfile](../../src/ptyfile.ts) | [spawn options](../../tests/spawn-options.test.ts), [restart parity](../../tests/restart-launch-parity.test.ts), [restart scrub](../../tests/restart-env-scrub.test.ts), [ptyfile](../../tests/ptyfile.test.ts) |
| R03 | [server](../../src/server.ts), [sessions](../../src/sessions.ts), [recovery](../../src/recovery.ts) | [kill](../../tests/kill-wait.test.ts), [immediate reuse](../../tests/rm-immediate-reuse.test.ts), [generation guard](../../tests/gc-generation-guard.test.ts), [exit signal](../../tests/exit-signal.test.ts), [recovery](../../tests/recovery.test.ts) |
| R04 | [server](../../src/server.ts), [connection](../../src/connection.ts) | [integration](../../tests/integration.test.ts), [alternate screen](../../tests/screen-replay-altscreen.test.ts), [scrollback](../../tests/scrollback-fidelity.test.ts) |
| R05 | [server](../../src/server.ts) | [integration](../../tests/integration.test.ts) |
| R06 | [server](../../src/server.ts), [protocol](../../src/protocol.ts) | [effective geometry](../../tests/effective-geometry.test.ts), [resize](../../tests/resize-tui.test.ts), [status](../../tests/stats-cli.test.ts) |
| R07 | [protocol](../../src/protocol.ts), [connection](../../src/connection.ts), [remote](../../src/remote.ts) | [protocol](../../tests/protocol.test.ts), [connection](../../tests/connection.test.ts), [remote reconnect](../../tests/remote-reconnect.test.ts) |
| R08 | [client](../../src/client.ts), [CLI](../../src/cli.ts), [entrypoint](../../bin/pty) | [attach stream](../../tests/attach-stream.test.ts), [signals](../../tests/wrapper-signal-forwarding.test.ts) |
| R09 | [sessions](../../src/sessions.ts), [server](../../src/server.ts), [recovery](../../src/recovery.ts), [CLI](../../src/cli.ts) | [root](../../tests/pty-root.test.ts), [display name](../../tests/display-name.test.ts), [status](../../tests/stats-cli.test.ts), [list purity](../../tests/list-purity.test.ts), [recovery](../../tests/recovery.test.ts) |
| R10 | [sessions](../../src/sessions.ts), [events](../../src/events.ts), [recovery](../../src/recovery.ts), [protocol](../../src/protocol.ts) | [atomic writes](../../tests/atomic-writes.test.ts), [metadata events](../../tests/metadata-events.test.ts), [events](../../tests/events.test.ts), [recovery](../../tests/recovery.test.ts), [disk layout](../../tests/disk-layout-docs.test.ts) |
| R11 | [CLI](../../src/cli.ts), [client API](../../src/client-api.ts), [remote](../../src/remote.ts), [testing API](../../src/testing/index.ts) | [help](../../tests/help.test.ts), [completions](../../tests/completions.test.ts), [remote](../../tests/remote-fabric.test.ts), [screenshots](../../tests/screenshot.test.ts), [keys](../../tests/keys.test.ts) |

`node scripts/verify-docs.ts --vrs-only` validates this two-document shape,
sequential requirement IDs, links, and complete requirement references.
