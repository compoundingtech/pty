# Session runtime specification

This document specifies the per-session execution engine. It builds on
[requirements.md](./requirements.md).

## Status

Active.

## Runtime structure

```text
detached Node daemon
  +-- node-pty child process
  +-- xterm-headless + SerializeAddon
  +-- Unix socket server
  +-- event writer
  `-- generation-owned registry artifacts
```

`spawnDaemon` serializes `ServerOptions` into the daemon launch, starts a
detached Node process, and waits for its socket with a bounded failure path.
`PtyServer` spawns the command under a real PTY, writes child output to the
headless terminal and stream clients, records terminal events, and owns cleanup
for its generation (PTY.RUN-R01–R03).

The child is launched through `/bin/sh -c 'exec "$@"'` so scripts, symlinks,
and shebangs follow shell execution semantics without leaving an intermediate
shell process.

## Terminal pipeline

```text
child output
   |---> xterm parser ---> serializable terminal state
   `---> ordered client broadcast

client DATA ---> child PTY input
effective size ---> xterm resize ---> child PTY resize
```

The runtime tracks mode sequences that a screen serialization alone cannot
re-establish reliably and prefixes them when reconstructing a client. It also
derives bell, title, notification, focus, and cursor events from terminal
output (PTY.RUN-R02).

## Launch and lifecycle refinements

- [launch context](./01-launch-context/spec.md) specifies command, geometry,
  environment, and restart preservation (PTY.RUN-R04–R05).
- [lifecycle](./02-lifecycle/spec.md) specifies exit, reap, permanent respawn,
  generation ownership, and explicit cleanup (PTY.RUN-R06–R07).

The runtime does not decide how users discover a session, how a client renders
it, or which product should supervise it. Those belong to the registry, stream,
and surface subsystems.
