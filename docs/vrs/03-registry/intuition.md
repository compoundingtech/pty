# Registry intuition

*For: integrators and fast-path readers · Assumes: the root session model ·
Covers: durable identity without turning files into the live protocol*

The registry answers “what sessions exist, how were they launched, and what
happened?” The socket answers “what is happening in this terminal now?” Keeping
those questions separate makes both boundaries simpler.

```text
registry JSON/events       live socket
identity + history         ordered terminal state
cheap external reads       attach/input/geometry
```

Atomic rename means an external reader never sees half a metadata document, but
it does not turn independent writers into a transaction. Generation checks
solve a different race: they prevent an old owner from deleting a new session
that reused the same stable id.

The stable id is deliberately boring because it reaches filesystem and kernel
socket paths. Display names and tags carry richer presentation and grouping,
while explicit ambiguity and `PTY_ROOT` keep them from silently becoming the
wrong kind of identity boundary.
