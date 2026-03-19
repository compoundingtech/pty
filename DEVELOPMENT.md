# pty — Development Guide

A persistent terminal session manager. Run long-lived processes, detach, reconnect later — from any machine over SSH. The npm package name is `ptym`; the CLI command remains `pty`.

## Objectives

- Replace tmux/zellij for **session persistence** of long-running processes
- Simple CLI: `pty run <name> <command>`, `pty attach <name>`, `pty peek <name>`, `pty restart <name>`
- Reliable detach/reattach with full screen replay (colors, cursor position, scrollback)
- Work seamlessly over SSH (Unix sockets, no port management)
- Multi-client support (multiple people can observe or interact with a session)
- Comprehensive test coverage — unit and integration tests for all features

## Non-goals

- **Not a window manager.** No splits, tabs, or layouts. Use kitty for that.
- **Not a shell.** It wraps a single command per session.
- **Not a security boundary.** Unix socket permissions are standard file permissions. Peek mode is a convenience, not access control.

## Quick Reference

```sh
npm install            # install dependencies
npm run typecheck      # typecheck with tsc (no emit)
npm test               # run all tests once
npm run test:watch     # run tests in watch mode
npm run verify-docs    # run executable examples in docs/testing.md

# Usage (during development)
npx tsx src/cli.ts run <name> -- <command> [args...]
npx tsx src/cli.ts run -d <name> -- <command> [args...]
npx tsx src/cli.ts attach <name>
npx tsx src/cli.ts peek <name>
npx tsx src/cli.ts peek -f <name>
npx tsx src/cli.ts list
npx tsx src/cli.ts restart <name>
npx tsx src/cli.ts kill <name>
npx tsx src/cli.ts test                  # run tests via vitest
npx tsx src/cli.ts test -t "pattern"     # run matching tests
```

Detach from any attached/following session with **Ctrl+\\**. Press Ctrl+\\ twice quickly to send it to the process.

### No build step

This project ships TypeScript source directly — there is no compile step. All `.ts` files use `.ts` import extensions and are executed at runtime by [tsx](https://tsx.is). The `bin/pty` entry point locates tsx from `node_modules` and runs `src/cli.ts`. The `tsc` command is used only for typechecking (`noEmit: true`).

## Architecture

```
┌─────────────────────────────────────────────┐
│  Daemon (one per session)                   │
│                                             │
│  ┌──────────┐    ┌───────────────────────┐  │
│  │ node-pty │───▶│ xterm-headless        │  │
│  │ (PTY)    │    │ (screen buffer)       │  │
│  └──────────┘    │ + SerializeAddon      │  │
│       ▲          └───────────────────────┘  │
│       │                    │                │
│       │              serialize()            │
│       │                    ▼                │
│  ┌──────────────────────────────────────┐   │
│  │ Unix Socket Server                   │   │
│  │ ~/.local/state/pty/<name>.sock       │   │
│  └──────────────────────────────────────┘   │
│       ▲          ▲            ▲             │
└───────┼──────────┼────────────┼─────────────┘
        │          │            │
     Client     Client       Peek
     (attach)   (attach)     (read-only)
```

Each session is a detached Node.js process running `src/server.ts` via tsx. It spawns the command in a PTY, feeds all output through xterm-headless to maintain a screen buffer, and listens on a Unix socket for client connections.

## Protocol

Binary packets over Unix sockets: `[type: uint8][length: uint32BE][payload]`

| Type | ID | Direction | Payload |
|------|----|-----------|---------|
| DATA | 0 | Both | Raw terminal bytes |
| ATTACH | 1 | Client → Server | `[rows: uint16BE, cols: uint16BE]` (4 bytes) |
| DETACH | 2 | Client → Server | Empty |
| RESIZE | 3 | Client → Server | `[rows: uint16BE, cols: uint16BE]` (4 bytes) |
| EXIT | 4 | Server → Client | `[exitCode: int32BE]` (4 bytes) |
| SCREEN | 5 | Server → Client | ANSI escape sequences (string) |
| PEEK | 6 | Client → Server | Empty |

`PacketReader` handles streaming reassembly of partial reads. Decoders gracefully handle truncated payloads (defaults for size, -1 for exit code). Unknown message types are silently ignored by the server.

## Key Design Decisions

### No build step — ship TypeScript directly

There is no `dist/` directory. Source files use `.ts` import extensions and run directly via tsx. The `tsconfig.json` has `noEmit: true` and `allowImportingTsExtensions: true` — tsc is only used for typechecking. This eliminates the compile-then-run dance entirely. tsx is a regular dependency (not devDependency) because it's needed at runtime.

### No TypeScript enums

We avoid TS enums because they emit runtime code that can't be type-stripped. Instead, we use `as const` objects with a derived union type. This is compatible with all TS runtimes: Node's native type stripping, tsx, esbuild, swc.

### Peek clients don't affect terminal size

The PTY can only be one size. If a peek client's terminal size were used, it could reflow the session — imagine vim at 120x40 suddenly becoming 40x20 because someone peeked from their phone. Readonly clients are excluded from size negotiation entirely. They see whatever fits; the active user's layout is never disrupted.

### Last attached client wins for size

When multiple interactive (non-peek) clients are connected, the most recently attached client's terminal size is used for the PTY. This is simple and predictable. An alternative would be minimum dimensions across all clients, but that punishes the primary user when a smaller client connects.

### xterm-headless as the screen buffer

The terminal emulation problem (parsing ANSI sequences, tracking cursor, colors, alternate screen, etc.) is genuinely hard. Instead of reimplementing it, we use xterm-headless — the same terminal engine as VS Code's terminal. The `SerializeAddon` produces ANSI escape sequences that reconstruct the screen state in any real terminal on reconnect.

### One daemon per session

Each session is an independent Node.js process. No central daemon, no shared state. This means a crash in one session doesn't affect others, and sessions are naturally isolated. The tradeoff is slightly higher memory per session.

### Unix sockets for local IPC

Fast, zero-config, and work transparently over SSH. No ports to manage, no firewall rules. Session sockets live at `~/.local/state/pty/<name>.sock` (override with `$PTY_SESSION_DIR`).

### PtyServer never calls process.exit()

The `PtyServer` class is a library — it cleans up resources via `close()` but never exits the process. Only the daemon entry point (bottom of `server.ts`) wires up signal handlers and `process.exit()`. This keeps the class testable in-process with vitest.

### Double Ctrl+\ passthrough

Ctrl+\ is the detach key. Since some programs (like vim or hx) may use Ctrl+\, pressing it twice quickly within 300ms sends Ctrl+\ to the process instead of detaching. This matches the UX pattern from `screen` and `tmux` where the prefix key is sent to the process by pressing it twice.

Programs that enable the Kitty keyboard protocol (claude, helix, neovim, etc.) cause the terminal to encode Ctrl+\ as `\x1b[92;5u` instead of the legacy byte `0x1c`. The client normalizes both encodings before detach processing so the detach key works regardless of which keyboard protocol is active.

### Terminal sanitize on disconnect

When a client detaches or a session exits, the client writes escape sequences to reset terminal modes that the PTY program may have enabled: mouse tracking (all three modes), SGR mouse, hidden cursor, and bracketed paste. This prevents "poisoned" terminal state (e.g., mouse movements producing escape characters) without clearing screen content.

### Spawn through shell

The server spawns commands via `/bin/sh -c 'exec "$@"'` rather than calling `posix_spawnp` directly. This handles shell scripts, symlinks, shebangs, and other executable formats that `posix_spawnp` may reject. `exec` replaces the shell with the actual process so there is no extra process. This matches the approach used by tmux and screen.

### Session name validation

Session names are restricted to `[a-zA-Z0-9._-]` to prevent path traversal and shell injection. This is validated before any filesystem operations.

### Race condition prevention

Concurrent `pty run` calls with the same name are protected by an exclusive lock file (`<name>.lock`). The lock includes the PID and is automatically stolen if the holding process dies.

### Daemon spawn failure detection

When `spawnDaemon` launches the daemon, it monitors the child for early exit. If the daemon crashes before the socket appears, the error is surfaced immediately instead of waiting for the 3-second timeout.

## Testing

Tests use **vitest** and live in `tests/`. The `ptym/testing` library (`src/testing/`) provides a `Session` class used by the tests — see [docs/testing.md](docs/testing.md) for the full API.

- `protocol.test.ts` — Unit tests for packet encoding, decoding, and streaming reassembly (partial reads, split packets, large payloads)
- `keys.test.ts` — Unit tests for key name resolution (`resolveKey`, `parseSeqValue`)
- `sanitize.test.ts` — Unit tests for terminal sanitization escape sequences
- `integration.test.ts` — Full integration tests that spawn real PTY sessions, connect clients via sockets, and verify behavior
- `screenshot.test.ts` — Screenshot-based tests using `Session.server()` that capture terminal state (ANSI and plain text) and assert on visual output from real programs (vim, nano, ls, bash). Covers control characters, terminal resize, alternate screen buffer, multiple clients, unicode, and daemon spawning.
- `tui.test.ts` — Interactive TUI tests using `Session.spawn()` that drive the `pty` session manager UI: session list rendering, filtering, keyboard navigation, attach/detach cycles, and the create wizard.

All tests run in `/tmp/` directories to avoid polluting the project folder (e.g., vim swap files). Integration and screenshot tests use real processes and real Unix sockets. Each test creates a uniquely-named session and cleans up afterward. There is a `200ms` delay in some tests to allow xterm-headless to process async writes before checking screen state — this is a known characteristic of xterm's write pipeline, not a flaky test.

### Running tests

```sh
npm test                       # run once (or: pty test)
npm run test:watch             # watch mode (or: pty test watch)
npx vitest run -t "peek"      # run tests matching "peek"
npm run verify-docs            # run executable examples in docs/testing.md
```

### node-pty on macOS

npm sometimes extracts the `spawn-helper` binary without the execute bit. The `postinstall` script in `package.json` fixes this automatically. If you still see `posix_spawnp failed`, run manually:

```sh
chmod +x node_modules/node-pty/prebuilds/darwin-arm64/spawn-helper
```

## File Structure

```
src/
  cli.ts          CLI entry point and command routing
  server.ts       PtyServer class + daemon entry point
  client.ts       attach() and peek() functions
  protocol.ts     Packet types, encoding, decoding, PacketReader
  sessions.ts     Session discovery, socket/PID file management
  keys.ts         Key name resolution (e.g. "ctrl+c" → bytes)
  spawn.ts        Daemon spawning logic
  tui/            Interactive session manager UI
  testing/        Testing library (exported as ptym/testing)
    index.ts      Public re-exports
    session.ts    Session class (spawn + server backends)
    screenshot.ts Screenshot capture helper
    types.ts      Screenshot, SpawnOptions, ServerOptions interfaces
tests/
  protocol.test.ts
  keys.test.ts
  sanitize.test.ts
  integration.test.ts
  screenshot.test.ts
  tui.test.ts
docs/
  testing.md      Testing library documentation (with executable examples)
scripts/
  verify-docs.ts  Extracts and runs doc examples via vitest
completions/
  pty.bash        Bash tab completion
  pty.zsh         Zsh tab completion
bin/
  pty             Entry point (locates tsx, runs src/cli.ts)
```

## Future

### WebSocket server

Add a WebSocket listener alongside the Unix socket so remote clients (phones, browsers) can connect without SSH. The `PtyServer` already manages clients generically — a WebSocket client would be another entry in the clients map, same protocol.

### Web UI

A lightweight browser-based terminal UI that connects via WebSocket. Could use xterm.js on the client side since the server already speaks its language.

### Native SwiftUI app

An iOS/macOS app for connecting to sessions over the network (via Tailscale). The binary protocol is simple enough to implement in Swift. The app would list available sessions, connect via WebSocket, and render the terminal natively.

### Kitty kitten

A Python kitten for kitty that lists sessions and opens them in new kitty windows. Would use kitty's remote control protocol to create windows and the Unix socket to attach.

### Session groups / profiles

Named configurations for starting multiple related sessions at once (e.g., "start my dev environment" → web server + database + file watcher).
