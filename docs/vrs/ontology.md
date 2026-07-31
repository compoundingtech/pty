# pty ontology

Terms defined here are inherited by every descendant. Child ontologies add
only local terms.

## Language

**Session**:
A stable-id execution record consisting of a child process, its PTY and
terminal state, and its registry artifacts. A session can outlive every client.

**Session runtime**:
The per-session daemon, child PTY, headless terminal emulator, and lifecycle
logic that maintain a session independently of clients.
_Avoid_: server (ambiguous with the child command), central daemon.

**Client**:
One socket connection observing or interacting with a session. A client is
ephemeral and is never session identity.

**Terminal state**:
The ordered result of parsing the child's terminal byte stream: grid, style,
cursor, terminal modes, alternate screen, and scrollback.
_Avoid_: log (a log cannot reconstruct this state).

**Registry**:
One directory tree selected by `PTY_ROOT`, containing the identities and
artifacts for a set of sessions. Distinct registries are hard isolation;
filtered tags are not.

**Stable id**:
The immutable, path-safe session name used for socket and registry filenames.
_Avoid_: display name, label.

**Display name**:
Mutable, non-unique presentation metadata. It resolves as a convenience
reference only when exactly one session matches.

**Generation**:
An opaque token identifying one daemon's ownership of a session's mutable
registry artifacts. It prevents stale cleanup from deleting a replacement.

**Surface**:
A supported way to invoke or embed the system: CLI, package entrypoint, exported
library, testing API, or remote route.
