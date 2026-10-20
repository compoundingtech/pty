# 0018 — Logical session IDs do not consume the socket path budget

**Status:** accepted

**Node behavior.** The pinned Node reference rejects a session when
`<PTY_ROOT>/<id>.sock` exceeds its Unix socket path limit. A valid logical
ID can therefore fail before the daemon starts, even when its registry
sidecars fit in a directory entry.

**Rust behavior.** Keep the readable socket name when it fits. Otherwise,
use `@` followed by the first 128 bits of SHA-256 of the logical ID and
`.sock`, under the same registry root. The reserved `@` prefix cannot
alias a readable session ID. Keep the full logical ID in metadata and
sidecar names. The longest sidecar limits an ID to 209 ASCII characters;
reject 210 characters before creating session state. Reject roots that
cannot hold even the bounded socket name.

Once a socket is published, its filename is authoritative across registry
root aliases. Lookup recognizes real socket entries, not regular files or
symlinks in either namespace. If both socket candidates exist, the bounded
name wins and strict inventory reports the other entry as noncanonical.
When an alias makes a published readable address too long, connection uses
a shorter symlink target or canonical root without changing the filename.

**Why.** A transport address is not a session identity. Rejecting a logical
ID because of the combined root and ID length prevents long session names
from working on Darwin and unnecessarily constrains names on Linux. The
bounded name preserves the identity used by list, attach, stats and kill.
It does not move sockets outside the selected registry or relax sidecar
limits.

**Conformance.** `security_fixes::bounds_names_that_overflow_the_readable_socket_path`,
`socket_path_boundary::logical_id_longer_than_socket_budget_can_list_attach_query_and_stop`
and `socket_path_boundary::registry_sidecar_limit_accepts_boundary_and_rejects_overflow`
exercise this deliberate extension. The pinned Node reference rejects all
three at creation; Rust must serve and clean up each accepted session.
Record these as intended Node-red divergences, not Rust regressions.

**Compatibility.** Short socket paths keep their existing names. Long
IDs are a Rust extension; the Node reference need not be changed to match
this decision. Remove the divergence entries if Node adopts the same
logical-ID and sidecar boundary behavior, or if this extension is removed.
