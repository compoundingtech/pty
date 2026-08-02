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

The unadmitted `ATTACH` writable path is compatibility-only. New interactive or
embedded consumers must use machine attach v2; no new consumer may depend on
the legacy writable role. The compatibility path is removed once
`SessionConnection`, the TUI builder transport, and the server-backed testing
session have migrated to v2 and their legacy protocol parity fixtures have been
replaced.

For writable-attached request set `W`, shared geometry is (R06):

```text
rows = min(client.rows for client in W)
cols = min(client.cols for client in W)
```

The dimensions are minimized independently. A changed `GEOMETRY` notification
precedes terminal output produced after the corresponding PTY resize. Removing
the last writable-attached client leaves the last effective geometry stable.

### Machine attach

`machine-attach-v2` gives stdin and stdout exclusively to a bounded framed
protocol. It does not allocate a controlling terminal, inherit a side-channel
descriptor, interpret an interactive detach key, or fall back to machine attach
v1 (R08, R11).

Human `pty attach` is an interactive policy adapter over that same behavioral
core, not a second writable daemon protocol. It reads the exact local generation
from metadata or the remote generation from route admission, requests the
optional `host-terminal-replay` capability, and frames complete UTF-8 input,
resize, and detach requests through `machine-attach-v2`. The local policy alone
reserves Ctrl-\\: it recognizes C0, Kitty, and xterm modifyOtherKeys level 2
encodings across stream chunks according to the installed input-mode snapshot,
uses a bounded ambiguity deadline only for an incomplete active encoding, and
otherwise preserves input bytes exactly. A second Ctrl-\\ within the documented
double-tap window emits one C0 byte to the child.

```text
host                         adapter                         daemon
 | OPEN(id,generation,size,caps) |                              |
 |------------------------------>| OPEN_V2 + STATUS sentinel    |
 |                               |----------------------------->|
 |       ADMISSION_FAILURE       |<-- STATUS (legacy) ----------|
 |<------------------------------|                              |
 |                               |<-- ADMISSION_V2 --------------|
 | HELLO -> READY -> updates     |                              |
 |<------------------------------|                              |
 | INPUT / RESIZE / DETACH       |                              |
 |------------------------------>|----------------------------->|
 | typed stream outcome -> EOF   |                              |
 |<------------------------------|                              |
```

Every machine frame uses the common five-byte header and the 32 MiB bound.
Structured payloads are fatal UTF-8 JSON capped at 64 KiB with exact fields;
unknown tags or fields are protocol errors. `DATA`, `INPUT`, and the screen
suffix of `READY` retain byte payloads. `INPUT` reaches the
node-pty string boundary only after one fatal UTF-8 validation. Valid UTF-8 and
all C0/escape bytes, including `0x1c`, pass unchanged; invalid UTF-8 produces a
typed failure rather than replacement characters (R07, R08).

The direction-specific frame schemas are:

| Direction | Frame | Payload and ordering |
| --- | --- | --- |
| host -> adapter | `OPEN` | Protocol 2, exact session id and expected generation, non-zero rows/cols, required capabilities; exactly once and first |
| host -> adapter | `INPUT` | Framed terminal bytes; only after `READY` |
| host -> adapter | `RESIZE` | Non-zero rows/cols; only after `READY` |
| host -> adapter | `DETACH` | Empty explicit intent after `HELLO`, including before `READY`; at most once |
| adapter -> host | `HELLO` | Exact generation, negotiated capabilities, diagnostic daemon build identity |
| adapter -> host | `READY` | Atomic output revision, effective geometry, complete input-mode snapshot, and screen baseline; at most once |
| adapter -> host | `DATA` | Atomic output revision, input-mode revision, optional complete changed input-mode snapshot, and terminal bytes after `READY` |
| adapter -> host | `GEOMETRY` | Ordered effective size after `READY` |
| adapter -> host | `ADMISSION_FAILURE` | Typed rejection before `HELLO`; no daemon attach mutation |
| adapter -> host | `EXITED` / `DETACHED` / `STREAM_FAILURE` | Exactly one terminal outcome, then EOF |

The causal output frames are:

```ts
type InputModeSnapshotV1 = {
  schema: "pty.input-mode.v1"
  wireEncoder: "xterm-input.v1"
  revision: number
  applicationCursorKeys: boolean
  applicationKeypad: boolean
  bracketedPaste: boolean
  focusReporting: boolean
  modifyOtherKeys: 0 | 1 | 2
  kittyKeyboardFlagsStack: u32[]
  mouseTracking: "Off" | "X10Press" | "Click" | "ButtonMotion" | "AnyMotion"
  mouseEncoding: "X10" | "Utf8" | "Sgr" | "Urxvt"
  mouseCoordinates: "Cell" | "Pixel"
}

type Ready = {
  outputRevision: number
  rows: number
  cols: number
  inputModes: InputModeSnapshotV1
  screen: Uint8Array
}

type Data = {
  outputRevision: number
  inputModeRevision: number
  inputModes?: InputModeSnapshotV1
  bytes: Uint8Array
}
```

`READY` is the authoritative baseline for one bound stream; its revision is read
at the baseline cut after earlier parser callbacks commit and need not be
contiguous with an earlier connection. Each subsequent `DATA` has non-empty bytes
and `outputRevision = previous + 1`; stripped query-only or empty output emits no
`DATA` and advances no output revision. `inputModes` is absent exactly when
`inputModeRevision` equals the installed revision. A changed mode revision must
strictly advance (it may jump after multiple transitions), and the same `DATA`
carries the complete replacement snapshot whose revision it names. Bytes, causal
stamps, and the optional snapshot form one envelope retained, delivered, and
reduced atomically.

Each client's bounded ordered writer retains a whole `DATA` envelope or rejects
it and ends that machine stream as a slow consumer. It never splits causal
metadata from bytes, pauses the PTY, or stalls another client.

The complete child-input mode snapshot contains application cursor keys,
application keypad, bracketed paste, focus reporting, modifyOtherKeys level,
mouse tracking (including DECSET 9), mouse encoding (including pixel SGR), and
the Kitty keyboard active-flags stack. Kitty set/add/remove and push/pop-n
operations update that one stack. The snapshot is state, not an inference from
screen bytes.

Fractal or another input-capable host opens its input gate only after applying
`READY`. For each `DATA`, it applies terminal bytes and any changed mode snapshot
in one reducer transition before encoding further child input. It never encodes
against a revision named by `DATA` but not yet installed.

The adapter sends daemon packet `OPEN_V2` (reserved type 8) followed by a
`STATUS` request on the same command-role socket. A v2 daemon answers first with
`ADMISSION_V2` (reserved type 9); a legacy daemon ignores type 8 and answers the
status sentinel first. Generation comparison and role transition are one
daemon operation. An accepting v2 daemon consumes and suppresses the following
status sentinel so no legacy status response enters the accepted stream.
Rejection leaves the command role unchanged. Version and
build identity are diagnostic; the three required capabilities are
`framed-utf8-input`, `typed-outcome`, and `input-mode-snapshot` (R07).
`host-terminal-replay` is an optional presentation capability: when requested,
the daemon prefixes `READY.screen` with its current host-terminal mode state.
Machine hosts that reconstruct presentation from the typed input-mode snapshot
do not request it.

The host lifecycle grammar is (R08):

```text
( HELLO ( READY (DATA | GEOMETRY)* STREAM_END
        | STREAM_END )
| ADMISSION_FAILURE ) EOF

STREAM_END = EXITED | DETACHED | STREAM_FAILURE
```

Bare EOF, a second outcome, a pre-`READY` update, a repeated `READY`, or a causal
revision violation is a protocol error. `AdmissionFailure`
includes unsupported daemon/capability, generation mismatch, missing or denied
session, malformed request, and adapter transport failure before `HELLO`.
`StreamFailure` covers baseline, streaming, or shutdown failure after `HELLO`.

## Registry and lifecycle state

`PTY_ROOT` selects one registry. A stable id owns socket, metadata, events, and
generation locks; display names and tags remain mutable lookup/presentation
fields (R09).

Inventory is observational: it derives running/exited/vanished state and enriches
it with live status when available, but does not restart, reap, or attach.
Every flattened local or remote inventory row carries the stable id and the
current opaque daemon generation as `generation: string | null`; live status
carries the same daemon generation as a string. Machine clients use this exact
pair for same-connection admission. Historical generation-less records remain
visible as `null` but are not eligible for machine attach v2; `createdAt` is
never interpreted as a generation.
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
| R07 | [protocol](../../src/protocol.ts), [machine protocol](../../src/machine-protocol.ts), [connection](../../src/connection.ts), [remote](../../src/remote.ts) | [protocol](../../tests/protocol.test.ts), [machine protocol](../../tests/machine-protocol.test.ts), [connection](../../tests/connection.test.ts), [remote reconnect](../../tests/remote-reconnect.test.ts) |
| R08 | [machine protocol](../../src/machine-protocol.ts), [machine adapter](../../src/machine-attach.ts), [CLI](../../src/cli.ts), [entrypoint](../../bin/pty) | [machine protocol](../../tests/machine-protocol.test.ts), [machine adapter](../../tests/machine-attach-v2.test.ts), [machine process and socket](../../tests/machine-attach-v2-e2e.test.ts), [signals](../../tests/wrapper-signal-forwarding.test.ts) |
| R09 | [sessions](../../src/sessions.ts), [server](../../src/server.ts), [recovery](../../src/recovery.ts), [CLI](../../src/cli.ts) | [root](../../tests/pty-root.test.ts), [display name](../../tests/display-name.test.ts), [status](../../tests/stats-cli.test.ts), [list purity](../../tests/list-purity.test.ts), [recovery](../../tests/recovery.test.ts) |
| R10 | [sessions](../../src/sessions.ts), [events](../../src/events.ts), [recovery](../../src/recovery.ts), [protocol](../../src/protocol.ts) | [atomic writes](../../tests/atomic-writes.test.ts), [metadata events](../../tests/metadata-events.test.ts), [events](../../tests/events.test.ts), [recovery](../../tests/recovery.test.ts), [disk layout](../../tests/disk-layout-docs.test.ts) |
| R11 | [CLI](../../src/cli.ts), [client](../../src/client.ts), [client API](../../src/client-api.ts), [remote](../../src/remote.ts), [testing API](../../src/testing/index.ts) | [help](../../tests/help.test.ts), [completions](../../tests/completions.test.ts), [interactive attach](../../tests/attach-no-restart.test.ts), [remote](../../tests/remote-fabric.test.ts), [screenshots](../../tests/screenshot.test.ts), [keys](../../tests/keys.test.ts) |

`node scripts/verify-docs.ts --vrs-only` validates this two-document shape,
sequential requirement IDs, links, and complete requirement references.
