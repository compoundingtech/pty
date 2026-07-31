# CLI and package specification

This document specifies the executable distribution boundary. It builds on
[requirements.md](./requirements.md) and the parent
[surface specification](../spec.md).

## Status

Active.

## Entrypoint

```text
OS exec bin/pty
  -> set process title
  -> verify ../dist/cli.js exists
  -> dynamic import in the same process
  -> CLI main reads original argv and inherited fds
```

There is no wrapper child. Consequently a caller's fd 3, controlling terminal,
process id, and signals are properties of the process actually executing the
CLI (PTY.SURF.CLI-R01–R03).

## Machine attach

`attach --attach-stream-fd-v1 <fd> <ref>` requires an inherited fd greater than
or equal to 3 and validates it with a zero-byte write before session resolution.
It retains stdin/stdout for raw mode, requested size, input, and resize events.
It writes reframed terminal-event packets only to the fd, diagnostics only to
stderr, and no terminal bytes to stdout (PTY.SURF.CLI-R04–R05).

The adapter owns its stream view with `autoClose: false`: session exit writes
framed `EXIT`; Ctrl-\\ writes the existing empty `DETACH` frame and sends the
server-side detach request. Either outcome is flushed before clean completion,
while the caller still owns the descriptor. Descriptor errors, unsupported
initial order, transport loss, reconnect give-up, and EOF without either
terminal outcome are non-zero failures. An administrative session destruction
is such an outcome-less failure unless the adapter first observed process
`EXIT`; it does not synthesize a third outcome.

## Schema and completions

The command schema distinguishes free-valued and choice-valued options.
Completion generation uses this arity to make `--attach-stream-fd-v1` consume a
following value in bash, fish, and zsh. Tests execute generated bash completion
behavior in addition to checking text output (PTY.SURF.CLI-R06).
