# Registry ontology

Root terms are inherited from [../ontology.md](../ontology.md).

## Language

**Tier 1 artifact**:
A documented external-readable storage surface whose changes are called out for
version-pinned consumers: session metadata or events.

**Tier 2 artifact**:
An implementation-owned registry artifact that may move without storage-format
compatibility: socket, pid, lock, theme, or gc log.

**Running**:
A session whose daemon process is alive and whose socket is reachable.

**Exited**:
A non-live session with recorded exit details.

**Vanished**:
A non-live session without recorded exit details because the daemon could not
finalize them.
_Avoid_: exited; the cause and code are unknown.

**Presentation reference**:
A display-name lookup accepted only when exactly one session matches. It is a
convenience selector, not durable identity.

**Hard isolation**:
Selection of a distinct registry root. Tag filtering within a registry is soft
scoping, not isolation.
