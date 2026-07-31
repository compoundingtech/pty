# pty intuition

*For: maintainers and embedders · Assumes: Unix PTYs and terminal escape
sequences · Covers: the system-wide mental model*

The durable thing is a **session**, not the terminal window connected to it.
One daemon owns one child PTY and continuously parses the child's bytes into a
headless terminal. Clients come and go. The daemon keeps the process and the
screen model alive between them.

```text
child <-> PTY <-> per-session runtime <-> zero or more clients
                         |
                         +-> atomic metadata + ordered events
```

Reattachment is state transfer, not log replay. A client first learns the
effective grid, then receives one screen image representing everything before
an exact parser cut, then receives bytes produced after that cut. This is why a
client can reconstruct colors, cursor position, alternate-screen applications,
and concurrent output without guessing.

The filesystem registry is a separate projection. It gives sessions durable
identity, restart inputs, lifecycle guards, and cheap observation. It does not
replace the live stream, and the live stream does not become identity.

The CLI and libraries are adapters around those same two boundaries. A useful
test of any new surface is therefore simple: does it preserve session lifetime,
ordered terminal state, effective geometry, and generation-safe lifecycle—or
does it explicitly decline the capability?
