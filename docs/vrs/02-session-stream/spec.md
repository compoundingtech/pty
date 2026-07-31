# Session stream specification

This document specifies the binary session protocol and its client modes. It
builds on [requirements.md](./requirements.md).

## Status

Active.

## Packet framing

```text
+------------+----------------+--------------------+
| type: u8   | length: u32 BE | payload: N bytes   |
+------------+----------------+--------------------+
```

`PacketReader` buffers partial frames and emits complete packets in order. A
length greater than `32 * 1024 * 1024` poisons the reader and requires the
connection to be destroyed (PTY.STREAM-R04).

| Type | Id | Direction | Payload |
| --- | ---: | --- | --- |
| `DATA` | 0 | both | terminal bytes |
| `ATTACH` | 1 | client to runtime | rows `u16BE`, cols `u16BE` |
| `DETACH` | 2 | client to runtime; machine adapter to consumer | empty |
| `RESIZE` | 3 | client to runtime | rows `u16BE`, cols `u16BE` |
| `EXIT` | 4 | runtime to client | signed exit code `i32BE` |
| `SCREEN` | 5 | runtime to client | serialized ANSI terminal state or requested plain text |
| `PEEK` | 6 | client to runtime | flags: plain bit 0, full-scrollback bit 1 |
| `STATUS` | 7 | both | empty request or JSON response |
| 8–9 | — | — | reserved |
| `GEOMETRY` | 10 | runtime to client | effective rows `u16BE`, cols `u16BE` |

Bounded unknown types are ignored by the runtime. Legacy size/exit payload
decoders retain historical fallback values; commands with stronger contracts
validate their required shape before acting (PTY.STREAM-T02).

`GEOMETRY` is an additive type at id 10. Clients predating it ignore the
bounded unknown packet and continue with their historical raw `SCREEN`/`DATA`
behavior. Machine attach v1 instead requires geometry and fails explicitly
against an older daemon that cannot establish its reconstruction contract.

## Connection roles

```text
new connection
  +-- ATTACH -> replace role with writable, contribute geometry, accept input
  +-- PEEK   -> replace role with readonly, remove geometry constraint
  `-- STATUS -> observation only, does not join geometry
```

An `ATTACH` carrying complete geometry or a recognized `PEEK` replaces rather
than accumulates role state and starts a fresh synchronization generation. A
malformed `ATTACH` leaves both role and generation unchanged. Peek retains its
historical optional/extensible flag handling rather than imposing a new strict
payload shape. `DETACH` ends only the connection. Socket close removes its
writable geometry constraint and may change the effective grid
(PTY.STREAM-R05).

## Ordered state transfer

The [synchronization specification](./01-synchronization/spec.md) owns initial
and reconnect ordering. The [geometry specification](./02-geometry/spec.md)
owns requested/effective size negotiation. Together they establish:

```text
GEOMETRY -> SCREEN -> DATA* -> EXIT?
```

## Human and machine attach

Ordinary attach clears the user's terminal before `SCREEN`, writes subsequent
`DATA` to stdout, forwards stdin and stdout resize events, and sanitizes modes
on detach/exit. Machine attach receives the same socket packets and reframes
the four server event types to `--attach-stream-fd-v1`; on local detach it emits
the existing empty `DETACH` frame as the terminal outcome. Stdout remains the
controlling TTY and receives no screen bytes (PTY.STREAM-R07).

The machine state machine requires `GEOMETRY`, permits further geometry updates
while awaiting `SCREEN`, then permits live events. Each reconnect resets it to
the initial state. A local detach may emit its terminal `DETACH` outcome while
the initial baseline is still pending. Descriptor backpressure pauses the
source socket. The caller retains descriptor ownership; the attach adapter ends
its stream view, not the underlying fd. Exactly one outcome terminates a clean
stream: framed `EXIT` means the session ended, while framed `DETACH` means this
client intentionally detached. EOF without either outcome, including transport
loss, reconnect give-up, or abrupt administrative session destruction before a
process `EXIT` was observed, is truncation. Administrative destruction does not
introduce a third clean outcome (PTY.STREAM-R06–R07).
