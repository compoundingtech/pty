# Changelog

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
