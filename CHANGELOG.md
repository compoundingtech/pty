# Changelog

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
