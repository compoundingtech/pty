// Per-worker isolation hard guard.
//
// The vitest process may itself be running INSIDE a pty session (this repo is
// often developed/tested from within a `pty` session), which means the ambient
// environment carries PTY_ROOT / PTY_SESSION / PTY_SESSION_DIR. Tests isolate by
// passing a per-test PTY_SESSION_DIR to each spawned `pty`, but getSessionDir()
// prefers PTY_ROOT over PTY_SESSION_DIR — so an ambient PTY_ROOT (inherited via
// `{ ...process.env }` in the test helpers) silently WINS and every spawned
// `pty` reads the developer's REAL live session dir instead of the tmpdir. That
// turns the suite non-deterministic and can even mutate live sessions.
//
// This file is registered as a vitest `setupFiles` entry, so it runs once in
// every worker BEFORE any test module executes. Scrubbing here guarantees that
// neither the worker nor any child it spawns (Session.spawn, raw spawn/spawnSync
// in the test helpers) inherits the ambient pty context. Tests that WANT a
// specific root still pass it explicitly per-child via `opts.env` /
// `PTY_SESSION_DIR`, which is unaffected — we only clear the process-level
// ambient values.
delete process.env.PTY_ROOT;
delete process.env.PTY_SESSION;
delete process.env.PTY_SESSION_DIR;
// Also scrub the exit-reap config knob: the ambient network may set
// PTY_REAP_ON_EXIT, and lifecycle tests must exercise a deterministic default
// (unset → the shipped `reap` default). Tests that want the other mode pass
// PTY_REAP_ON_EXIT explicitly per-child.
delete process.env.PTY_REAP_ON_EXIT;
