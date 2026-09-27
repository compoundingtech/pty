# Registry writes — Requirements

## Context

This node defines when a Rust session daemon rewrites its session record `<root>/<id>.json`, and where it keeps the facts that change while the session runs. The record's fields, key order and unknown-field preservation are the Node contract tracked in [docs/parity.md](../../parity.md) section 5. How a caller reads the registry is [03-registry-reads](../03-registry-reads/requirements.md). The rationale and the consumer audit are in [decision 0015](../../decisions/0015-the-record-changes-only-when-a-fact-does.md).

## Assumptions

- **PTY.REGW-A01 Observers stat the record:** A viewer or supervisor that shows a set of sessions (`PTY.REG-A01`) stats each shown session's `<id>.json` on its cadence and re-reads that session's STATUS only when the record's identity (mtime and size) changed.
- **PTY.REGW-A02 Busy sessions print continuously:** A coding-agent session prints several times a second for long periods (spinners, clocks) while no client comes or goes.
- **PTY.REGW-A03 Mixed registry:** Node and Rust daemons and CLIs share one root; a Node daemon keeps its own write behaviour.

## Acceptable Tradeoffs

- **PTY.REGW-T01 Stamp outside the record:** While the child runs, a reader of `<id>.json` alone sees no `lastOutputAtMs`; it reads the sidecar for it.
- **PTY.REGW-T02 Lost client write:** A client-generation write that cannot take the metadata lock within its retry budget is dropped; the next client change carries a newer counter.
- **PTY.REGW-T03 Node leftovers:** A Node CLI that removes a Rust session leaves its sidecar behind; the sidecar's generation keeps it from being believed.

## Requirements

### Must not rewrite the record for output

- **PTY.REGW-R01 Stable while unobserved:** While no client attaches, resizes or leaves and no lifecycle, tag or name change happens, `<id>.json` stays byte-identical however much the child prints.
- **PTY.REGW-R02 Durable output stamp:** The newest output time is persisted, at most once a second while output flows, where a reader can find it without the daemon, and it is carried into the exit record.
- **PTY.REGW-R03 Stamp belongs to its generation:** A reader never credits one daemon generation's output to another generation under the same id.
- **PTY.REGW-R04 Nothing left behind:** Removing a session through any Rust cleanup path removes its output stamp; an exit record that carries the final stamp leaves no separate stamp behind.

### Must rewrite the record for client facts

- **PTY.REGW-R05 Client changes are visible in the record:** A writable client attaching, resizing to a new size or leaving (by DETACH or by closing its socket), and any change of the negotiated session size, each increase a per-session counter and rewrite `<id>.json` when the metadata lock is available within the retry budget (`PTY.REGW-T02`). Consecutive changes while the lock is held may coalesce into one write carrying the newest counter.
- **PTY.REGW-R06 Observation writes nothing:** PEEK, STATUS and send connections, and a RESIZE that repeats a client's current size, do not rewrite `<id>.json`.
