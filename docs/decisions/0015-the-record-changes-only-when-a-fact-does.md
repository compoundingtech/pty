# 0015 — The record changes only when a fact does

**Status:** accepted

The requirements this serves are
[docs/vrs/04-registry-writes/requirements.md](../vrs/04-registry-writes/requirements.md);
the mechanism is in [its spec](../vrs/04-registry-writes/spec.md). Issue:
compoundingtech/pty-rust#45.

**Node behavior.** The Node daemon stamps `lastOutputAtMs` on every output
chunk and persists it into `<id>.json` with a one-second trailing debounce
(`src/server.ts` `scheduleActivityPersist`, R14 of the Node repository's
`docs/vrs/requirements.md`, at f005432). A Node daemon also advertises
`recovery{}`, whose `metadataRevision` is a hash of the whole record
(`src/recovery.ts` `metadataRevision`), so each of those writes rewrites
`recovery.metadataRevision` too. ATTACH stamps `lastAttachAt`; detach and
resize write nothing.

**Rust behavior.**

- While the child runs, the daemon persists `lastOutputAtMs` to
  `<root>/.activity/<id>.json`, `{"generation":"…","lastOutputAtMs":N}`, with
  the same one-second debounce. `<id>.json` does not carry the field until
  exit. The exit write folds the final stamp into the record as before, then
  removes the sidecar. Every Rust cleanup path removes the sidecar with the
  session.
- The daemon keeps a counter, `clientGeneration`, in `<id>.json`. It bumps
  the counter in one generation-fenced write when the client facts `pty stats`
  reports change: a writable client attaches (that write also stamps
  `lastAttachAt`), resizes to a new size, or leaves by DETACH or by closing its
  socket, or the negotiated session size changes. PEEK, STATUS and `pty send`
  connections do not bump it. A write that meets a held metadata lock retries
  every 10 ms for up to 2 s.
- The Rust daemon never writes `recovery{}` (decision 0005), so it has no
  `metadataRevision` churn to remove.

The result: a session that no client comes to or leaves writes `<id>.json`
only at publication, on tag and name changes, and at exit, however much it
prints.

**Why.** An observer that wants to know when to re-read a session's STATUS
could stat `<id>.json`, but the record changed about once a second for every
busy session. On dev3 on 2026-09-26, sampling all root-level metadata files
at 4 Hz for 30 s, the changed keys were `lastOutputAtMs` 675 times,
`recovery.metadataRevision` 87 times (Node daemons only), and one-off
lifecycle writes: `exitedAt` 6, `exitCode` 2, `lastLines` 2, `tags` 2,
and one restart's `generation`, `daemonPid`, `daemonStartToken` and
`createdAt`. An inotify census counted about 1,371 record rewrites a minute
(#45). The noise came from output, which no reader of the record waits on. The
facts a reader does wait on, which clients are attached and at what size, did
not change the record at all, except for the attach stamp.

**Consumer audit.** Every reader of the fields that change while a session is
running:

| Consumer (revision) | `lastOutputAtMs` | `recovery.metadataRevision` | Effect of this record |
| --- | --- | --- | --- |
| pty-rust (a2bfa66) daemon | writes it (debounced), carries it into the exit record | preserves, never writes (0005) | writes the running stamp to the sidecar instead |
| pty-rust CLI (`list`, `list --json`, `stats`, `metadata`) | no command prints or reads it | reads `recovery.processStartToken` for liveness, not the revision | none |
| pty-rust conformance (`output_activity.rs`) | reads the file directly | – | reads the way a consumer should: exit record, then sidecar of the same generation, then record |
| Node pty (f005432) daemon | writes it (debounced), carries it into the exit record | writes it on every metadata write | – |
| Node pty CLI | type only (`src/sessions.ts`); `list --json` omits it | `pty recover` compares it | a Node CLI reading a live Rust session sees no stamp until exit; `recover` is unaffected (Rust never advertises recovery) |
| st2 (05b18900) | no reader; `newest_activity_ms` uses message/status timestamps, and `session_liveness_in` reads `<id>.pid`, not metadata | no reader | none |
| dotfiles fractal (9014a23) | no reader; reads `generation` and tags through `pty_core::registry::SessionMetadata`, and STATUS | no reader | can gate STATUS reads on a stat of `<id>.json` |
| dotfiles vista `pty.provider.ts` | no reader; sockets and the `pty` CLI | no reader | none |
| dotfiles elsewhere | none outside `context/`; `archive/oi` has an unrelated `lastOutput` summary field | none | none |

The one program that ever read `lastOutputAtMs` as activity was an st2
experiment that mapped a recent stamp to "active", and it was rejected: an
observer's own attach redraw moved the stamp and flipped the answer
(schickling/dotfiles,
`context/agent-ecosystem/03-coding-agents/20-fractal/.experiments/2026-08-27-semantic-activity-observer-effect.md`).
No consumer needs the running stamp inside `<id>.json`.

**Options compared.**

| Option | Record stable while output flows | Stamp readable without the daemon | Downstream change |
| --- | --- | --- | --- |
| Keep the field in the record, debounce to 30–60 s | no, one write per busy session per window | yes, but up to a minute stale | none |
| Write the stamp only at exit | yes | only after exit; lost if the daemon is killed | none |
| Serve the stamp over STATUS | yes | no | a new `StatsResult` field breaks Rust struct literals in embedders |
| Sidecar `<id>.activity` in the root | yes | yes | root watchers still see its renames; Node listing must learn a suffix |
| **Sidecar `.activity/<id>.json`, generation-tagged** | **yes** | **yes** | **none** |

The subdirectory follows `.recovery/`: neither listing looks below the root's
own entries, and a watcher of the root does not see the sidecar's renames. The
generation tag keeps a replacement daemon under the same id from being
credited with the old daemon's output. Writing the sidecar in place instead of
temp-and-rename was rejected: a reader could see a torn number, and the
subdirectory already hides the renames.

For the client signal, a separate `<id>.clients` file was rejected because an
observer already stats `<id>.json`, and one stat per session is the point.
Relying on the record's mtime alone was rejected as the only signal: a
counter in the content tells a reader which change it has seen, independent
of timestamp granularity. Bumping on PEEK or STATUS was rejected: observing a
session must not change its record, or observers would wake each other.

**Client effect.**

- `<id>.json` of a running Rust session has no `lastOutputAtMs`. Absence was
  already valid ("never a claim of idleness"). Read the stamp with
  `pty_core::registry::last_output_at_ms(name, &metadata)`, or by the same
  precedence by hand: an exited record answers for itself, otherwise the
  sidecar whose `generation` equals the record's, otherwise the record.
- `<id>.json` carries `clientGeneration` after the first client change. It
  restarts with each daemon generation; compare it together with
  `generation`. A Node daemon never writes it.
- Mixed registry: a Node `pty rm` or `gc` of a Rust session leaves
  `.activity/<id>.json` behind, because Node does not know the file. The
  generation tag keeps it from being believed, and a Rust cleanup of the same
  id removes it.
- Rust daemons started before this change keep writing the stamp into the
  record until they are replaced; the reader precedence covers them.
- Node daemons keep rewriting their records (`lastOutputAtMs` and
  `recovery.metadataRevision`) until they are replaced by Rust daemons. That
  churn is Node's and is not changed here.
- `<id>.events.jsonl` rewrites, the other half of #45, are not changed here.

**Test.** `crates/pty-conformance/tests/output_activity.rs`:
`output_leaves_the_record_alone_rust` / `output_rewrites_the_record_node`
(gated pair), `the_exit_record_takes_over_from_the_sidecar`,
`a_recorded_exit_is_not_rewritten_during_shutdown`, and the cross-binary
stamp contract tests, which now read through the consumer precedence.
`crates/pty-conformance/tests/client_generation.rs`:
`attach_resize_and_detach_each_bump_the_generation_rust` /
`client_changes_write_no_generation_node` (gated pair),
`a_resize_to_the_same_size_leaves_the_record_alone`,
`peek_and_stats_leave_the_record_alone`. Reader precedence:
`crates/pty-core/src/registry/activity.rs` unit tests.

**Measurement.** In separate test roots, each binary ran one silent `cat`
session and one detached `sh` session that printed every 50 ms.
`inotifywait -m -r -e moved_to -e close_write` counted writes over the same
60-second window, after a 3-second startup settling period:

| Artifact | Before (a2bfa66) | After |
| --- | ---: | ---: |
| silent `<id>.json` renames | 0/min | 0/min |
| busy `<id>.json` renames | 58/min | 0/min |
| busy `.activity/<id>.json` renames | 0/min | 58/min |
| root-level write/rename events (both sessions) | 174/min | 0/min |

The sidecar's own directory still sees 116 events/min (a close-write and a
rename per stamp); a watcher of the registry root sees none. The events log
in this test emitted no semantic events, so neither binary rewrote it.

**Migration / negotiation.** None required. A reader that wants a running
Rust session's output stamp switches to `last_output_at_ms` or the precedence
above. An observer that wants a cheap change signal stats `<id>.json` (mtime
and size) for the sessions it shows and re-reads STATUS only when that
changes.
