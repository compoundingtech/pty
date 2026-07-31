# Session runtime ontology

Root terms are inherited from [../ontology.md](../ontology.md).

## Language

**Launch definition**:
The complete persisted input needed to start the child equivalently: command,
arguments, display command, working directory, initial geometry, lifetime
flags, tags, display name, and environment policy.

**Inherited environment policy**:
The ordered transformation `base -> removals -> assignments -> runtime
invariants`. It is distinct from an exact environment map.

**Ordinary environment key**:
A caller-owned child variable whose assigned value, including an empty value,
is preserved. `PTY_SESSION` is runtime identity and `TERM` is terminal
capability metadata, so neither is ordinary launch data.

**Permanent session**:
A preserved session tagged `strategy=permanent` whose absent or dead runtime is
eligible for explicit `gc` reconciliation.
_Avoid_: immortal session; abandonment and flapping policy can still stop it.

**Reap**:
Removal of a finished session's registry artifacts according to exit policy.
_Avoid_: kill (termination and removal are distinct lifecycle actions).

**Generation-owned cleanup**:
Deletion that proceeds only while the target metadata still belongs to the
daemon generation or observation used to authorize it.
