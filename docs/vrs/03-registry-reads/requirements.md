# Registry reads — Requirements

## Context

This node defines how pty-core reads the session registry and the session daemons behind it for a caller in the same process. It covers bounded socket transport diagnostics inside a strict listing (`list_sessions_in`, `list_sessions_with`, `socket_reachable`) and STATUS reads, single (`query_stats_in_with_timeout`) and batched (`query_stats_batch_in`). A running classification requires a live daemon identity with matching recorded boot identity and start token on Linux and macOS, plus thread-group leadership on Linux. Missing boot identity or start token is Unknown, never a legacy Match, and grants no signal authority; socket reachability is not liveness authority. Teardown signals only pinned exact owners through the existing `Exact` abstraction (Linux pidfd / macOS audit token), without a raw-PID or process-group fallback. Classification is tracked in [docs/parity.md](../../parity.md) section 5.2. This node constrains how the reads are performed. The rationale is in [decision 0014](../../decisions/0014-registry-reads-run-on-the-callers-thread.md).

## Assumptions

- **PTY.REG-A01 Periodic embedders:** A viewer or supervisor links pty-core and, for the life of its process, lists the registry and reads STATUS for every running session on a fixed cadence of seconds.
- **PTY.REG-A02 Unreliable daemons:** Behind a socket file there may be no listener (stale file), a listener that never accepts (full accept queue), or a daemon that accepts and never answers.
- **PTY.REG-A03 Registry size:** A registry holds tens to a few hundred sessions.

## Constraints

- **PTY.REG-C01 Full accept queue:** On Linux a blocking `connect(2)` to an AF_UNIX listener with a full accept queue waits until the listener accepts, and a non-blocking one fails with `EAGAIN`. On macOS both fail with `ECONNREFUSED`.
- **PTY.REG-C02 No readiness for queue room:** Linux raises no poll(2) event when a full accept queue gains room.
- **PTY.REG-C03 Allocator arenas:** glibc gives threads their own malloc arenas, up to eight per core, and keeps freed arena memory resident. Thread churn in a long-lived process grows its resident memory.

## Acceptable Tradeoffs

- **PTY.REG-T01 Retry tick:** A socket whose accept queue is full is retried on a fixed tick until the deadline. Each retry costs one `socket(2)` and one `connect(2)`.
- **PTY.REG-T02 Batch bounds the connect:** A single STATUS read bounds only its read. A batch also bounds the connect, so on Linux a daemon with a full accept queue is a `StatsTimeout` in a batch and a wait in a single read.

## Requirements

### Must not grow with the caller's lifetime

- **PTY.REG-R01 No thread per session:** Listing, liveness probing, and batch STATUS reads run on the calling thread and spawn no threads.
- **PTY.REG-R02 Nothing outlives the call:** When a listing, a probe, or a batch returns, it holds no socket, thread, or pending connect for any session.
- **PTY.REG-R03 Flat embedder cost:** An embedder that lists the registry and batch-reads STATUS on a fixed cadence (`PTY.REG-A01`) keeps its thread count and resident memory flat over time. Neither grows with the number of polls.

### Must answer within its budget

- **PTY.REG-R04 Probe budget:** A listing probes every socket that needs a probe under one shared budget, 500 ms by default, and does not wait past it.
- **PTY.REG-R05 Unanswered is absent:** A socket that has neither connected nor failed when the budget ends is absent from the probe result and its transport is unconfirmed. Strict listings report incomplete transport diagnostics; neither a reachable nor an unanswered socket proves daemon liveness. A Linux listener with a full accept queue is such a socket.
- **PTY.REG-R06 One deadline per batch:** A batch STATUS read serves any number of sessions under one shared deadline. Silent daemons and full accept queues cost the deadline once, not once each, and cannot hold the caller past it. Neither can a daemon that sends packets without pause, and it cannot keep the other sessions from being answered.

### Must answer what a single read answers

- **PTY.REG-R07 Probe equivalence:** A socket that answers within the budget maps to what a blocking connect reports: success is reachable, any failure is unreachable.
- **PTY.REG-R08 Batch equivalence:** A batch returns one result per requested session, in request order. Each result equals what `query_stats_in_with_timeout` returns for that session: the same `StatsResult`, or the same `ClientError` variant and text. A session not finished at the deadline reports `StatsTimeout`.
