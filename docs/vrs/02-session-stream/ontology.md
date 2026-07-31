# Session stream ontology

Root terms are inherited from [../ontology.md](../ontology.md).

## Language

**Requested geometry**:
The rows and columns most recently advertised by one writable client. It is an
input to negotiation, not necessarily the terminal's current grid.

**Effective geometry**:
The shared rows and columns selected across all connected writable clients and
applied to both the child PTY and headless terminal.

**Readonly client**:
A `PEEK` connection that receives terminal state but cannot send child input or
participate in geometry negotiation.
_Avoid_: unauthorized client; readonly is not an access-control claim.

**Synchronization generation**:
One `ATTACH` or `PEEK` request's ordered transfer of geometry, baseline, and
post-cut events. A later request on the socket invalidates it.

**Screen baseline**:
A serialized terminal state representing all parser input before one exact cut.
It is the starting state for later data, not a periodically sampled screenshot.

**Machine attach stream v1**:
The versioned CLI contract that reframes terminal events unchanged onto a
caller-owned inherited file descriptor while retaining the invoking terminal
for input and requested geometry.

**Machine attach outcome**:
Exactly one terminal frame before clean EOF. `EXIT` means the session process
ended; `DETACH` means this attach client intentionally detached. EOF without an
outcome is truncation or transport loss.
