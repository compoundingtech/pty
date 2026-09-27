# Plan: bring the Rust `pty` to full parity with the Node `pty`, in one go

## Context

The Node `pty` (`@compoundingtech/pty` 0.12.0) hosts every agent terminal in
the network. A Rust port exists (`compoundingtech/pty-rust`, `origin/main` at
`e4d6cda`). A measured experiment on 2026-08-28 showed one session costs
6.1 MB RSS on Rust against 158.6 MB on Node. The owner wants off Node. The map at
`docs/parity.md` (branch `parity-map`, head `4b6e533`) lists every surface a
drop-in replacement must carry and records the decisions taken on 2026-08-29.
This plan closes the whole gap in one build, then cuts the fleet over.

**Goal.** A Rust `pty` binary that any consumer in the network (`st2`,
`pty-relay`, `deskset`, `ding`, `smalltalk`, `evals`, `fabric`) uses without a
change, on a `$PTY_ROOT` it can share with Node daemons during migration, plus
a Rust testkit, a TypeScript testing package, a Rust TUI library with the
session manager, and an embedding API.

**Done condition** (checkable, all five):
1. `PTY_TEST_BIN=<node pty> cargo test -p pty-conformance` is green (the suite
   is faithful to Node).
2. `PTY_TEST_BIN=<rust pty> cargo test -p pty-conformance` is green; every
   decision-gated test cites an accepted record in `docs/decisions/`.
3. The mixed-fleet rig is green in both directions (Node client ↔ Rust daemon).
4. Every consumer row in `docs/parity.md` §2 has a named verification run that
   went green (WP-CUT lists them).
5. `docs/parity.md` has no `missing`/`partial` rows except dropped/deferred;
   `st2`'s flake points at pty-rust and the first host runs on the Rust `pty`.

Decisions already taken (do not reopen): base `origin/main`; drop-in target;
mixed fleet wanted; dropped: `gc` respawn/flapping/abandoned, `pty test`, the
second test binary, `remote-serve --socket`, legacy positional display name,
the Rust geometry-neutral `ATTACH` flag, `<name>.screen`; deferred and
documented: `recover`, `evidence`; version `0.13.x-rust+<short-sha>`; TUI on
`ratatui` + `crossterm`, all 28 widgets; TS package in pty-rust with the Rust
binary as engine; the lead agent drives with subagents in worktrees.

## Shape decisions (settled)

- **Six crates, one workspace** (`Cargo.toml` at the root; `[workspace.package]
  version = "0.13.0-rust"`, edition 2024, rust-version 1.88):
  - `crates/pty-core` — framing, registry, locks, events, metadata, names,
    tags, keys/paste/duration/input/queries/ptyfile, client ops (attach loop,
    peek, send, status, `SessionConnection`, reconnect, remote dial). No
    libghostty, no zig. This is what `deskset`'s `pty-wire` + `pty-cli` become.
  - `crates/pty-terminal` — the libghostty actor: owns the `!Send` `Terminal`,
    typed snapshots (cells, wrapped flags, cursor, modes, kitty stack,
    scrollback), VT/plain serialization, query answers, terminal events. The
    `TerminalHandle` (attach/spawn) for embedding was built here and has
    since moved to `crates/pty`, so this crate owns no process.
  - `crates/pty-testkit` — `Session` (spawn + daemon-backed), `Screenshot`, waits.
  - `crates/pty-tui` — ratatui-based library: pty pane, theme tokens, focus
    stack, fuzzy, line edit, app runner, and the 28 widgets.
  - `crates/pty-conformance` — the black-box suite that runs against any `pty`
    binary (`PTY_TEST_BIN`), plus the mixed-fleet rig.
  - `crates/pty` — the `pty` binary: CLI (one module per command), daemon
    (`__daemon`), `remote-serve`, the interactive session manager.
- **CLI parsing is hand-rolled**, one module per command, a shared `Argv`
  cursor. Node's grammar is irregular (`--root` anywhere, flags-then-ref
  loops, `--with-delay` position rule, `--flag=value` only in `gc`, silently
  ignored tokens in `list`) and the pinned error texts name exact tokens.
  Mirroring `cli.ts` loop for loop reproduces them; clap would fight it. Help
  text and completion scripts are vendored verbatim from Node at `500eab2`.
- **Daemon keeps the single actor thread.** `vt_write` is synchronous, so a
  SCREEN cut always sees every received byte; the 80 ms settle is an
  actor-owned deadline serviced with `recv_timeout`. No `Arc<Mutex<Terminal>>`.
- **Locks follow Node's file protocol exactly** (`O_CREAT|O_EXCL`, holder pid,
  one stale steal, order events → creation), because Rust and Node writers
  share a root during migration.
- **Query answers are fixed in Rust, not recorded**: DA1 `?62;22c`, DA2
  `>0;382;0c`, XTVERSION `pty(0.8)`, OSC 10/11/4 with Node's constants, via
  libghostty callbacks and default colors. A decision record is only for a
  difference the CLI cannot hide.
- **Conformance first.** The harness and the first CLI files run against the
  Node `pty` (`0.12.0+500eab2`) from day one; the Rust
  build drives them from red to green.
- **Integration branch** `parity` off `parity-map`; each work package is a
  branch off `parity`, rebased and merged by the lead after
  `cargo test --workspace` and the package's done condition. Push branches
  and report SHAs; no PR on `compoundingtech/pty-rust` without say-so.

## Work packages

Each package: goal, files, reference, done condition. Node references are
`pty/src/<file>:<lines>` in `~/src/github.com/compoundingtech/pty`; the
inventories in the working notes (not in this repository) cite them per behavior
(`node-cli-surface.md`, `node-daemon-protocol-disk.md`, `node-testing-tui.md`,
`rust-port-and-st2.md`).

### WP1 — Workspace restructure (alone, first)
Move today's `src/*` into the six crates with no behavior change. `src/bin/pty.rs`
+ `src/daemon.rs` → `crates/pty/src/{main,daemon/mod,cli/mod}.rs`;
`protocol/registry/keys/paste/duration/input/queries/ptyfile/client/stats` →
`crates/pty-core/src/`; `screenshot.rs` + terminal parts → `crates/pty-terminal`;
`session.rs` → `crates/pty-testkit`. Tests move with their owners; the
`tests/fixtures/parity` corpus stays at the root. `crates/pty/build.rs` stamps
`0.13.0-rust+<short-sha>` (`git rev-parse --short HEAD`, overridable by
`PTY_BUILD_SHA` for nix). Done: `cargo test --workspace` = 173 green;
`cargo build -p pty-core` needs no zig.

### WP2 — Registry and metadata (`pty-core::registry`)
`metadata.rs`: every Node field (`generation`, `daemonPid`, `recovery` as
opaque `Value`, `rows`, `cols`, `ephemeral`, `isolateEnv`, `extraEnv`,
`unsetEnv`, `env`, ms-precision `createdAt`, …) plus `#[serde(flatten)]
extra` so unknown fields round-trip; publication writes use Node's key order
(`node-daemon-protocol-disk.md` §2.4). `atomic.rs`: `<path>.tmp.<pid>.<16hex>`
+ rename; readers skip `.tmp.`. `lock.rs`: `acquire_file_lock` per
`sessions.ts:2293-2336`, event lock with 5 s wait and the `event log is busy`
text, `with_both_locks`. `mutate.rs`: `mutate_metadata_under_lock` →
`Busy|Missing|GenerationMismatch|Stale|Unchanged|Changed` (`sessions.ts:347-398`).
`list.rs`: `list_sessions` per `sessions.ts:895-1013` (`.sock` scan, orphan
`.json`, `daemonPid` only with a matching start token, 500 ms probe budget,
never mutates), `get_session` with the ambiguity text. `names.rs`:
`validate_name`, `validate_display_name`, random id alphabet
`23456789abcdefghjkmnpqrstuvwxyz`, `auto_display_name` (`cli.ts:651-668`).
`tags.rs`: reserved keys, `matches_all_tags`, `is_keep_requested`,
`should_reap_at_exit`, gc bookkeeping key list. `cleanup.rs`:
`cleanup_owned_socket/all` with generation CAS. Remove `<name>.screen`.
Done: a Node-written `<name>.json` rewritten by Rust `tag` differs only by
the tag and the event; ported `security-fixes` lock tests and
`atomic-writes` concurrency tests green; `list --json` on a mixed root
matches Node field for field.

### WP3 — Events log (`pty-core::events`)
Envelope `{session, type, ts, …}`, all type constants and payloads
(`events.ts:10-22, 42-191`), `append_event` under the event lock, retention
1000 → 500 (daemon every 100 appends; one-shot writers at ≥ 40 000 bytes),
`clear_events` at daemon start, `read_recent_events(50)`,
`validate_user_event_type`, `format_event` (`events.ts:548-604`),
`EventFollower` on `notify` (existing files from EOF, new from 0, shrink →
restart, `--all` directory watch). Done: `events.test.ts` and
`events-emit.test.ts` literals green; Node `pty events -f` follows a Rust
daemon.

### WP4 — Terminal actor (`pty-terminal`)
`actor.rs`: `TerminalActor` with synchronous methods (`write`, `resize`,
`snapshot`, `serialize`, `plain(viewport|full)`, `modes`, `take_pty_replies`,
`take_events`). `queries.rs`: `on_device_attributes` (DA1 `?62;22c`, DA2
`>0;382;0c`), `on_xtversion` (`pty(0.8)`), default fg `c0c0c0` / bg `000000`
/ palette so OSC 10/11/4 answer with Node's constants; `on_bell`,
`on_title_changed`; OSC 9/99/777 tapped from the raw stream; cursor-visible
and focus-request from mode diffs around `vt_write`; kitty stack tracked by
scanning `CSI > n u` / `CSI < u` (libghostty exposes one value, Node tracks a
stack). `serialize.rs`: replay = `Format::Vt` + modes + cursor + kitty, with
the daemon prepending Node's mode prefix (`server.ts:1065-1082`);
`plain_viewport`/`plain_full` by row walk (`grid_ref`, `with_trim`) so
`--full` is honoured and trimming is controlled (`server.ts:1269-1293`).
`snapshot.rs`: `CellGrid { rows, wrapped, cursor, base_y, len }` from
`GridRef::cell()/style()` + `Row::is_wrapped()` — the `readCells` /
`readWrappedFlags` contract. `strip.rs`: Node's exact query-strip set.
`handle.rs` (since moved to `crates/pty/src/handle.rs`):
`TerminalHandle::attach(SessionRef, AttachOptions)` and
`TerminalHandle::spawn(cmd, args, SpawnOptions)` (issues #1, #3): one actor
thread, `AttemptId` per attach so late frames from an older attempt are
dropped, events `Dirty{rev}|Title|Bell|Exited|Geometry`, `snapshot(offset)`,
`write`, `resize`, `close`. Done: `terminal-queries` responses byte-equal;
`screens.json` fixtures pass with viewport semantics; a ratatui-compat-style
replay (alt screen + kitty stack + ECH/CUF backgrounds) restores the same
picture as Node; `pty-handle`/`pty-pane` cell tests ported and green.

### WP5 — Daemon (`crates/pty/src/daemon/*`)
`launch.rs`: `pty run` passes a JSON config on inherited fd 3 to
`<self> __daemon` (same shape as `PTY_SERVER_CONFIG`, `spawn.ts:169-184`),
`setsid`, stdio null, stderr captured for `Daemon process exited immediately
(code N).\n<stderr>`; readiness = socket → `daemonPid == child.pid` and a
`session_start` line with `ts >= createdAt` (`spawn.ts:225-236`), 30 s;
process titles `pty` / `pty-daemon` via `prctl` on Linux. `env.rs`:
`build_child_env` per `server.ts:131-209`; `describe_invalid_cwd`; child via
`/bin/sh -c 'exec "$@"' sh <cmd> <args>` with `command` resolved absolute
(`spawn.ts:372-393`). `clients.rs`: `Client { role: Command|Writable|Readonly,
rows, cols, attach_seq, phase: Live|Settling{deadline, generation}, queued }`;
ATTACH/PEEK/DATA/RESIZE/STATUS/DETACH per `server.ts:931-1063`; cut = when
the deadline fires (or immediately) serialize from the live terminal, send
SCREEN, go `Live`, then EXIT if exited; supersession by generation;
`nudge_redraw` when the attach size differed. `geometry.rs`: per-axis min
over writable-attached clients, GEOMETRY broadcast before the PTY resize
(`server.ts:1158-1190`). `status.rs`: `StatsResult` with
`clients.connections[]`, command sockets excluded; `geometryNeutral` removed.
`lifecycle.rs`: generation `randomBytes(16).hex`; publication order dir →
clear events → unlink stale sock → bind (umask 077, chmod 600) → pid →
metadata → `session_start`; exit code `128 + signal` from raw `waitpid`;
`save_exit_metadata` under lock with retries, `lastLines` 200;
`session_exit`; shutdown 500 ms later; external kill: descendants snapshot
with start tokens (`process-tree.ts`), SIGHUP child, TERM ≤ 1.5 s, KILL
≤ 0.5 s; `PTY_SHUTDOWN_DEADLINE_MS` backstop text; reap re-reads on-disk
tags and refuses on a generation change; `PTY_SPAWNER_PID` poll 5 s;
`signal-hook` → channel message. `events.rs`: actor events → `EventWriter`
thread. Done: `integration.test.ts` order cases (`[GEOMETRY, SCREEN]`,
post-cut DATA before EXIT, exit during sync, supersession, PEEK cancels
ATTACH), `effective-geometry`, `exit-signal` (137 + signal 9),
`shutdown-backstop`, `spawner-pid-watchdog`, the 3-deep-tree kill case, all
green through the conformance socket client; Node
`pty attach --attach-stream-fd-v1` against a Rust daemon yields
`[GEOMETRY, SCREEN, …, EXIT]`.

### WP6 — Client operations (`pty-core::client`)
`attach.rs` (raw mode only on a tty, ATTACH with stdout size or 24×80,
SIGWINCH → RESIZE, `\x1b[2J\x1b[H` before SCREEN, exact exit/detach lines,
`TERMINAL_SANITIZE`, error mapping, `--attach-stream-fd-v1` re-framing with
the GEOMETRY-before/SCREEN-before checks, DETACH on local detach, truncation
and EPIPE texts, backpressure; reconnect backoff table and
`PTY_RECONNECT_MAX_ATTEMPTS`; `client.ts:415-442, 467-523, 596-641, 706-749`),
`peek.rs` (one-shot plain/ANSI + sanitize + `\n`; follow with `stripAnsi`;
`peek_wait` 200 ms with `lastLines` fallback and exact diagnostics),
`send.rs`, `connection.rs` (`SessionConnection` for deskset/testkit; a
`tokio` feature with `AsyncConnection` for deskset), `stats.rs`,
`remote.rs` (fabric dial, route line, `RouteRefusedError`). Done:
`attach-stream` and `sanitize` literals green against the Rust daemon;
deskset's `pty-wire` tests pass on a deskset branch that re-exports
`pty-core` (its owner merges it, not us).

### WP7 — CLI (`crates/pty/src/cli/*`, one file per command)
`main.rs`: `--root` scan, root-length backstop (three lines), subcommand
detection skipping `--filter-tag` values, interactive dispatch, per-command
`-h/--help` only as `args[1]`, git-style forwarding via `which pty-<cmd>`,
`Unknown command:` + usage on stdout, every thrown error → stderr + exit 1,
usage errors exit 1 (`completions` keeps 2). `help.rs`: `usage()` and
`COMMAND_HELP` vendored verbatim (test against `tests/fixtures/help/*.txt`
extracted from Node at `500eab2`). `completions.rs`: the three vendored
scripts byte for byte. Commands: `run` (all flags incl. `--env`,
`--unset-env`, `-a`, `--isolate-env`, `--no-display-name`, `--force`,
creation/event locks, `PTY_CREATION_LOCK_OWNER_PID`, `Session "<id>"
created.`, foreground attach; legacy positional → usage error), `attach`
(restart policies, dead-session prompt incl. the `Command was:` quirk),
`exec`, `peek`, `send` (Node's `--paste` modifier and position rules, typo
flags), `events`, `list` (§2.7 text layout with SGR, JSON key order,
`--summary`, filters, sort), `stats` (`printStats` block, gone shapes),
`restart` (guard regexes, prompt, tag strip, `ST_AGENT`/`ST_ROOT` scrub),
`kill`, `rm`, `gc` (debris, orphan kill, sweep, keep, prune, dry-run, footer,
`--print-launchd-plist`), `tag`, `tag-multi` (own help), `emit`, `rename`,
`metadata patch`, `up`/`down` (bind by the `(ptyfile, ptyfile.session)` tag
pair, tag sync lines), `version`; `recover`/`evidence`/`test` print
`pty <cmd>: not available in this build. See docs/parity.md.` exit 1.
`ask.rs`: `[Y/n]` prompt. Reference: `node-cli-surface.md` §1–3, §5, §6.
Done: the conformance CLI files (WP-CONF) for every verb green against Rust;
`st2 up` with `ST2_PTY_BIN` runs one agent through spawn → list →
metadata patch → send → peek → kill → rm.

### WP8 — Remote (`crates/pty/src/remote.rs`)
`remote-serve --stdio`: one JSON line, `list` / `route`, `{"ok":true}` +
bidirectional splice, residual bytes forwarded, exact error shapes
(`remote.ts:81-165`), exit 0 at the end. `--remote <peer>` on `list`
(host groups, `{local, remote}` JSON, bare form via `pty-relay ls --json`),
`peek` (no `--wait`), `send`, `attach` (reconnect loop and status lines;
`cli.ts:2019-2102, 2223-2247`). Done: `remote-fabric` and
`remote-exec-bridge` cases green with a stub `fabric` on PATH; the evals cell
`pty-attach-machine-stream` passes with the Rust binary.

### WP-CONF — Conformance suite (`crates/pty-conformance`)
`src/harness.rs`: `Rig` (temp `PTY_ROOT` under `/tmp/pc-XXXX`,
`PTY_ROOT_LEGACY_SILENT=1`, scrub `PTY_SESSION*`, `PTY_SESSION_DIR`,
`PTY_REAP_ON_EXIT`, `ST_AGENT`, `ST_ROOT`, `NO_COLOR`; `TERM`; `HOME`
sandboxed), `rig.pty(args)`, `pty_env`, `pty_stdin`, `pty_tty` (runs the CLI
inside a real PTY via `pty-testkit`), `rig.daemon(id, cmd, opts)`,
`rig.connect(id)` (`pty-core` socket client with `sequence()`), deadline
polls (10 s, `PC_SLOW=1` doubles), teardown kills every `<root>/*.pid`.
`PTY_TEST_BIN` selects the binary (default: the workspace's own build).
One test file per Node test file, each `#[test]` annotated
`/// node: tests/<file>.test.ts:<line>`; `scripts/conformance-map` emits
`docs/conformance.md` (file → kind → reason). Kinds: **cli** (48 Node suites,
black-box), **protocol** (12: integration, effective-geometry, connection,
attach-stream frames, screen-replay-altscreen, scrollback-fidelity,
terminal-queries responses, sanitize, exit-event-race, atomic-writes,
security-fixes, ratatui-compat re-expressed through the socket), **unit**
(9, already ported), **not portable** (TUI widget/framework files, in-process
xterm/PtyServer tests, recovery, disk-layout-docs — listed with reasons).
Decision-gated tests come in pairs (`_node` / `_rust`, gated on
`pty --version`), each citing `docs/decisions/NNNN-<slug>.md` (template:
status, Node behavior, Rust behavior, why, client effect, test, migration).
Expected records: raw DATA bytes (issue #1's first), ANSI serialization
byte-differs-but-equivalent (proven by re-parsing both with libghostty),
possibly plain trimming, emoji width, reflow, `scrollbackUsed` counting.
New fixtures (`conformance/fixtures/`, issue #4): UTF-8 and escape sequences
split at every byte boundary, raw 0x80–0xff, attach identity with a
replacement under the same id, late-event rejection, frame limits, slow
reader. `tests/mixed.rs` (`PTY_TEST_BIN` + `PTY_TEST_BIN_PEER`): peer
`run -d`, ours `list/peek/send/stats/attach/metadata patch/kill/rm/gc`, lock
contention, unknown-field preservation; both orientations. Done: green on
Node from the first files; green on Rust at the end; `docs/conformance.md`
covers all 120 Node suites.

### WP-KIT — Rust testkit server mode (`pty-testkit`)
`Session::server(cmd, args, ServerOptions)` spawns the daemon through
`pty-core`'s spawn path, connects, renders SCREEN/DATA into its own actor so
`screenshot()` is identical in both modes; `attach()` resolves on the first
SCREEN; `reconnect()`, `connect_to_existing`, `resize` with effective
`rows()/cols()` from GEOMETRY, `has_exited`, `exit_code`, `name`, `close`
(kills only if owned); `wait_for_*_default` (10 s, 50 ms); `resolve_key`
accepts `+`, `-`, `_`, `C-`. Done: server-mode cases of `screenshot.test.ts`
ported and green; crate README updated.

### WP-TS — TypeScript testing package (`packages/pty-testing`)
Root `package.json` with `workspaces: ["packages/*"]`; npm name
`@compoundingtech/pty-testing`; ESM, no runtime deps, vitest as a peer.
Engine: `PTY_BIN` or `pty` on PATH; refuses a non-`-rust` binary unless
`PTY_TESTING_ALLOW_NODE=1`. API: `Session.spawn/connect/connectToExisting`,
`name/root/rows/cols/hasExited/exitCode`, `sendKeys/type/press` (key table
ported from `keys.ts`), `screenshot()` (PEEK plain → lines/text, PEEK ANSI →
ansi), `waitForText/waitForAbsent/waitFor` (50 ms, 10 s, Node's error
texts), `resize`, `reconnect`, `attach`, `close`. `spawn` = temp `PTY_ROOT`,
`pty run -d -e --no-display-name --id <8> --cwd --env --rows --cols -- cmd`,
one attached socket + short-lived PEEK sockets; `src/protocol.ts` reimplements
the 5-byte framing (~120 lines). `docs/testing.md` with executable fences
(`scripts/verify-docs.ts`); vitest setup scrubs `PTY_*` and kills leaked
daemons under `/tmp/pt-*`. Done: `npm test` green on the Rust binary;
Node's `screenshot.test.ts` cases ported; `verify-docs` green; one smoke
test in `evals` or `st2`.

### WP-TUI — `pty-tui` and the session manager
Deps `ratatui`, `crossterm 0.29` (kitty flags and SGR mouse confirmed),
`pty-terminal`, `pty-core`. Add: `PtyPane` (renders a `CellGrid` with
palette indices preserved, border/title/focus color, selection with scroll
translation, cursor report), `Theme` (13 slots) + 9 semantic tokens →
`ratatui::Color`, built-in themes incl. `terminal`, `theme_to_palette` for
embedded terminals, `FocusStack`, `fuzzy`, `LineEdit`, `App` runner
(enter/leave, `pause/resume` handing the tty to an in-process attach with a
forced redraw, tick, resize, ctrl+c → 130, overlay via `Clear`),
`ScrollRegion` + grouped selectable list. **All 28 Node widgets**, in three
groups: thin wrappers over ratatui built-ins (table, tabs, sparkline,
bar-chart, progress-bars, code-block, message, badge, breadcrumbs, toast,
help-overlay); ports with our state model (virtual-list, form/LineEdit,
select, confirm, command-palette, command-registry, tree, text-area,
stream-view, prompt-bar, toolbar, accordion, action-list-item, date-picker,
markdown); and the pty pane. Each widget gets a test ported from its Node
`widgets-*.test.ts`. Session manager in `crates/pty/src/interactive/`:
everything in `docs/parity.md` §9 (nesting guard text, list rendering,
fuzzy filter with `host/session`, relay hosts via `pty-relay ls --json`,
keys, attach-and-return, one-key create, restart of exited/vanished,
`--preselect-new`, `--filter-tag`, theme file, 1 s refresh paused during
attach, remote attach/spawn by shelling to `pty-relay`). Done: a port of
`tests/tui.test.ts` driven through `pty-testkit` (60/80/120/200 cols, filter,
kitty CSI-u escape, attach/detach cycles without doubled keys, external
refresh, `--preselect-new`, `--filter-tag`) green; widget tests green.

### WP-PKG — Packaging
`flake.nix`: `rustPlatform.buildRustPackage` (st2's pattern, `cargoLock`),
`PTY_BUILD_SHA` from `self.rev`, `installShellCompletion` from the vendored
files, `mainProgram = "pty"`. libghostty-vt-sys under nix: its `build.rs`
honours `GHOSTTY_SOURCE_DIR` and `GHOSTTY_ZIG_SYSTEM_DIR`; add a fixed-output
`fetchgit` of `ghostty-org/ghostty` at commit `a887df42c56f6de86c0fe6da9c4eeca37931e083`
(the sys crate's pin) and a fixed-output derivation that runs `zig build
--fetch`; `nativeBuildInputs = [ zig_0_15 ]`. `checks.conformance` runs the
suite on the built binary; `checks.completions` compares vendored files.
README rewritten for the workspace, stating the deferred commands. Done:
`nix build` on the first host yields a binary printing `0.13.0-rust+<sha>`.

### WP-CUT — Cutover

**Correction, 2026-09-01: the `ST2_PTY_BIN` steps below cannot be run as
written.** That variable exists only on st2's unmerged `pty-rust` branch
(`b63abfb`); `origin/main` has none, so nothing running reads it and no agent
can be pointed at one build. Either ship that st2 branch first, or select the
binary the only way st2 supports — by what is on `PATH`. The rest of the
staging, the checks and the rollbacks stand. See docs/parity.md §12a.
Consumer runs (all green before step 3): `st2` `cargo test` with the Rust
`pty` on PATH plus `st2 eval` on `pty-send-peek`, `pty-attach-only`,
`pty-attach-machine-stream`; `pty-relay` `integration/*.test.ts`; `ding`
`tests/integration.rs` and `smalltalk`'s ding tests with `PTY_SESSION_DIR`;
`deskset` `cargo test -p pty-wire -p pty-cli` against the Rust binary;
`fabric expose pty-remote --exec -- pty remote-serve --stdio` on the first host with
`pty list --remote <peer>` from another host. `st2` flake: `pty.url` →
`github:compoundingtech/pty-rust/<sha>`; rewrite the `pty-fleet-contract`
check to run `pty list --json` on the built binary (one commit on an st2
branch, SHA reported). Staged rollout on the first host, each with a check and a
rollback (`ST2_PTY_BIN` unset / flake revert / `nix profile rollback`;
sessions keep running because the registry is shared): (1) `ST2_PTY_BIN` on
the lead agent only; (2) all agents on the first host with Node `pty` still on PATH —
one day mixed; (3) Rust `pty` on PATH, `ST2_PTY_BIN` removed; (4) the other hosts
after a week. Rollback exercised once at step 1. Node package stays
as the oracle; a note to the Node maintainers about fixture ownership (issue #4).

## Order and parallel lanes

```
WP1 restructure (alone, ~1 day)
 ├─ lane A: WP2 registry → WP3 events                      (pty-core/registry, events)
 ├─ lane B: WP4 terminal actor + handle                    (pty-terminal)
 ├─ lane C: WP6 client ops                                 (pty-core/client)
 ├─ lane D: help/completions fixtures + WP-PKG flake       (crates/pty/src/cli/{help,completions}.rs, flake.nix)
 └─ lane E: WP-CONF harness + first CLI files vs Node      (pty-conformance)
then
 ├─ WP5 daemon (needs A, B)            ─┐
 ├─ WP7 file-op verbs (needs A)         ├─ WP7 socket verbs (needs WP5, C) → WP8 remote
 ├─ WP-CONF protocol files (needs WP5) ─┘
 ├─ WP-KIT (needs C, WP5)
 ├─ WP-TS (needs WP7 run flags, WP5 GEOMETRY)
 └─ WP-TUI (needs B handle, C)  — all 28 widgets, then the manager
finally
 WP-CONF green on both binaries + mixed rig + decision records → WP-PKG done → WP-CUT
```
File ownership per lane is disjoint, so lanes run as subagents in worktrees
off `parity`. The lead merges in the order above.

## What the lead decides alone / what comes back to the owner

Alone: module layout inside a crate, test file organization, which crate
versions to pin, wording of decision records, the order inside a lane.
Back to the owner: any new decision record that changes a user-visible result
(a sentence each, batched); every push to a repository with outside readers
beyond the pty-rust branches named here; the st2 flake commit; each rollout
step on a machine; the note to the Node maintainers; anything that would grow the scope
(a Node PR merging — #168 — during the build).

## Risks

- libghostty silent on OSC 10/11/4 even with defaults set → answer in the
  strip layer, where the bytes are already intercepted. Checked in WP4's
  first hour.
- Plain trimming / cursor after reflow differ from xterm → the shared
  fixtures decide; a decision record with a fixture, not a blocker.
- Node PR #168 (activity timestamp) merges mid-build → add it as one more
  WP5 item; the map's §13 tracks it.
- macOS paths (`ps` start tokens, resources): keep both code paths; test the
  Linux one on the first host, the darwin one on the owner's machine before step 4.

## Status

Approved on 2026-08-29. This file is the record of the plan; `docs/parity.md`
is the map it closes.

**Every work package here is built except WP-CUT, as of 2026-09-02.** WP1,
lanes A to E, WP5, WP6, WP7a, WP7b, WP8, WP-CONF, WP-KIT, WP-TS, WP-TUI with
the session manager, and WP-PKG are all on `parity`.

What that means, measured rather than claimed:

- The whole workspace test suite passes: 1298 tests, none failing.
- The conformance map covers all 120 Node test files. Every command-line and
  protocol suite has Rust tests and none are left to do. Six gated pairs
  remain and each points at a decision record; those six are the parity debt.
- `nix flake check` passes end to end.
- The TypeScript testing package passes 17 of 17 driving the Rust binary.
- The help text is byte-identical to the Node tool for 24 of 25 verbs. The
  one difference is `--version`, by decision.
- A client of either implementation drives a session hosted by the other, in
  both directions, for list, peek, send, stats, kill and rm. That is what
  makes a staged rollout safe: sessions keep running while the binary on PATH
  changes.
- A real coding-agent harness runs in a session this daemon hosts. It renders,
  it takes keys, and a bracketed paste reaches its composer. Hosting the same
  harness, this daemon holds 6.7 MB against the Node daemon's 60.8 MB.

**WP-CUT is what is left, and most of it is not code.** The consumer runs
belong to the people who own those repositories, the flake change belongs to
`st2`, and the rollout is a decision rather than a task.

**One thing must be fixed before this binary reaches a machine that runs
evals, and it is not in this repository.** `st2`'s boot gate decides a task is
alive from a single reading of `pty list --json`. Both implementations leave a
session's record saying "running" for about five milliseconds after `pty run
-d` returns; the Node command line takes 33 milliseconds to start and so
always stepped over that window, while this one answers in 1.2 milliseconds
and lands inside it. The gate was protected by the slowness of the tool it was
asking. **Do not slow this tool down to restore that.**
