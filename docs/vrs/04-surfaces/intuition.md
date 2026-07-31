# Surface intuition

*For: CLI and package maintainers · Assumes: runtime, stream, and registry
contracts · Covers: adding an interface without creating another system*

The safest surface is a thin adapter. A remote attach is still an attach. A
testing server is still a `PtyServer`. A TUI pane still consumes geometry before
screen bytes. This keeps difficult terminal ordering in one place.

```text
human CLI --------+
machine CLI ------+--> shared client/runtime modules --> one protocol
testing library --+
TUI pane ---------+
remote route -----+
```

Process boundaries are part of an interface. A wrapper that spawns a child and
inherits only fds 0–2 silently breaks a caller-owned fd 3 even if every protocol
unit test passes. Loading the compiled CLI in the package-entrypoint process is
both simpler and more faithful: argv, signals, controlling terminal, and all
inherited descriptors arrive at the actual CLI.
