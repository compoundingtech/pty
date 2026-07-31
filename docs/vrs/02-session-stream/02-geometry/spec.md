# Geometry specification

This document specifies shared terminal-size negotiation. It builds on
[requirements.md](./requirements.md) and the parent
[stream specification](../spec.md).

## Status

Active.

## Negotiation

For writable client set `W`:

```text
effectiveRows = min(client.rows for client in W)
effectiveCols = min(client.cols for client in W)
```

Rows and columns are minimized independently. A client requesting `60x80` and
one requesting `30x200` therefore produce `30x80`. Readonly clients are absent
from `W` (PTY.STREAM.GEO-R01–R03).

## Change order

```text
writable membership/request change
  -> resize headless terminal
  -> broadcast GEOMETRY to attached and readonly clients
  -> resize child PTY (SIGWINCH/redraw may follow)
  -> deliver resulting terminal DATA
```

The headless terminal is resized before the child so it is ready to parse the
redraw. Geometry is enqueued before that redraw can become stream data
(PTY.STREAM.GEO-R04).

`SessionConnection`, server-mode `Session`, and `attachPty` consume geometry as
an event, update effective rows/columns, and resize their emulator before later
screen/data. Calling `resize` changes the local requested size; a smaller peer
can keep effective size below it (PTY.STREAM.GEO-R05).

When `W` becomes empty, negotiation performs no resize. The last effective
terminal and PTY grid remains the baseline for readonly observation and the
next attach (PTY.STREAM.GEO-R06).
