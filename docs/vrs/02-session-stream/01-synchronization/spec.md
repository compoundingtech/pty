# Synchronization specification

This document specifies the initial terminal parser cut. It builds on
[requirements.md](./requirements.md) and the parent
[stream specification](../spec.md).

## Status

Active.

## Per-client state machine

```text
ATTACH / PEEK
     |
     v
 settling -- parser marker enqueued --> cutting -- marker callback --> live
     |                                      |                       |
 data is represented in SCREEN         queue DATA/EXIT          write directly

complete ATTACH / recognized PEEK: replace role and synchronization generation
malformed ATTACH: preserve role and generation
```

The runtime emits current or newly negotiated `GEOMETRY` on admission. An
attach affected by recent resize may remain `settling` for the redraw bound.
The eventual call to `terminal.write("", callback)` establishes the exact cut
(PTY.STREAM.SYNC-R01–R03).

In the callback, the current generation writes `SCREEN`, transitions to live,
flushes `postCutPackets` in source order, and synthesizes `EXIT` only when the
runtime already exited and no queued exit exists. `node-pty` exposes its public
exit after draining PTY data, so queued `DATA` precedes queued `EXIT`
(PTY.STREAM.SYNC-R04).

Each complete attach or recognized peek replaces readonly/writable state and
increments `initialScreenGeneration`. Delayed callbacks compare that token
before writing, so stale baselines and queues cannot leak across role changes or
reconnect generations. Attach geometry validation precedes both mutations;
peek preserves its optional flag compatibility (PTY.STREAM.SYNC-R05–R06).
