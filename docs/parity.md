# Parity map: the Rust `pty` against the Node `pty`

This document maps the distance between this Rust port and the Node `pty`
(`@compoundingtech/pty` 0.12.0). It lists every surface a drop-in replacement
must carry, what the Rust port has today, and what is missing. It is the input
to the plan that closes the gap. Section 12 records the decisions taken on it.

Compared versions, read on 2026-08-29:

- Node: `compoundingtech/pty` at `500eab2` (0.12.0, last change 2026-08-26).
- Rust: this repository at `e4d6cda` (crate `pty-testkit` 0.1.0, 173 tests).

## 1. Target and how to read this map

**Target: drop-in.** The Rust binary is named `pty`. It accepts the same
commands, flags, and environment variables. It prints the same text and the
same JSON shapes. It uses the same exit codes. It writes the same files under
`$PTY_ROOT`. It speaks the same socket protocol. A program that works with the
Node `pty` works with the Rust `pty` without a change.

**Wanted, not required: mixed fleet.** A Node client talks to a Rust daemon
and a Rust client talks to a Node daemon, on the same `$PTY_ROOT`. Section 11
lists what this costs beyond drop-in.

**Three surfaces.** The Node package has three parts. The parity rule differs
per part:

| Surface | Parity rule |
|---|---|
| CLI, daemon, protocol, registry, remote | Drop-in. Same behavior, same bytes on disk and on the wire. |
| Testing library (`pty/testing`) | Same capabilities. A Rust crate and a TypeScript package. The API does not need to match the Node API. |
| TUI framework (`pty/tui`) | Same capabilities. May be replaced by a new Rust TUI library. The interactive session manager is rebuilt on it. |

**Legend for the status column.**

- `have` — the Rust port has it and it matches.
- `partial` — the Rust port has it, but the behavior, text, or shape differs.
- `missing` — the Rust port does not have it.
- `dropped` / `deferred` — decided on 2026-08-29. Section 12 collects these.

Sizes are rough and relative: S = hours, M = a day or two, L = several days,
XL = a week or more. They cover code, tests, and docs.

## 2. Who depends on `pty`

These are the programs that call `pty` or read its files. A drop-in must keep
every surface in this table working.

| Consumer | CLI verbs used | Reads `$PTY_ROOT` directly | Speaks the socket | Env |
|---|---|---|---|---|
| `st2` (supervisor) | `run -d --force --id --name/--no-display-name --cwd --tag --unset-env --env`, `list --json`, `kill`, `rm`, `metadata patch --id`, `peek`, `peek --full --plain`, `send --seq/--with-delay`, `stats --json`, `--root`, `--help` | `<id>.pid` | no | sets `PTY_ROOT`; the binary comes from `PATH` and nothing else |
| `pty-relay` | `list`, `peek`, `send`, `tag --json`, `events --json` (over ssh), `run`, `attach` | `<name>.sock` path | yes (session bridge) | `PTY_SESSION_DIR` |
| `deskset` (`pty-wire`, `pty-cli` crates) | `run -d --id --tag`, `kill`, `rm`, `restart`, `rename`, `tag`, `list --json`, `stats --json`, `events --all --json` | `<name>.sock`, `<name>.json` | yes (attach, peek, status) | `PTY_ROOT` |
| `ding`, `smalltalk` | `peek --plain`, `send --seq/--paste/--with-delay`, `list` | `<session>.pid`; writes `<session>.ding-health` into the root | no | `PTY_SESSION_DIR` |
| `evals` cells | `run`, `send`, `peek`, `list`, `attach --no-restart`, `attach --attach-stream-fd-v1`, `remote-serve` | no | via the CLI | `PTY_ROOT` |
| `fabric` | `remote-serve --stdio` under the `pty-remote` ALPN | no | via `remote-serve` | — |
| `pty-layout` (stale, reference client) | library only: `createPty`, `attachPty`, `spawnDaemon`, `listSessions`, `updateTags`, `EventFollower` | via the library | yes | — |
| Fractal (external, Rust) | wants a Rust embedding API (issue #1, #3) | — | yes | — |

Two facts from this table shape the plan:

- `st2` passes `--env` on every spawn. The Rust `run` does not parse `--env`,
  so `origin/main` cannot host an `st2` task today. See section 3.
- `deskset` already carries a second Rust implementation of the protocol,
  registry, keys, and stats (`crates/pty-wire`, 2026-08-28). The plan should
  make one shared crate that `deskset` can use, and retire the copy.

## 3. CLI surface

### 3.1 Commands

| Command (Node) | Rust status | Gap |
|---|---|---|
| `pty` / `i` / `interactive` — session manager TUI | missing | Needs the new TUI library (section 9). `--preselect-new`, `--filter-tag`, `--force`, theme cycling, relay hosts, attach-and-return. |
| `run` | partial (M) | Missing `--env`, `--unset-env`, `-a` semantics, `--isolate-env` semantics, auto display name, display name validation, legacy positional name with hint, `Session "<id>" created.`, foreground attach, `Command not found`, `already in use` text, id alphabet, cwd validation text, creation lock, event lock, `PTY_CREATION_LOCK_OWNER_PID`. Rust extra: `--rows/--cols`. |
| `attach` / `a` | partial (L) | Missing `-r/--auto-restart`, `--no-restart`, `--force`, nesting guard, dead-session prompt with `lastLines`, `--remote`, `--attach-stream-fd-v1`, reconnect loop, exact `[detached]` / `[<name> exited with code N]` lines, `TERMINAL_SANITIZE` bytes, `\x1b[2J\x1b[H` before SCREEN. |
| `exec` | missing (S) | Rewrites command under lock with `expectedGeneration`, appends `session_exec`, runs the command. |
| `peek` | partial (M) | Missing multiple `--wait`, float `-t`, `--remote`, `lastLines` fallback, exit diagnostics, 200 ms poll, `--full` honoured by the daemon, viewport-only default, `TERMINAL_SANITIZE` after ANSI output. |
| `send` | partial (S) | `--paste` is a modifier in Node, a value flag in Rust. `--with-delay` position rule. Rejects extra positional args. Typo flags rejected. `--remote`. Key chord separators `-`, `_`, `C-`. |
| `events` | missing (M) | `--all`, `--recent`, `--json`, `--wait <type>`, `-t`, follow mode, text format `[HH:MM:SS] <session>: ...`. |
| `list` / `ls` | partial (M) | Missing `tags` in JSON, `--tags`, `--filter-tag`, `--status`, `--older-than`, `--newer-than`, `--summary`, `--remote [<peer>]`, sort by display name, the whole text layout with sections, colors, `[permanent]`/`[flapping]`, `~` paths, `timeAgo`. |
| `remote-serve` | missing (M) | `--stdio` control protocol (`{"op":"list"}`, `{"op":"route"}`). The `--socket <path>` form is dropped. |
| `stats` | partial (S) | Text output missing (Rust always prints JSON). JSON: add `clients.connections[]`, remove `geometryNeutral` or record it as a decision. Gone-session text. `--all` layout. |
| `restart` | partial (M) | Missing `-y`, `--force`, running prompt, stateful-agent guard, gc bookkeeping tag strip, persisted `rows/cols/env` reuse, `ST_AGENT`/`ST_ROOT` scrub, `Session "<n>" restarted.`, nested note, attach after restart. |
| `kill` | partial (S) | Exit 1 when not running (`not running` / `not found`), strip `strategy` on permanent, 7 s wait text, `Session "<n>" killed.`, ptyfile note. |
| `recover` | deferred | Full authenticated rebind of a live daemon. Documented as absent. Section 12. |
| `rm` / `remove` | partial (S) | Refuse when running, 7 s wait, generation check, `Session "<n>" removed.` / `not found`. |
| `gc` | parity | Debris/orphan/abandoned/permanent-respawn/flapping/sweep ordering, `keep`, layout-tag pruning, dry-run, lifecycle tuning flags, footer text, and `--print-launchd-plist` are implemented. Concurrent permanent reconciliation is serialized by the existing event/creation locks. |
| `tag` | missing (S) | Show, set, `--rm`, ordering rules, `tags_change` event, ptyfile warning. |
| `tag-multi` | missing (M) | Selectors `<ref>...`, `--filter-tag`, `--all --yes`; `--json`; own help. |
| `emit` | missing (S) | `user.*` validation, `--json`, `--text`, default ref from `PTY_SESSION`. |
| `rename` | partial (S) | Missing `--show`, `--clear`, inside-session single-arg form, validation, `display_name_change` event, lock, exact stdout text. |
| `metadata patch` | missing (S) | `--id`, JSON patch on stdin, `{changed, metadata}` reply, validation text, `metadata_change` event. `st2` calls this. |
| `evidence snapshot` / `remove` | parity | Tagged JSON results, strict reader, exact-generation remove, and replacement-safe cleanup are implemented. |
| `up` / `down` | partial (S) | Node binds by the `(ptyfile, ptyfile.session)` tag pair, syncs tags, prints `● <label> (started)` lines. Rust binds by name. |
| `test` | dropped | A vitest wrapper for the Node repository. |
| `completions <shell>` | missing (S) | fish, bash, zsh. Output must equal the checked-in files byte for byte. Exit 2 on a bad shell. |
| `version` / `--version` / `-v` / `-V` | partial (S) | Node prints `<semver>+<short-sha>` in a git checkout, `<semver>` otherwise. Rust prints `0.1.0`. Decided: `0.13.x-rust+<short-sha>`. |
| `help` / `--help` / `-h`, per-command `--help` | partial (S) | Top-level and per-command help text, verbatim. Rust has one fixed block. |
| `pty-<cmd>` forwarding | missing (S) | Unknown command → `which pty-<cmd>` → exec with inherited stdio; else `Unknown command: <cmd>` + usage, exit 1. |

### 3.2 Global behavior

| Behavior | Rust status | Note |
|---|---|---|
| `--root <path>` anywhere in argv | missing | First occurrence; sets `PTY_ROOT`. `st2` tests use it. |
| Root length check (`> 90` bytes) before dispatch | missing | Exact three-line message. |
| `PTY_SESSION_DIR` deprecated alias with one-time notice; `PTY_ROOT_LEGACY_SILENT` | partial | Rust reads the alias, prints no notice. `ding`, `smalltalk`, `pty-relay` set the alias. |
| Exit codes: 1 for every usage error; 2 only for `completions` | partial | Rust uses 2 for usage errors. |
| Ref resolution: exact id, then unique display name, else `Session reference "<ref>" is ambiguous.` with the id list | partial | Rust picks the first display-name match. |
| Session name rules and messages | partial | Node: `^[a-zA-Z0-9._-]+$`, 255 chars, not `.`/`..`, socket path ≤ 104 bytes. Rust: looser. |
| Display name rules | missing | Non-empty, trimmed, ≤ 160 scalars, no control chars. |
| Random id alphabet `23456789abcdefghjkmnpqrstuvwxyz`, 8 chars | partial | Rust uses base36 of a time hash. |
| Auto display name `<cwd-base>-<cmd-base>[-<arg-base>]` | missing | |
| Process title `pty` (CLI) and `pty-daemon` (daemon) | missing | Visible in `ps`; pinned by a test. |
| stdout/stderr split per command | partial | |
| Env vars: `PTY_ROOT`, `PTY_SESSION_DIR`, `PTY_ROOT_LEGACY_SILENT`, `PTY_SESSION`, `PTY_SESSION_GENERATION`, `PTY_REAP_ON_EXIT`, `PTY_SHUTDOWN_DEADLINE_MS`, `PTY_SPAWNER_PID`, `PTY_CREATION_LOCK_OWNER_PID`, `PTY_RECONNECT_MAX_ATTEMPTS`, `PTY_FABRIC_BIN`, `PTY_REMOTE_SERVE_DEBUG` | partial | Rust reads the first two, `PTY_SESSION`, `PTY_REAP_ON_EXIT`. `PTY_SERVER_CONFIG` is Node-internal and not needed. |
| Key chord grammar (`ctrl+u`, `ctrl-u`, `ctrl_u`, `C-u`, named keys, CSI-u, error texts) | partial | Rust splits on `+` only. Byte tables match. |
| Durations `30s 5m 2h 1d` and `formatDuration` | have | |
| Bracketed paste framing, 300 ms default gap, `round(sec*1000)` | have | |

## 4. Daemon and socket protocol

The wire format is shared by design. The framing already matches. Most of the
gap is in what the daemon does with the frames.

| Behavior (Node) | Rust status | Note |
|---|---|---|
| Frame `[type u8][len u32 BE][payload]`, 32 MiB cap, unknown types tolerated | have | |
| Message types 0–7 | have | |
| `GEOMETRY` (10) after every `ATTACH`/`PEEK` and on every effective resize | missing | Node clients wait for it in machine mode. `deskset` and the TS testing library read it. |
| Order per attach: `GEOMETRY → SCREEN → DATA* → EXIT?`; ordered cut with `terminal.write("", cb)`; settling/cutting/live phases; a newer `ATTACH`/`PEEK` supersedes a pending one | missing | Rust sends `SCREEN` at once. Node folds output that arrives during the cut into `SCREEN`. |
| 80 ms redraw settle after a resize before the cut; `nudgeRedraw` (`cols-1` then back) when the attach size differs | missing | |
| Client roles: command (no `ATTACH`), writable-attached, readonly (`PEEK`); roles replace each other on one socket | missing | Rust writes `DATA` and applies `RESIZE` from any socket. |
| Effective geometry = per-axis minimum over writable-attached clients; readonly never constrains; last geometry sticks with zero writers | missing | Rust: last non-neutral attach wins. Rust extra: `ATTACH` flag byte `0x01` geometry-neutral. Record as a decision or drop. |
| Mode prefix before `SCREEN`: `?1049h` (attach only), mouse `1000/1002/1003/1006`, `?25l`, kitty stack | partial | Rust replays modes and cursor through libghostty. Exact bytes to verify. |
| `PEEK` plain vs ANSI; `full` bit; viewport-only by default | partial | Rust ignores `full` and always includes scrollback. |
| Query stripping from `DATA`: OSC 10/11/4 `?`, DA1, DA2, DSR, XTVERSION | partial | Rust strips; the set to verify. |
| Query answers: DA1 `?62;22c`, DA2 `>0;382;0c`, DSR, OSC 10 `c0c0`, OSC 11 `0000`, OSC 4, XTVERSION `pty(0.8)` | partial | libghostty answers DA1, DSR, DA2 (`>1;0;0c`). It does not answer OSC 10/11 without default colors. Visible to fish, neovim, and others. |
| `EXIT` code = `128 + signal` on a signal death | unverified | |
| `STATUS` JSON with `clients.connections[]` | partial | See `stats` above. |
| Readonly clients never write; command sockets excluded from counts | missing | |
| `DATA` from the client written as UTF-8 text; Rust writes raw bytes | decision record | Issue #1 names this as the first decision record. The plan settles it with a byte round-trip fixture. |
| Machine stream `--attach-stream-fd-v1` on the client side | missing | Section 3. |
| Reconnect with backoff (`--remote`) | missing | |

### 4.1 Daemon lifecycle

| Behavior (Node) | Rust status | Note |
|---|---|---|
| Launch: one fork, `setsid`, stdio to null, wait for socket then for `daemonPid` + `session_start` | partial | Rust re-execs itself with `__daemon`. Fine. It must publish `daemonPid` and `session_start` so Node's `spawnDaemon` recognises it (mixed fleet). |
| Child spawn `/bin/sh -c 'exec "$@"' sh <cmd> <args>` with `command` resolved to an absolute path | partial | Rust spawns argv as typed. |
| Env policy: inherited → `unsetEnv` → `extraEnv` → force `PTY_SESSION` + `PTY_SESSION_GENERATION` → `TERM` default; `--isolate-env` allow-list | missing | Rust forces `TERM=xterm-256color` always and adds nothing else. |
| `generation` token per daemon incarnation | missing | Needed by `exec`, `evidence`, `rm`, `gc`, locks. |
| cwd validation messages | missing | |
| `SIGTERM`/`SIGINT` = external kill: preserve unless ephemeral; child gets `SIGHUP`; descendants `TERM` ≤ 1.5 s then `KILL` ≤ 0.5 s, deepest first, start-token checked | partial | Rust: `SIGHUP` then `SIGKILL` to the child after 500 ms, no descendants walk. `pty-kill-releases-socket-test` pins the tree case. |
| Shutdown backstop `PTY_SHUTDOWN_DEADLINE_MS` (5 s) | missing | |
| Spawner watchdog `PTY_SPAWNER_PID` | missing | Library-only; low priority. |
| 500 ms grace after exit before shutdown; exit metadata retried under lock | partial | |
| Reap vs preserve precedence (`keep` > `ephemeral` > `permanent` > `PTY_REAP_ON_EXIT`) | have | Rust accepts only `keep=true`; Node accepts any value except `false/0/no/off`. |
| On preserve: `exitCode`, `exitedAt`, `lastLines` (≤ 200) | partial | Rust keeps 50 lines and writes a Rust-only `<name>.screen` file. |
| Scrollback 10000, `scrollbackCapacity = rows + 10000` | have | |
| Terminal events to the event log: `bell`, `title_change`, `notification` (OSC 9/99/777), `focus_request`, `cursor_visible`, `session_start`, `session_exit` | missing | |
| Live recovery request handling (`.recovery/`) | deferred | |

## 5. Registry on disk (`$PTY_ROOT`)

| File | Rust status | Note |
|---|---|---|
| `<name>.json` metadata, pretty JSON, atomic tmp + rename | partial | Tmp name differs (`.json.tmp` vs `.tmp.<pid>.<rand>`); harmless if both sides ignore `.tmp`. |
| `<name>.sock`, `<name>.pid` | have | Node: socket `0600`, pid written after listen. |
| `<name>.events.jsonl` (1000 → 500 retention, truncated at daemon start) | missing | |
| `<name>.lock` creation/metadata lock; `<name>.events.lock`; lock order events → creation; stale-lock steal by pid | missing | Every mutation in Node takes these. A Rust writer without them can tear a Node writer's update. |
| `.recovery/` | deferred | |
| `.activity/<name>.json` output-activity sidecar | Rust-only | The running `lastOutputAtMs`, so output never rewrites `<name>.json`; folded into the exit record and removed. Decision 0015. |
| `theme`, `gc.log` | missing | Small. |
| `<name>.screen` | Rust-only | Remove once `lastLines` matches. |
| Root created `0700` on demand | have | |
| Foreign files in the root (`<session>.ding-health`) must be ignored | have | |

### 5.1 Metadata fields

| Field | Rust status |
|---|---|
| `command` (resolved path), `args`, `displayCommand`, `cwd`, `createdAt` (ms precision) | partial (`command` as typed; seconds precision) |
| `generation`, `daemonPid` | missing |
| `recovery{...}` | deferred (preserved on rewrite, never written) |
| `rows`, `cols`, `ephemeral`, `isolateEnv`, `extraEnv`, `unsetEnv`, `env` | missing |
| `tags`, `displayName`, `lastAttachAt` | have |
| `lastOutputAtMs` | have, placed differently: in `.activity/<name>.json` while running, in the record from exit on (decision 0015) |
| `clientGeneration` | Rust-only: bumped on writable attach, resize and leave (decision 0015) |
| `exitCode`, `exitedAt`, `lastLines` | partial (50 vs 200 lines) |
| Unknown fields survive a rewrite | missing — Rust drops them. This breaks a mixed registry. |

### 5.2 Liveness, reap, and gc

| Behavior | Rust status |
|---|---|
| `listSessions` never mutates; scans `.sock` then orphan `.json`; pid alive or socket reachable → running; probe budget 500 ms | partial |
| `daemonPid` accepted only with a matching start token | missing |
| Status `running` / `exited` / `vanished` | have |
| `gc` steps and messages | missing (see section 12) |
| Reserved tags hidden by default: `ptyfile*`, `strategy`, `:*` | missing |
| Behavioral tags: `strategy=permanent`, `keep`, `parent`, `role=agent`, `strategy.*` bookkeeping, `:l<pid>-*` | partial (`keep`, `permanent` only) |

## 6. Terminal emulation: libghostty in place of xterm

This is where a Rust port cannot be byte-identical by construction. Each row
is user-visible. Each needs a fixture that runs against both implementations,
and a recorded decision where the two differ.

| Observable | Note |
|---|---|
| ANSI screen serialization (`peek`, `SCREEN`) | Node uses `@xterm/addon-serialize`: per-cell SGR diffs, `ESC[nX`/`ESC[nC` for blank runs, `\r\n` between rows except wrapped rows, relative cursor moves, mode string, alt-screen block. libghostty `Format::Vt` produces a different but equivalent stream. Clients re-parse it, so equivalence is the bar, not identity. The `ratatui-compat` suite (kitty stack, ECH/CUF with backgrounds, alt-screen full redraw, resize timing) is the test. |
| Plain text (`peek --plain`, `lastLines`) | Shared fixtures pin exact strings (`READY> `, `LINE_A\nLINE_B\nDONE`). Trailing-space and trailing-row rules to verify against the fixtures. |
| Viewport vs scrollback | Node plain peek is viewport-only; `--full` adds scrollback. Rust always includes scrollback. |
| Wide characters, combining marks, box drawing, emoji width | Both handle CJK. Emoji width tables can differ. |
| Cursor position after resize and reattach | Node pins the child's post-`SIGWINCH` redraw position. |
| Alt-screen enter/leave and replay | Rust has tests. |
| Query answers | Section 4. |
| Bracketed paste markers absorbed, not shown | To verify. |
| Reflow on resize | xterm reflows; libghostty reflows. Rules differ in detail. |

## 7. Remote over fabric

| Behavior | Rust status |
|---|---|
| `remote-serve --stdio`: one JSON request line, `list` or `route`, `{"ok":true}` then splice | missing (M) |
| `remote-serve --socket <path>` listening form | dropped (the Node docs call it transitional) |
| `--remote <peer>` on `list`, `peek`, `send`, `attach`; `fabric dial <peer> pty-remote`; 10 s timeouts; `RouteRefusedError` | missing (M) |
| Bare `list --remote` via `pty-relay ls --json` | missing (S) |

## 8. Testing library

Node ships `Session.spawn` (direct child) and `Session.server` (daemon in the
test process), screenshots `{lines, text, ansi}`, `waitForText` /
`waitForAbsent` / `waitFor` (50 ms poll, 10 s default), `press`, `type`,
`resize`, `reconnect`, `connectToExisting`. Docs are executable
(`npm run verify-docs`).

| Deliverable | Rust status | Note |
|---|---|---|
| Rust crate: `Session::spawn`, screenshot, waits, keys, resize, title | have | |
| Rust crate: daemon-backed session (`attach`, `reconnect`, second client, effective geometry) | missing (M) | Needs the shared client crate. |
| Rust crate: default timeouts, `-`/`_`/`C-` chord separators | partial (S) | |
| TypeScript package that drives a TUI Playwright-style | missing (L) | See the design note below. |
| Executable docs | missing (S) | |

**Decided 2026-08-29: the Rust `pty` binary is the engine of the TypeScript
package.** The Node library needs `node-pty` and `@xterm/headless` in the test
process. The new package instead spawns the target with `pty run -d` under a
temporary `PTY_ROOT`, sends keys as `DATA`, and takes screenshots with `PEEK`.
The TypeScript and Rust harnesses then see the same bytes, and the package has
no native dependency.

## 9. TUI library and the session manager

No program outside this repository depends on `pty/tui` today. `pty-layout`
did, and is stale. The interactive session manager is the only in-tree
consumer. A new agent-management program will be built on the Rust library.
So the Node framework is a feature list, not an API to match.

Capabilities the session manager and a `pty-layout`-class program need:

- Terminal session: alt screen, raw mode, cursor show/hide, clean teardown,
  `pause`/`resume` that hands the terminal to an in-process attach client,
  SGR mouse and bracketed paste on/off, `SIGWINCH` with full redraw.
- Cell buffer: char with wide-char placeholder, fg/bg as RGB or default,
  palette index preserved for indexed SGR, bold/dim/italic/underline; ingest
  from ANSI strings; direct `setCell`; diff with minimal cursor moves and
  index-first SGR, wrapped in synchronized output 2026; full render.
- Layout: declarative node tree, vertical flow, rows with flex spacers,
  columns, panels with 4 box styles, title and bottom caption, pinned status
  bar and footer, clipping, centered overlay with shadow.
- Text: semantic or RGB colors, truncation with ellipsis, soft wrap with
  offsets, highlight spans, wcwidth for CJK/emoji/box drawing.
- Lists: scroll region model, selectable and grouped lists with headers.
- Theme: 13-slot theme, 9 semantic tokens, built-in themes including a
  "terminal default" theme, theme → 16-color palette for embedded terminals.
- Input: keys incl. kitty CSI-u, modified arrows, SGR mouse; hit testing;
  stack-based focus scopes; readline-style single-line editing; fuzzy match.
- Embedded live pty (`PtyHandle`): spawn a child or attach to a session over
  the socket; typed cell grid with scrollback offset and wrapped-row flags;
  cursor, mouse mode, alt-screen flag, kitty flag stack, bracketed-paste flag,
  scrollback length and base row; dirty/revision/activity signals; a pane
  widget with border, focus color, selection highlight, and cursor report.
- Session manager: list with running/exited markers, display name and id,
  cwd with `~`, command, `(exited 2h ago)`, `[permanent]`, inline tags; fuzzy
  filter with `host/session` syntax; relay host groups; keys `↑↓ ⏎ q esc
  ctrl+c ctrl+g`; attach and return to the list; one-key create (`$SHELL` in
  `$HOME`, random id, no display name); restart of an exited session;
  `--preselect-new`; `--filter-tag` inheritance; 1 s refresh; theme file.

Note: the README's directory picker and name/command prompt no longer exist
in the Node code. Creation is one keystroke.

**Decided 2026-08-29: build on `ratatui` + `crossterm`** (layout, widgets, and
buffer diff come with them) and add the pty pane on libghostty.

## 10. Rust embedding API

Issue #1 and #3 ask for one Rust terminal handle with `attach` (a persistent
session) and `spawn` (an owned child), typed snapshots, mode state, an event
stream, a private attempt id to drop late events, and one owner thread for the
non-`Send` libghostty terminal. This is the same object as the TUI `PtyHandle`
in section 9. One crate serves both. Proposed layout from issue #1:
`pty-protocol`, `pty-client`, `pty-terminal`, `pty-testkit`, `pty` (the CLI).

Status: missing (L). `deskset/crates/pty-wire` is a partial third copy of
`pty-protocol` + `pty-client`.

## 11. Mixed fleet: Node and Rust on one `$PTY_ROOT`

Drop-in parity covers most of this. What remains:

| Direction | Needs |
|---|---|
| Node client → Rust daemon | `GEOMETRY` frames; the attach ordering and roles; `STATUS` shape; `daemonPid` + `session_start` at publication (so the Node library accepts the daemon); the lock files; metadata fields Node reads (`generation`, `rows`, `cols`, ...). |
| Rust client → Node daemon | Ignore unknown frames (done); drop or keep the `ATTACH` flag byte (Node ignores it); read Node metadata without dropping fields; take the same locks before a write. |
| Both | Same tmp-file and lock conventions; same reserved tags; same `PTY_REAP_ON_EXIT` reading. |

Cost beyond drop-in: small. The items are the same ones drop-in needs. The
one extra is a test rig that runs the shared fixtures with a Node client
against a Rust daemon and the reverse.

## 12. Candidates to leave off

Decided on 2026-08-29. Each row records the decision.

| Item | Size | Decision |
|---|---|---|
| `pty test` (vitest wrapper) | — | Dropped. It belongs to the Node repository. |
| `pty-kill-releases-socket-test` second binary | S | Dropped. The case becomes a Rust test. |
| `remote-serve --socket <path>` | S | Dropped. `--stdio` stays. The Node docs mark the socket form transitional. **`pty remote-serve --help` still describes `--socket`**, because the help texts are vendored from the Node tool byte for byte so the help test can compare them. Passing `--socket` prints a usage line naming only `--stdio` and exits 1. Checked 2026-09-02. |
| Legacy positional display name (`pty run mylabel -- cmd`) and the `Hint:` line | S | Dropped. Nothing in the network uses it. |
| `gc`: permanent respawn, flapping classifier, abandoned reap | L | Kept for the shared-input/SCG reconciler contract. `gc` respawns stopped `strategy=permanent` sessions under their stable id, re-reads bound `pty.toml` definitions with last-known-good fallback, persists the Node-compatible fast-fail tags/events and flapping stop, and reaps cwd-gone or opt-in idle permanents before respawn. The accepted tuning flags are active and validated. |
| `recover` and the `recovery{}` capability | XL | Deferred and documented as absent. No program in the network calls it. Rust daemons omit the capability; Node `list` handles that. Rust preserves the field on rewrite, so `recovery.metadataRevision` goes stale for a session a Rust binary writes to — accepted, decision 0005. |
| `evidence snapshot` / `remove` | M | Kept for exact-generation retained exit evidence and replacement-safe cleanup. |
| `--attach-stream-fd-v1` | M | Kept. An eval cell and relays use it. |
| `PTY_SPAWNER_PID` watchdog | S | Kept. Small. |
| Rust `ATTACH` geometry-neutral flag and `stats.clients.geometryNeutral` | S | Dropped. Node's readonly role covers `peek -f`. |
| Rust-only `<name>.screen` file | S | Dropped once `lastLines` matches. |
| Rust `run --rows/--cols` | S | Kept as an extension. Node persists rows/cols anyway. |
| `up`/`down` and `pty.toml` | S | Kept. Already ported; the binding rule needs the tag pair. |
| `queryStats` waiting out its 2 s timeout on a daemon that closes without STATUS, and `peek -f` hanging on a plain close | S | Not reproduced, on purpose. Both end promptly instead. Deliberate improvements, not gaps — accepted, decision 0006. |

## 12a. How st2 chooses the `pty` binary

**From `PATH`, and nothing else.** `PtyCli.bin` is the literal `"pty"` in both
of st2's non-test constructors (`src/run.rs:378` and `:435` on `origin/main`,
read 2026-09-01). There is no environment variable that selects it.

**`ST2_PTY_BIN` is real but unshipped**, so it is easy to believe in and wrong
to rely on. It exists only on st2's `pty-rust` branch, added by `b63abfb`, and
that branch is not merged: `origin/main` has zero occurrences. Nothing running
today reads it, so no agent can opt one task onto a different build.

**That matters for the cutover.** Every staged rollout in `docs/parity-plan.md`
that begins "`ST2_PTY_BIN` on one agent" needs that st2 branch shipped first,
or it needs a different mechanism. Putting the Rust `pty` on `PATH` is the only
selection st2 supports as it stands.

## 12b. Known red that is not a missing verb

**The whole workspace suite is green as of 2026-09-02: 1293 tests, none
failing.** The three entries this section used to list are gone, and two of
them were not what they were labelled.

| Test | What it turned out to be |
|---|---|
| `scrollback_fidelity.rs::alt_screen_and_normal_scrollback_both_survive_reconnect` | A real gap, now fixed. The replay carried only the alternate screen, so a client that reconnected while a full-screen program ran saw a blank screen the moment that program exited. libghostty offers no reader for the screen that is not in use, so the daemon now copies the normal screen at the moment the child switches away from it. |
| `fixtures_protocol.rs::a_client_that_never_reads_does_not_starve_the_others` | **Not flaky, and this file said it was, twice.** It is a throughput bound that only an optimized build meets. One megabyte of child output through a session with nobody attached: 9.4 s unoptimized, 0.22 s optimized, 0.25 s with the Node tool. The shipped binary is faster than Node; the test build is about forty times slower than either. The budget is now widened when the binary under test is the workspace's own test build. |
| `rm_immediate_reuse.rs::rm_waits_out_the_old_generation_before_permitting_replacement` | Has not failed since. Left as it was. |

**"Flaky" was the wrong word for the middle one and it cost four days.** A test
that fails at random under load and a test that states a bound the profile
cannot meet look identical from the outside, and only one of them is a
scheduling accident. Calling it flaky ends the investigation. See
docs/hardening.md for the same lesson from the other side: a red test inside a
red suite is invisible, and a suite that runs a binary it never built is a
green that measured nothing.

**One surface of the suite is known to be unexamined.** 105 of the 467
substring assertions in the suite check that output *contains* a short common
word. Most are safe, because the word is a marker the test itself printed into
the session and nothing else could have produced it. The rest are the shape
that failed before: `exited`, `removed`, `killed`, `busy`, `not found`,
`restarted`, `vanished`, `null`. Three tests of exactly that shape were passing
for the wrong reason during the 2026-08-31 build, each matching its word in
output that had nothing to do with what it was checking, and each was found by
accident rather than on purpose. Treat a pass from one of them as unproven
until somebody has read it.

## 12c. What differs because the kernel differs

Checked on 2026-09-02, on Linux here and on Apple silicon by another machine.

**Closing a socket that still holds unread data is reported differently by the
two kernels, and macOS is not silent about it — it uses a different errno in a
different place.** Measured on the unix-domain sockets `pty` actually uses:

| | Linux | macOS |
|---|---|---|
| the peer's next **read** | fails, `ECONNRESET`, errno 104 | ends the stream cleanly |
| the peer's next **write** | — | fails, `EPIPE`, errno 32 |

**Both pty implementations inspect the read and both branch on
`ECONNRESET`** — the Node client on `err.code === "ECONNRESET"`, this one on
the same errno — so both reach the same two answers on the same two kernels.
**The boundary is which errno is inspected, not what the kernel is willing to
say.** A client that also treated `EPIPE` as "the session is gone" would reach
the Linux answer on both, and neither implementation does that today.

**What it means for somebody watching a session:** where the reset does not
arrive, a daemon that drops a client with input still unread looks exactly
like one that closed politely, so `pty attach` ends quietly with the last
known code rather than saying the session is gone. Every other way of losing a
daemon — it exits, it is killed, its socket disappears — is reported the same
on both.

`client_attach.rs::reset_maps_to_not_found_or_not_running` asks the kernel
which of the two it is doing and pins the answer, rather than assuming the one
it was written on.

## 12d. Absent on macOS

Measured on Apple silicon on 2026-09-02, by a machine that could run what this
one cannot.

| | |
|---|---|
| **The daemon has no name.** `ps -o comm=` shows the binary's whole path; the Node tool shows `pty-daemon`. | Cosmetic. Nothing reads it. Node rewrites the process-argument region to do this, which means overwriting the memory `argv` points at, in place; `pthread_setname_np` compiles and does not move the field. Both dead ends are written beside `set_process_title`. |

Everything else that was absent there is now done: the child is told its
working directory as the caller wrote it, memory and CPU come from `ps` where
there is no `/proc`, and the build works.

**Nothing else is open there.** The whole suite is green on Apple silicon:
1308 tests, one load-sensitive binary, no real failures. Two things that looked
like macOS defects were not: a character split across DATA frames was a test
that let its child speak before a client was listening, and a detached client
counted twice was a daemon race both implementations share and only a slower
machine can see (decision 0007).

## 13. In flight, not on `main`

Checked again on 2026-09-02.

| Where | What | Where it stands |
|---|---|---|
| Node PR #168 | Persist `lastOutputAtMs`, the time the child last printed | **Merged 2026-08-29, the day this plan was approved, and nobody noticed.** Now ported: the daemon stamps it, persists it at most once a second, and carries it into the exit record. The running stamp lives in the `.activity/<name>.json` sidecar rather than the record (decision 0015). See `crates/pty-conformance/tests/output_activity.rs`. |
| Node PR #173 | Bounded `keep` retention in gc: `--keep-max-age <dur>` (default `7d`, `0` sweeps now), keep-expired reported apart from the plain sweep | **Merged 2026-09-04**, and ported the same day: `pty gc --keep-max-age`, `registry::is_keep_expired` / `DEFAULT_KEEP_MAX_AGE_MS`, `crates/pty-conformance/tests/gc_keep_expiry.rs`. Node's own suite cannot be cross-run here yet — the installed Node binary is 0.12.0, which predates the flag. |
| Node PRs #131, #133 | Generation-bound activity status; revision-guarded send | Both still drafts. Watch. |
| Node PR #60 | Lean core: delete `up`/`down`, gc respawn, flapping | Still held. Superseded by `st2`. Informs section 12. |
| Node issue #167 | `--isolate-env` drops `TERM_PROGRAM` and `GHOSTTY_*`, so a full-screen program cannot tell what terminal it is in | Open. This port carries the same allow-list, so it has the same problem. Fixing it means changing both. |
| Node issue #163 | The `PTY_ROOT` / `PTY_SESSION_DIR` disagreement warns on stderr, and the callers that need the warning throw stderr away | Open. This port carries the same warning. |
| Node issue #107 | No `--version`, and three numbers claim to be the version | Open there. Settled here: `0.13.x-rust+<short-sha>`, and `--version` prints it. |

**The lesson from #168 is about the tracking, not the field.** This table said
"track" and nothing tracked it. A row that names a thing to watch, with nobody
named to watch it and no check that looks, is a note rather than a mechanism.
The port was two days behind the tool it copies and the map still said the
change was in flight. Ask what would have caught it, not who should have
looked.

## 14. Build, packaging, and verification

| Item | Status | Note |
|---|---|---|
| Binary name `pty`; daemon re-execs `current_exe()` | have | The binary path must outlive its sessions. |
| Rust edition 2024, let-chains (≥ 1.88) | have | The README says edition 2024 and Rust 1.88; checked 2026-09-02. |
| `libghostty-vt-sys` needs Zig 0.15.2 and fetches Ghostty source at build | have | A nix package needs a fixed-output fetch. |
| `flake.nix` for this repo | have | Added on `parity`. `st2`'s flake still pins the Node `pty`. |
| Completion files vendored byte for byte | have | Added on `parity`; `checks.completions` compares them. |
| Version string | decided | `0.13.x-rust+<short-sha>`: one minor above the Node line, a `rust` pre-release tag, and the commit. |
| Node test corpus: 120 files, 31k lines; 13 VRS requirements each mapped to test files | oracle | The plan decides which suites to port, which to run as black-box CLI tests against both binaries. |
| Shared fixtures `tests/fixtures/parity/{screens,shapes}.json` | have | Node-owned, vendored here byte-identical. Extend per section 6. |
| Rust tests today: 173 at `e4d6cda` | have | 1293 on `parity` at 2026-09-02, all passing. |

## 15. Rough size of the whole

| Area | Size |
|---|---|
| CLI verbs and text (sections 3) | XL |
| Daemon protocol semantics (section 4) | L |
| Registry: locks, generation, events, metadata (section 5) | L |
| Terminal fixtures and decisions (section 6) | M |
| Remote (section 7) | M |
| Testing library, Rust + TypeScript (section 8) | L |
| TUI library + session manager (section 9) | XL |
| Embedding API and shared crates (section 10) | L |
| Mixed-fleet rig (section 11) | M |
| Packaging and completions (section 14) | M |

Sources for this map: the Node source and its tests at `500eab2`, and — all in
the `compoundingtech/pty` (Node) repository — its `docs/vrs`,
`docs/disk-layout.md`, `docs/client.md`, `docs/testing.md`; this repository at
`e4d6cda`; the `st2`, `pty-relay`, `deskset`, `ding`, `smalltalk`, and `evals`
call sites; issues #1, #3, #4 here and the open issues and PRs on the Node
repository.
