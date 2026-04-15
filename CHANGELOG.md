# Changelog

## Unreleased

### Interactive TUI
- Add `--preselect-new` flag: `pty --preselect-new` opens the interactive TUI with "Create new session..." pre-selected (useful for pty-layout panes that should land on the create prompt)
- Add `--filter-tag key=value` flag (repeatable): filters the TUI to sessions matching all given tags AND auto-applies those tags to any session created from this TUI instance — so new sessions (local and remote) stay in the filtered view (e.g., pty-layout layouts)
- Remote session spawns forward filter tags to pty-relay as `--tag key=value` so remote sessions created from a filtered TUI are tagged on the remote side and stay in the filtered view
- Tag filter is shown in the Filter line; remote groups are filtered by their `tags` field when a tag filter is active
- Session rows in the interactive list now show user-facing tags inline (`#key=value`) alongside cwd and command (matches `pty list` output)

### Listing
- `pty list` now shows tags by default (hashtag format, e.g., `#role=web`) — internal bookkeeping keys (`ptyfile*`, `strategy`, `supervisor.status`) are hidden
- `pty list --tags` now means "show all tags including internal bookkeeping" (previously required to show any tags)
- Add `pty list --filter-tag key=value` (repeatable): show only sessions matching all given tags

### Client API
- Export `extractFilterTags` and `matchesAllTags` from `@myobie/pty/client` so third-party tools (e.g., pty-relay) can accept and apply the same `--filter-tag key=value` syntax
- Add optional `tags?: Record<string, string>` on remote session entries so pty-relay can surface tags in `ls --json` and have the interactive TUI filter remote sessions by them
- Add `launcher?: { command: string; args?: string[] }` to `SpawnDaemonOptions` so non-Node callers (Bun, Deno) can route the detached daemon launch through a Node binary — the daemon needs Node to load the `node-pty` native addon (closes #17)

### Project files
- `pty up` now removes tags that were removed from `pty.toml` — toml-managed tag keys are tracked in a `ptyfile.tags` meta tag so manually-added tags (set via `pty tag`) are preserved

### Fixes
- Fix garbage characters in `less`/`git log`: respond to terminal queries (OSC 10/11/4, DA2, DSR, XTVERSION) and strip them from client broadcast so the client's terminal doesn't respond with duplicate input

### pty exec
- Add `pty exec -- <command> [args...]` to replace the current session's command from inside the session
- Updates session metadata so the supervisor restarts the new command, not the original
- Errors if not inside a pty session (`PTY_SESSION` not set) or if the session is managed by a pty.toml
- Preserves existing tags and other metadata
- Emits `session_exec` event with previous and new command
- Interactive TUI shows "Create new session..." for spawn-enabled remote hosts
- Interactive filter hides "Create new session..." items when filter doesn't match "new"
- `host/session` filter syntax: type `prod/api` to filter by host then session
- Extracted `buildFilteredGroups` as a pure function for unit testing

## 0.8.0

### Relay integration
- Interactive TUI (`pty` with no args) discovers [pty-relay](https://github.com/myobie/pty-relay) on PATH and shows remote sessions alongside local ones, grouped by host
- Remote sessions are fetched asynchronously — local sessions render immediately, remote groups appear when the relay responds
- Enter on a remote session spawns `pty-relay connect` with pause/resume
- Add `pty list --remote` to include remote hosts in the text and JSON output
- Graceful degradation: if pty-relay is not installed, nothing changes

### Events
- Add `session_start` event — emitted when a session is created, includes tags for filtering
- Add `session_exit` event — emitted when a session's child process exits, includes exit code

### TUI framework
- Export `SelectableGroup<T>` interface from `@myobie/pty/tui`
- `groupedSelectable` `renderHeader` callback now receives the full group object instead of `(title, count)`
- Empty groups are now rendered (header shown) instead of being silently skipped

## 0.7.2

### launchd
- `pty supervisor launchd install` now compiles a small C wrapper binary (`pty-supervisor`) that validates Full Disk Access before exec'ing node — grant FDA to this binary, not to node itself
- Install flow checks FDA via a one-shot launchd job (no false positives from terminal's FDA)
- Interactive prompt guides user through granting FDA, opens Finder to the binary, verifies after confirmation
- Wrapper bakes in PATH at compile time so child processes can find deno, claude, etc.
- `--path` flag to override the baked-in PATH: `pty supervisor launchd install --path "$PATH"`
- Wrapper runs `--check` for diagnostics: validates node, bundle, and FDA

### Fixes
- Fix `spawnDaemon` leaking orphaned daemon processes on failure — child is now killed if `waitForSocket` times out
- Fix `events --wait` timeout not being cancelled when event is found (caused exit code 1 even on match)
- Fix `displayCommand` duplication in `pty list` for toml-spawned sessions
- `displayCommand` now includes full command + args for `pty run` sessions
- Supervisor logs every skip reason in `doRestart` for debugging
- Supervisor state directory moved to `~/.local/state/pty/supervisor/` (no longer pollutes session dir)

## 0.7.1

### Fixes
- `pty peek` now works on exited sessions by reading saved output from metadata
- `pty peek --wait` handles exited sessions: checks saved output, shows last lines and exit code if pattern not found
- `--wait` accepts multiple patterns (`--wait "passed" --wait "failed"`) — matches on any
- Increase saved output from 20 to 200 lines (`lastLines` in exit metadata)
- Exit metadata saved twice: immediately in `onExit` (for status display) and again in `close()` (for complete output after all PTY data has flushed)
- Fix TUI race where session showed as "running" after exit (delay list refresh 200ms to let metadata flush)
- Fix SKILL.md examples to use multiple `--wait` flags instead of regex syntax

## 0.7.0

### Supervisor
- Add session supervisor: `pty supervisor start` runs a foreground process that watches for sessions with `strategy=permanent` tag and restarts them on exit with exponential backoff (1s→16s, max 5 restarts per 60s)
- `pty supervisor start/stop/status/forget/reset` commands
- `pty supervisor launchd install/uninstall` for macOS auto-start — bundles the supervisor into a portable JS file via esbuild, uses absolute paths to node (no PATH dependency), `KeepAlive=true`
- Supervision is configured entirely through tags (`strategy=permanent` or `strategy=temporary`)
- `strategy=permanent`: restart on exit with backoff. `strategy=temporary`: clean up on exit
- Supervisor detects dead processes via PID liveness check (handles external kills where `exitedAt` is never set)
- Supervisor state persisted in `~/.local/state/pty/supervisor/` (restart counts survive supervisor restarts)
- `pty supervisor reset <name>` clears failed status for retry
- New event types: `session_restart`, `session_failed`, `supervisor_start`, `supervisor_stop`
- 10s periodic scan as safety net for missed `fs.watch` events

### Project files
- Add `pty up` / `pty down` commands to start and stop sessions defined in a `pty.toml` project file
- `pty.toml` supports named sessions with commands, tags, and an optional `prefix` for session naming
- `pty up` accepts a directory argument (`pty up ./backend`) and session name filtering (`pty up dev serve`)
- `pty up` syncs tags from the toml to already-running sessions (without removing manually-added tags)
- `pty up` stores `ptyfile` and `ptyfile.session` tags so the supervisor re-reads the toml on restart
- `pty down` removes strategy tags and stops sessions (including supervised ones), warns about toml-managed sessions

### Mutable tags
- Add `pty tag <name> key=value` / `pty tag <name> --rm key` to set and remove tags on running or exited sessions
- `pty tag <name>` with no args shows current tags
- Warns when modifying tags on toml-managed sessions (changes will be overwritten by `pty up`)
- Atomic metadata writes (write-to-temp + rename) to prevent partial reads

### Peek and wait
- Add `pty peek --wait "text"` to block until text appears on screen, with optional `-t` timeout (seconds)
- Add `pty peek --full` to show full scrollback (not just viewport)
- Add `pty events --wait <type>` to block until a specific event type occurs, with optional `-t` timeout

### CLI improvements
- Add `--cwd` flag to `pty run` to start a session in a specific directory
- Add `--tags` flag to `pty list` to display tags as `#key=value` hashtags
- Colorize `pty list` output: bold cyan session names, dimmed commands
- Interactive TUI list shows `[permanent]`/`[temporary]`/`[failed]` markers with color
- `pty kill` on supervised sessions removes the strategy tag (supervisor won't restart it)
- `pty kill` and `pty down` warn when stopping toml-managed sessions

### Fixes
- Defensive `meta.args` fallback to `[]` in all display code (prevents crashes on old metadata)
- Shell integration tests isolated from real session directory
- Fix flaky TUI filter test (wait for list to re-render, not just input to appear)

## 0.6.0

### Breaking changes
- **`PtyServer` moved from `@myobie/pty/client` to `@myobie/pty/server`** — this keeps `./client` free of native addon dependencies (`node-pty`). Update imports: `import { PtyServer } from "@myobie/pty/server"`
- **`resolveKey` and `parseSeqValue` are still in `@myobie/pty/client`** but also available standalone via the new `@myobie/pty/keys` export (browser-safe, zero dependencies)

### Features
- Add session tags: `pty run --tag owner=forge --tag env=dev -- command` sets key-value metadata on sessions, visible in `pty list --json` and persisted across exits and restarts. Tags are available in `SpawnDaemonOptions`, `ServerOptions`, and `SessionMetadata` for programmatic use (#12)

### Exports
- Add `@myobie/pty/server` subpath export for `PtyServer` and `ServerOptions` (requires `node-pty` native addon)
- Add `@myobie/pty/keys` subpath export for browser-safe key resolution (`resolveKey`, `parseSeqValue` — zero dependencies)
- Add `@myobie/pty/protocol` subpath export for browser-safe wire protocol types (`PacketReader`, `MessageType`, encode/decode helpers) (#11, thanks @schickling)

### Fixes
- Fix `resolveKey` silently dropping shift modifier for non-letter keys: `shift+return` now correctly produces CSI u encoding (`\x1b[13;2u`), `shift+up` produces `\x1b[1;2A`, etc. All modifier combinations (ctrl+shift, alt+shift, ctrl+alt+shift) now work for arrows, navigation keys, and control chars (#13, #14, thanks @schickling)
- Validate session `cwd` before spawning and surface explicit errors (`Working directory does not exist`, `Working directory is not a directory`, `Working directory is not searchable`) instead of failing silently with exit code 1 or misleading `posix_spawnp failed` messages (#9, #10, thanks @schickling)
- Lazy-load the interactive TUI module so non-interactive CLI commands like `pty list` don't crash with `uv_cwd` when launched from a deleted directory (#9, #10)
- Clarify the `posix_spawnp` error message to mention the actual PTY shell and cwd context instead of blaming the wrapped command

## 0.5.0

### Client API (`@myobie/pty/client`)
- New `@myobie/pty/client` entry point for programmatic session management — no TUI framework dependency required
- `SessionConnection` class for bidirectional session connections without taking over stdin/stdout
- `sendData()` — Promise-based alternative to CLI send (no `process.exit()`)
- `peekScreen()` — Promise-based screen capture (no stdout writes)
- Export `queryStats`, `attach`, `peek`, `send` from client API
- Export `PtyServer` and `ServerOptions` for embedding
- Export events system: `EventType`, `EventRecord`, `EventFollower`, `readRecentEvents`, `formatEvent`, and all event subtypes
- Export key resolution: `resolveKey`, `parseSeqValue`
- Export session helpers: `gc`, `validateName`, `cleanupAll`, `cleanupSocket`, `getSocketPath`
- Export protocol types: `PacketReader`, `MessageType`, `Packet`
- `spawnDaemon` now takes an options object with optional `rows`/`cols` (breaking change from positional args)

### CLI improvements
- Add `pty gc` command to remove all exited sessions at once
- Git-style plugin support: `pty <anything>` looks for `pty-<anything>` in PATH and runs it, forwarding remaining args
- Prevent accidental session nesting: `pty run` inside an existing session execs the command directly instead of creating a nested session (`-d` bypasses the check)
- Set `PTY_SESSION` env var in child processes so they can detect they're inside a pty session
- Add CPU and memory usage to `pty stats` (child process and daemon, via `ps`)
- Add process PIDs to `pty stats` output
- Gracefully handle older daemons that don't report resource usage
- Exit messages now include the session name (`[myserver exited with code 0]`)

### Events
- Add terminal event logging — sessions capture bell, title changes, desktop notifications (OSC 9/99/777), focus requests, and cursor visibility transitions to a per-session JSONL file
- Add `pty events <name>` command to follow events in real-time (like `tail -f`)
- Add `pty events --all` to follow events from all sessions, interleaved
- Add `pty events --recent <name>` to show recent events and exit
- Add `pty events --json` for machine-readable JSONL output
- Deduplicate consecutive identical title change events
- Event files auto-truncate at 1,000 lines (keeping most recent 500)
- Event file I/O is fully async (non-blocking write queue)
- Event files are cleaned up with the existing 24-hour dead session TTL

### Fixes
- Respond to DA1 (Primary Device Attribute) queries so fish shell 4.x starts in under 50ms instead of blocking 10s at startup (#5)
- Fix postinstall `spawn-helper` chmod to work under pnpm's global virtual store layout, replacing the broken relative-path `chmod` with a proper Node.js script that uses `createRequire` to find node-pty regardless of layout (#8, thanks @schickling)

### Tests
- Add shell integration tests covering bash, zsh, and fish startup

## 0.4.1

- Add `pty stats` command for live session metrics (terminal size, scrollback, clients, modes, uptime)
- Add `pty stats --json` for machine-readable output
- Add `pty rm` command to remove exited session metadata
- Add `--ephemeral` / `-e` flag to `pty run` for auto-cleanup on exit
- `pty kill` now only kills running sessions (use `pty rm` for exited ones)
- Increase default scrollback from 1,000 to 10,000 lines (matching Ghostty)
- Exited sessions now show cwd in `pty ls` and interactive list
- Exited sessions show command in `pty ls`
- Running sessions always rank above exited in interactive search
- Selecting an exited session in interactive UI restarts it
- New STATUS protocol message (type 7) for querying live session metrics
- Export `spawnDaemon`, `listSessions`, `getSession` from `@myobie/pty/tui`
- Add `cursorRow`, `cursorCol`, `mouseMode`, `scrollback`, `bufferLength`, `baseY` to `PtyHandle`
- Fix build bug: dynamic `require()` paths used `.ts` extension in dist

## 0.3.0

- Add `pty wrap` / `pty unwrap` to auto-wrap commands in pty sessions
- Improve attach fidelity for ratatui/crossterm TUI apps (ECH/CUF serialize fixes, SIGWINCH nudge)

## 0.2.2

- Restrict session directory and socket permissions (0o700 / 0o600)
- Allow following a peek in plain mode (`pty peek -f --plain`)
- Fix lifecycle hooks, command parsing, and peek flag handling

## 0.2.1

- Add fuzzy filter to interactive session list
- Add light themes, terminal theme detection, Ctrl+G theme cycling
- Persist theme preference

## 0.2.0

- Auto-name sessions from command + directory
- Rebuild interactive list with the TUI framework
- Weight session name higher in search results
- Fix global install spawn failure (#4)

## 0.1.3

- Fix doubled keystrokes after session exits in interactive list

## 0.1.2

- Fix CLI for npm global install (build src/ to dist/ with tsc)

## 0.1.1

- Bundle CLI for npm global install compatibility (#3)

## 0.1.0

- Initial release
- Persistent terminal sessions with detach/attach
- Multi-client support
- Interactive session manager
- Playwright-style terminal testing library (`@myobie/pty/testing`)
- Declarative TUI framework (`@myobie/pty/tui`)
