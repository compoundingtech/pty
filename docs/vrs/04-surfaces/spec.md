# Surface specification

This document specifies how the product and package surfaces compose lower
layers. It builds on [requirements.md](./requirements.md).

## Status

Active. The TUI toolkit is alpha; the package as a whole is pre-1.0.

## Surface map

```text
@compoundingtech/pty
  +-- bin/pty + dist/cli.js       operator CLI
  +-- /client                     session and registry client API
  +-- /server                     embeddable runtime
  +-- /protocol                   packet codec
  +-- /testing                    real-PTY test sessions
  +-- /tui                        alpha terminal UI toolkit
  `-- /keys                       key-name codec
```

The CLI imports the same modules exported to embedders. `Session.server`,
`SessionConnection`, and `attachPty` consume the same ordered geometry and
terminal frames as the CLI. No surface owns a second session state machine
(PTY.SURF-R01–R02).

## Command routing

The CLI schema defines commands, positionals, flags, repeatability, value mode,
and choices. Parsing, help, and generated completions consume that shape.
Before machine attach resolves a session, it validates that the requested
descriptor is an open writable inherited fd greater than or equal to 3.
Reference-taking commands use stable-id first, unambiguous-display-name second
resolution (PTY.SURF-R03, R05).

## Remote composition

`remote-serve --stdio` receives a newline-delimited JSON control request from a
trusted fabric route. `list` returns JSON. A routed command resolves the remote
reference, replies with an acknowledgment, then bridges residual and subsequent
bytes bidirectionally to the local session socket. The session protocol remains
unchanged across the bridge. Interactive remote attach may reconnect by dialing
a new route; a resolved session absence ends that loop (PTY.SURF-R06).

The listening-socket form is transitional. The on-demand stdio form leaves
persistence and roaming to fabric and does not introduce a central pty daemon.

## Child specifications

- [CLI and package](./01-cli-package/spec.md) owns the executable artifact,
  process boundary, descriptors, signals, help, and completions.
- [libraries](./02-libraries/spec.md) owns public module boundaries and the
  testing/TUI embedding contracts.
