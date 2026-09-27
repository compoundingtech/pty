# 0014 — Registry reads run on the caller's thread

**Status:** accepted

The requirements this serves are
[docs/vrs/03-registry-reads/requirements.md](../vrs/03-registry-reads/requirements.md)
(`PTY.REG-R01` to `PTY.REG-R08`); the mechanism is in
[its spec](../vrs/03-registry-reads/spec.md).

**Node behavior.** `probeSocketsWithinBudget` (`src/sessions.ts:2274-2300`
at f005432) starts one `net.createConnection` per socket on the event loop and
races them against one budget timer. Every probe is a non-blocking connect
multiplexed on one thread. `queryStats` (`src/client.ts`) reads one session
per call, and a caller that wants many issues them concurrently on the same
loop.

**Rust behavior.** `probe_sockets_within_budget` and `query_stats_batch_in`
put non-blocking AF_UNIX connects for every socket into one poll(2) set on the
calling thread, under one deadline. They spawn no threads, and nothing they
open outlives the call. This is the shape Node gets from its event loop.

**Why.** The probe used to spawn one detached thread per socket for a
blocking connect, then stop waiting at the budget. That bounded the caller's
wait but not the threads: a connect to a full accept queue blocks on Linux
until the daemon accepts, so a wedged daemon kept its thread forever.
Embedders then did the same for STATUS, with one thread per running session.

fractal, a viewer that links pty-core, lists the registry every 2 s and reads
STATUS for every running session. On a 32-core glibc host, glibc allows up to
256 malloc arenas, and each new thread may get one. Freed arena memory stays
resident. An idle fractal grew from 66 MiB to 858 MiB in 20 minutes, while a
control run with `MALLOC_ARENA_MAX=2` stayed flat at 56 MiB. The root-cause
analysis is in schickling/dotfiles,
`context/agent-ecosystem/03-coding-agents/20-fractal/.experiments/2026-09-25-resource-load-rca.md`
(finding R1).

**Options compared.** A bakeoff harness replicated fractal's poll against the
live dev3 registry: about 71 running sessions, a 2 s cadence, and a probe plus
a STATUS read for every session per poll. It also ran two fault cases: 4
daemons that accept and never answer, and one daemon with a full accept queue.

| Option | RSS at end | glibc arenas | Gather p50 / p95 | 4 silent daemons | Full accept queue |
| --- | --- | --- | --- | --- | --- |
| Thread per session | 142 MiB, growing | 69 | 310 / 1006 ms | – | – |
| Sequential, blocking | 22 MiB | 71 | 259 / 508 ms | 8.2 s | hangs |
| Pool of 4 workers | 23 MiB | 84 | 419 / 1226 ms | 2.0 s | hangs |
| **One poll(2) set, caller's thread** | **8.8 MiB, flat** | **1** | **77 / 172 ms** | **2.0 s** | **2.0 s** |

- Thread per session is the defect itself.
- Sequential pays each silent daemon's timeout in turn, and a blocking connect
  to a full accept queue never returns.
- A pool bounds the threads but not the wait: a blocking connect still pins a
  worker on a full queue, and latency is worse than sequential at this size.
- The poll set is the only option that bounds threads, memory and wall time
  together, and it is also the fastest.

The replicated listing matched `list_sessions_in` in 30 of 30 polls on the
live registry. A fractal release build on this design, which also caps its
own arenas with `mallopt(M_ARENA_MAX, 2)` as a backstop, ran beside the
installed build on the same roots. It held one 64 MiB-aligned anonymous
mapping for two minutes, while the installed build went from 47 to 76 (RCA,
Amendment 2).

**What is not done.** No `mallopt(M_ARENA_MAX)` in pty-core. Capping arenas
is the embedder's choice for its whole process; a library that stops creating
threads removes the cause without deciding that for it.

**Client effect.**

- `list_sessions*` and `socket_reachable` return the same answers as before.
  A Linux listener with a full accept queue is still absent from the probe
  map, which the listing reads as unreachable.
- One edge case changes: with a zero budget, a socket whose first connect
  completes at once now appears in the probe map. Before, the budget expired
  before any thread reported, so the map was empty.
- `query_stats_batch_in(root, names, deadline)` is a public pty-core API.
  Each result equals `query_stats_in_with_timeout` for that session, except
  that a Linux full accept queue is a `StatsTimeout` at the deadline, not an
  unbounded wait in connect.

**Test.** `crates/pty-client/tests/observation_roots.rs`:
`socket_probe_matches_a_blocking_connect` (probe answers equal a blocking
connect, a full queue is absent on Linux and `false` on macOS, and the
listing classifies each) and `batch_stats_bound_every_session_by_one_deadline`
(an answering, a silent, a full-queue and a missing session in one batch, all
bounded by one 500 ms deadline). No gated `_node` / `_rust` pair: the CLI
output of `pty list` and `pty stats` does not change. The memory figures are
measured, not tested.

**Migration / negotiation.** None. An embedder that fans out one thread per
session for STATUS can call `query_stats_batch_in` instead.
