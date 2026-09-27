# Registry writes — Specification

This document specifies when the Rust daemon rewrites `<id>.json`, the output-activity sidecar, and the client generation. It builds on [requirements.md](./requirements.md).

## Status

Active. Implemented in the Rust daemon and pty-core. The rationale, the consumer audit and the rejected options are in [decision 0015](../../decisions/0015-the-record-changes-only-when-a-fact-does.md).

## Scope

Defined here: which daemon events write `<id>.json`, the sidecar's path, schema, write and removal, the reader precedence for `lastOutputAtMs`, and the `clientGeneration` counter.

Not defined here: the record's other fields and key order ([docs/parity.md](../../parity.md) section 5), the metadata lock and the generation-fenced mutation (`crates/pty-core/src/registry/mutate.rs`), the events log `<id>.events.jsonl`, and the Node daemon's writes.

## Writers of `<id>.json`

| Event | Written fields | Guard |
| --- | --- | --- |
| Publication at daemon start | the whole record, Node publication order | creation lock |
| Tag, name, metadata patch (CLI) | the changed fields | metadata lock, CAS as requested |
| Writable client attaches | `lastAttachAt`, `clientGeneration` | metadata lock, `expectedGeneration` |
| Client facts change (below) | `clientGeneration` | metadata lock, `expectedGeneration` |
| Child exits | `exitCode`, `exitedAt`, `lastLines`, `lastOutputAtMs` | metadata lock, `expectedGeneration` |
| Child output | nothing (sidecar only) | – |

Every other daemon event leaves the record alone (`PTY.REGW-R01`).

## Output-activity sidecar

Path: `<root>/.activity/<id>.json`. The directory is created on first use with mode `0700`.

```json
{"generation":"4e244e77d9f468d07151fb9bd894abb5","lastOutputAtMs":1790441064309}
```

| Field | Type | Meaning |
| --- | --- | --- |
| `generation` | string | the daemon generation that saw the output |
| `lastOutputAtMs` | integer | unix milliseconds of that generation's newest output |

Write sequence (`PTY.REGW-R02`):

1. Each output chunk sets the in-memory stamp to now. If no persist is pending, one is scheduled 1 s out.
2. When it falls due and the child has not exited, the daemon publishes the sidecar by temp file and rename inside `.activity/`, under `<id>.lock` and only while the record names its generation (`publish_output_activity`). A held lock (`Busy`) retries 50 ms later; any other refusal drops the stamp, because the name belongs to a replacement. A write failure is ignored.
3. At exit the stamp goes into the exit record. When that write lands (`Changed` or `Unchanged`), the daemon removes the sidecar in the same critical section, before releasing `<id>.lock` (`record_exit_retiring_output_activity`, `PTY.REGW-R04`), so a replacement that reuses the id cannot publish its sidecar between the generation check and the unlink. Shutdown's later retry does not rewrite a record whose exit facts have not changed. A `GenerationMismatch` leaves the sidecar, because it may be a replacement's.

Removal with the session: `cleanup_all_while_locked` (and so `cleanup_all`, `cleanup_owned_all`, `cleanup`), `remove_session_generation`, and gc's raw-candidate cleanup unlink the sidecar with the other session files (`PTY.REGW-R04`).

### Reader precedence

`pty_core::registry::last_output_at_ms(name, &metadata)` uses the ambient root;
`last_output_at_ms_in(root, name, &metadata)` uses an explicit root, as with
`list_sessions_in`. Both apply the pure `newest_output_at_ms(&metadata, sidecar)`
(`PTY.REGW-R03`):

```
record has exitedAt                          → record.lastOutputAtMs
sidecar.generation == record.generation      → sidecar.lastOutputAtMs
otherwise                                    → record.lastOutputAtMs   (Node daemons, older Rust daemons)
```

## Client generation

`clientGeneration` is an unsigned integer in `<id>.json`. It is absent until the first change, starts from 1 in each daemon generation, and increases by one per change (`PTY.REGW-R05`).

Client facts: the negotiated `(rows, cols)` and, for every client that constrains the size (writable, has attached), `(connection id, rows, cols)` in connection order.

| Event | Bumps |
| --- | --- |
| ATTACH | always; the same write stamps `lastAttachAt` |
| RESIZE from a writable client | when the facts differ from the last bumped facts |
| A writable client leaves (DETACH or socket close) | when the facts differ |
| A writable client sends PEEK (becomes readonly) | when the facts differ |
| PEEK, STATUS or send from any other connection, and its close | never; the facts are not computed (`PTY.REGW-R06`) |

Write: one `mutate_metadata_under_lock` with `expectedGeneration` = the daemon's generation, setting `clientGeneration` to the in-memory counter and a pending `lastAttachAt`. On `Busy` or `Stale` the write is retried every 10 ms until 2 s after the first attempt, then dropped (`PTY.REGW-T02`). A later change resets the retry window and writes the newer counter. `Missing` and `GenerationMismatch` end the attempt.

## Edge cases

- **Replacement under the same id.** The old daemon's sidecar names the old generation and is ignored by readers. The old daemon's client writes fail their generation fence.
- **Exited session with attached clients.** A client leaving after exit still bumps the counter; the exit record is not otherwise touched.
- **Node daemon.** Writes `lastOutputAtMs` and `recovery.metadataRevision` into the record and never writes `clientGeneration`; `PTY.REGW-A03` excludes it from these requirements.

## Module map

| Concern | Source |
| --- | --- |
| Sidecar path | `crates/pty-core/src/registry/root.rs` — `output_activity_path` |
| Sidecar schema, write, read, removal, reader precedence | `crates/pty-core/src/registry/activity.rs` — `OutputActivity`, `publish_output_activity`, `record_exit_retiring_output_activity`, `read_output_activity[_in]`, `last_output_at_ms[_in]`, `newest_output_at_ms` |
| `clientGeneration` field | `crates/pty-core/src/registry/metadata.rs` — `SessionMetadata::client_generation` |
| Output debounce and exit fold | `crates/pty/src/daemon/lifecycle.rs` — `stamp_output_activity`, `persist_output_activity`, `save_exit_metadata` |
| Client facts and bumps | `crates/pty/src/daemon/clients.rs` — `ClientFacts`, `note_client_change`, `write_client_generation` |
| Session cleanup | `crates/pty-core/src/registry/cleanup.rs`, `crates/pty-core/src/registry/evidence.rs`, `crates/pty-lifecycle/src/gc.rs` |

## Traceability

| Requirement | Evidence |
| --- | --- |
| `PTY.REGW-R01` | `crates/pty-conformance/tests/output_activity.rs::output_leaves_the_record_alone_rust`, `a_recorded_exit_is_not_rewritten_during_shutdown` |
| `PTY.REGW-R02` | `output_activity.rs`: `the_stamp_appears_after_output_and_reads_as_now`, `a_later_burst_moves_the_stamp_forward`, `a_busy_session_writes_the_stamp_about_once_a_second`, `a_child_that_prints_and_exits_at_once_keeps_its_stamp` |
| `PTY.REGW-R03` | `crates/pty-core/src/registry/activity.rs` unit tests; `crates/pty-client/tests/observation_roots.rs::output_activity_reads_only_the_selected_roots_generation` |
| `PTY.REGW-R04` | `output_activity.rs::the_exit_record_takes_over_from_the_sidecar`; cleanup by construction (module map) |
| `PTY.REGW-R05` | `crates/pty-conformance/tests/client_generation.rs::attach_resize_and_detach_each_bump_the_generation_rust` |
| `PTY.REGW-R06` | `client_generation.rs`: `peek_and_stats_leave_the_record_alone`, `a_resize_to_the_same_size_leaves_the_record_alone` |
