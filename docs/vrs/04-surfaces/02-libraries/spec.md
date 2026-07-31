# Library specification

This document specifies exported embedding surfaces. It builds on
[requirements.md](./requirements.md) and the parent
[surface specification](../spec.md).

## Status

Active. The TUI module remains alpha.

## Public modules

| Export | Contract |
| --- | --- |
| `/client` | session discovery, lifecycle, connection, attach/peek/send/stats, events, ptyfile helpers |
| `/server` | embeddable `PtyServer` runtime |
| `/protocol` | packet constants, codecs, bounded streaming reader |
| `/keys` | named key and sequence parsing |
| `/testing` | real-PTY `Session` and screenshot types |
| `/tui` | alpha terminal rendering/input/widgets and PTY panes |

`tsc -p tsconfig.build.json` produces `dist` JavaScript and declarations while
rewriting source `.ts` imports to `.js`. Development source remains runnable
with Node type stripping (PTY.SURF.LIB-R01).

## Testing backends

```text
Session.spawn  -> direct node-pty child -> local headless terminal
Session.server -> PtyServer -> SessionConnection -> local headless terminal
```

Both backends send real key bytes and parse real terminal output. Screenshots
project the active terminal as lines/text/ANSI. `waitForText`, `waitForAbsent`,
and general `waitFor` repeatedly inspect that state until success or a bounded
diagnostic failure (PTY.SURF.LIB-R03–R04).

Server mode supports attach, reconnect, peer clients, and resize. A `GEOMETRY`
event resizes the receiving terminal before affected screen/data; public
`rows`/`cols` report effective rather than merely requested dimensions.
`SessionConnection` and `attachPty` preserve the same rule
(PTY.SURF.LIB-R02, R05).

## Daemon launch strategies

An explicit server-module override wins, then an installed sibling
`dist/server.js`, then delegation to the installed `pty` CLI for bundled
consumers. Direct strategies carry serialized launch options. CLI fallback is
limited to the options its command surface can express and rejects an exact
environment map instead of silently changing its meaning
(PTY.SURF.LIB-R06).
