# Surface ontology

Root terms are inherited from [../ontology.md](../ontology.md).

## Language

**Package entrypoint**:
The shipped `bin/pty` executable that selects compiled CLI code without adding
a second process or alternate argument contract.

**Controlling terminal**:
The invoking terminal retained on stdin/stdout for raw input, size, and resize
events even when screen events are emitted on a machine descriptor.

**Remote route**:
A fabric-provided ordered stream bridged to one ordinary local session socket.
It is transport composition, not a second session protocol.

**Testing session**:
A real PTY-backed test handle using either a direct child backend or the
persistent server backend.

**Terminal screenshot**:
A point-in-time projection of a testing terminal as trimmed lines, joined plain
text, and ANSI serialization. It is not the live protocol's screen baseline.

**TUI toolkit**:
The alpha package surface for terminal layout, rendering, input, widgets, and
attaching a session as a pane.
