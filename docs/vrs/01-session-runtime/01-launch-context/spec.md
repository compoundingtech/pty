# Launch context specification

This document specifies launch-definition assembly and persistence. It builds
on [requirements.md](./requirements.md) and the parent
[runtime specification](../spec.md).

## Status

Active.

## Environment modes

```text
exact mode:       copy env -> PTY_SESSION -> normalize TERM if absent or empty

inherited mode:   process.env
                    -> remove PTY_SERVER_CONFIG
                    -> unsetEnv[]
                    -> extraEnv{}
                    -> PTY_SESSION
                    -> normalize TERM if absent or empty

isolated mode:    allowlisted process.env + LC_*
                    -> unsetEnv[] -> extraEnv{}
                    -> PTY_SESSION -> normalize TERM if absent or empty
```

Passing `env` together with `isolateEnv`, `extraEnv`, or a non-empty `unsetEnv`
is invalid in both `SpawnDaemonOptions` and `ServerOptions`
(PTY.RUN.ENV-R02–R04).

The environment map is exact for ordinary caller-owned keys: removal removes,
assignment wins, and empty values such as `NO_COLOR=` remain empty. Two keys are
runtime-owned instead. `PTY_SESSION` is forced to the stable session id, and
node-pty interprets `TERM` as its terminal-name capability. The runtime selects
`xterm-256color` when it is absent or empty; a nonempty value is preserved.

`SessionMetadata` stores `rows`, `cols`, `ephemeral`, `isolateEnv`, `extraEnv`,
`unsetEnv`, and exact `env` alongside the command fields. Operator restart and
metadata fallback pass them back to `spawnDaemon`. Manifest-backed permanent
respawn re-reads command, cwd, tags, and assignments from `pty.toml`; unreadable
or absent definitions use the recorded values (PTY.RUN.ENV-R01, R05–R06).

The operator restart path may scrub explicitly named variables from the
daemon's own inherited environment before spawn. This prevents the restarter's
ambient identity from becoming undeclared child state; it does not alter the
persisted child policy.
