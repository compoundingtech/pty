# pty

> **Beta** — the CLI and testing library are usable but the API may change before 1.0.

Persistent terminal sessions. Run a process, detach, reconnect later. From anywhere, locally and over SSH.

Uses [@xterm/headless](https://github.com/xtermjs/xterm.js/tree/master/headless) internally.

## Install

```sh
npm install -g @myobie/pty
```

Or with Nix:

```sh
nix profile install github:myobie/pty   # install the CLI
nix develop github:myobie/pty           # dev shell with node, npm, native deps
```

Requires Node.js. Works on macOS and Linux.

## Usage

Run `pty` with no arguments to launch the interactive session manager:

```
╭─ pty ────────────────────────────────────────────────────────────╮
│                                                                  │
│  Filter: (type to filter)                                        │
│                                                                  │
│  ● webserver      ~/projects/myapp          node server.js       │
│  ● worker         ~/projects/myapp          npm run worker       │
│  ● devlog         ~/projects/myapp          tail -f log/dev.log  │
│  ○ migrations     (exited 2h ago)           npm run migrate      │
│  + Create new session...                                         │
│                                                                  │
╰──────────────────────────────────────────────────────────────────╯
 ↑↓ select  ⏎ attach  q quit
```

Arrow keys to navigate, type to filter, Enter to attach, `q` to quit. Creating a new session walks through a directory picker and name/command prompt.

When you detach from a session entered via the interactive list (`Ctrl+\`), you return to the list. The session keeps running in the background.

### Commands

```sh
pty                                       # interactive session manager
pty run -- node server.js                 # start a session (auto-named)
pty run --name myserver -- node server.js # start with an explicit name
pty run -d -- node server.js              # start in the background
pty run -a -- node server.js              # create or attach if already running
pty run -e -- npm test                    # ephemeral: auto-remove on exit

pty list                                  # show active sessions
pty list --json                           # show as JSON

pty attach myserver                       # reconnect to a session
pty attach -r myserver                    # reconnect, auto-restart if exited
pty peek myserver                         # print current screen and exit
pty peek --plain myserver                 # print as plain text (no ANSI)
pty peek -f myserver                      # follow output read-only

pty send myserver "hello"                 # send text (no implicit newline)
pty send myserver $'hello\n'              # send text with newline (shell syntax)
pty send myserver --seq "git status" --seq key:return  # ordered sequence
pty send myserver --seq key:ctrl+c        # send control keys

pty stats                                 # live metrics for all sessions
pty stats myserver                        # stats for a specific session
pty stats --json                          # stats as JSON (includes CPU, memory, PIDs)

pty events myserver                       # follow events in real-time
pty events --all                          # follow events from all sessions
pty events --recent myserver              # show recent events and exit
pty events --json myserver                # output raw JSONL

pty restart myserver                      # restart an exited session
pty kill myserver                         # terminate a running session
pty rm myserver                           # remove an exited session's metadata

pty wrap claude                           # auto-wrap claude in pty sessions
pty unwrap claude                         # remove the wrapper
pty wrap --list                           # show wrapped commands
```

### Wrapping Commands

`pty wrap` creates a small shell script that shadows a command so it always runs in a pty session:

```sh
pty wrap claude
# Now running "claude" anywhere automatically gets a persistent session
```

The wrapper uses `pty run -a` (create or attach if already running), so running the command twice in the same directory reattaches instead of creating a duplicate.

Wrappers live in `~/.local/pty/bin/`. Add it to the front of your PATH:

```sh
export PATH="$HOME/.local/pty/bin:$PATH"
```

Detach with `Ctrl+\`. (Press `Ctrl+\` twice to send it through to the process.)

### Nesting Prevention

If you run `pty run` (or a wrapped command) inside an existing pty session, pty detects the nesting via the `PTY_SESSION` environment variable and runs the command directly instead of creating a session-inside-a-session. This means wrapped commands "just work" inside pty sessions without double-wrapping.

Use `pty run -d` to explicitly create a background session from inside another session.

### Events

Sessions automatically log terminal events — bell, title changes, desktop notifications (OSC 9/99/777), focus requests, and cursor visibility transitions — to per-session JSONL files.

```sh
pty events myserver              # follow events live (like tail -f)
pty events --all                 # follow all sessions, interleaved
pty events --recent myserver     # dump recent events and exit
pty events --json myserver       # raw JSONL output
```

Event files auto-truncate at 1,000 lines and are cleaned up with the 24-hour dead session TTL.

## Client API

@myobie/pty exposes a programmatic TypeScript API for building apps on top of pty sessions. Import from `@myobie/pty/client`.

```typescript
import {
  spawnDaemon, listSessions, getSession,
  SessionConnection, sendData, peekScreen, queryStats,
  EventFollower, readRecentEvents,
  resolveKey,
} from "@myobie/pty/client";
```

### Managing sessions

```typescript
// Create a session
await spawnDaemon({
  name: "myserver",
  command: "node",
  args: ["server.js"],
  displayCommand: "node server.js",
  cwd: "/path/to/project",
  rows: 24,
  cols: 80,
});

// List and query
const sessions = await listSessions();
const stats = await queryStats("myserver");
```

### Connecting to a session

`SessionConnection` provides a bidirectional, event-driven connection without taking over stdin/stdout — ideal for GUI apps, multiplexers, or web interfaces:

```typescript
const conn = new SessionConnection({ name: "myserver", rows: 24, cols: 80 });
const initialScreen = await conn.connect();

conn.on("data", (data) => myTerminalView.write(data));
conn.on("exit", (code) => console.log(`Exited: ${code}`));

conn.write("hello\r");
conn.press("ctrl+c");
conn.resize(30, 100);
conn.disconnect();
```

For simpler operations:

```typescript
await sendData({ name: "myserver", data: ["hello\r"] });
const screen = await peekScreen({ name: "myserver", plain: true });
```

### Following events

```typescript
const follower = new EventFollower({
  names: ["myserver"],
  onEvent: (event) => console.log(event.type, event.ts),
});
follower.start();
```

See **[docs/client.md](docs/client.md)** for the full API reference.

## Testing Library

@myobie/pty includes a terminal testing library — like Playwright, but for the terminal. Spawn any process in a real PTY, send keystrokes, take screenshots, assert on visible output.

```typescript
import { Session } from "@myobie/pty/testing";

const session = Session.spawn("node", ["--experimental-strip-types", "my-app.ts"]);
await session.waitForText("Ready");

session.press("down");
session.press("return");
await session.waitForText("Selected!");

const ss = session.screenshot();
expect(ss.text).toContain("Selected!");
expect(ss.lines[0]).toMatch(/My App/);

session.press("ctrl+c");
await session.waitForAbsent("My App");
await session.close();
```

Works with any process: CLI tools, interactive TUIs, shells, vim, even `top`. The test runs in a real PTY with a real xterm terminal emulator, so you test exactly what users see.

See **[docs/testing.md](docs/testing.md)** for the full API reference, key names, patterns, and tips.

## TUI Framework (alpha)

@myobie/pty also includes an experimental declarative TUI framework for building terminal interfaces with reactive signals, layout, and efficient cell-buffer diffing. Import from `@myobie/pty/tui`.

> **Alpha** — the TUI framework API is unstable and will change. Use it for experiments, not production.

The `demos/` directory has three working apps built with the framework:

- **file-browser** — two-pane directory tree + file preview with soft-wrap and markdown highlighting
- **reminders** — full CRUD backed by `.md` files, three views (list, board, calendar), overlays
- **agent-teams** — live dashboard of a simulated AI agent hierarchy with real-time updates

Run them with `node --experimental-strip-types demos/{name}/main.ts`. Each demo includes unit tests and PTY integration tests that exercise the testing library.

## Tab Completion

```sh
brew install bash-completion  # required for bash on macOS; zsh works out of the box
npm run install-completions
```

## Prior Art

pty focuses on session persistence only — no splits, no panes, no window management. On mobile we don't need or want splits, and on desktop we have kitty/ghostty/native terminal splits. Keep things simple.

- [abduco](https://github.com/martanne/abduco) — minimal session management for terminal programs, handling detach and reattach cleanly. A major inspiration for pty.
- [dtach](https://github.com/crigler/dtach) — emulates the detach feature of screen with minimal overhead.
- [GNU Screen](https://www.gnu.org/software/screen/) — the original terminal multiplexer that pioneered session persistence.
- [tmux](https://github.com/tmux/tmux) — modern terminal multiplexer with session, window, and pane management.

## License

MIT
