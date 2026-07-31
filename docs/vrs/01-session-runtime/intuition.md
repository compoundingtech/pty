# Session runtime intuition

*For: runtime maintainers · Assumes: the root pty model · Covers: why one daemon
owns one session*

A session survives because its daemon is detached from the command that asked
for it. The daemon is deliberately small in responsibility: own one child PTY,
maintain one terminal model, serve clients, and finalize one lifecycle.

```text
launcher exits                 daemon exits
     |                              |
     v                              v
session continues        child result + policy finalize
```

Restart is not “run the same-looking command.” It replays a complete launch
definition. Environment removals matter as much as assignments: if `NO_COLOR`
was deliberately absent, inheriting it from a later operator shell would change
the program even though the command line stayed identical.

Cleanup is likewise about ownership, not filenames. A stale daemon may see the
same stable id after a replacement has started. The opaque generation turns
“delete this name” into “delete this name only if it is still mine.”
