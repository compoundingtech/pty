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

Arrow keys to navigate, type to filter, Enter to attach, `q` to quit. When pty-relay is installed, remote sessions appear grouped by host. Use `host/session` syntax to filter by host (e.g., `prod/api`). Creating a new session walks through a directory picker and name/command prompt.

When you detach from a session entered via the interactive list (`Ctrl+\`), you return to the list. The session keeps running in the background.

### Commands

```sh
pty                                       # interactive session manager
pty --preselect-new                       # open the TUI with "Create new session..." selected
pty --filter-tag layout=work              # TUI filtered by tag; new sessions inherit the tag
pty run -- node server.js                 # start a session (random name + auto displayName)
pty run --name myserver -- node server.js # start with an explicit name (still gets a displayName)
pty run --no-display-name -- bash         # random name, no friendly label (good for throwaway shells)
pty run -d -- node server.js              # start in the background
pty run -a -- node server.js              # create or attach if already running
pty run -e -- npm test                    # ephemeral: auto-remove on exit
pty run --tag owner=forge -- node srv.js  # tag a session with metadata
pty run --cwd /path -- node server.js    # run in a specific directory

pty rename my-label                       # inside a session: add/change its displayName
pty rename <ref> my-label                 # outside: set displayName on <ref>
pty rename --show <ref>                   # show current displayName
pty rename --clear [ref]                  # remove displayName

pty list                                  # show active sessions (tags shown by default)
pty list --tags                           # include internal bookkeeping tags (ptyfile*, strategy, etc.)
pty list --json                           # show as JSON
pty list --remote                         # include remote sessions via pty-relay
pty list --filter-tag role=web            # show only sessions with matching tag (repeatable)

pty attach myserver                       # reconnect to a session
pty attach -r myserver                    # reconnect, auto-restart if exited
pty exec -- codex                         # replace this session's process (inside a session)
pty peek myserver                         # print current screen and exit
pty peek --plain myserver                 # print as plain text (no ANSI)
pty peek --full myserver                  # print full scrollback
pty peek --wait "Listening" myserver      # wait until text appears on screen
pty peek --wait "Ready" -t 10 myserver    # wait with timeout (seconds)
pty peek -f myserver                      # follow output read-only

pty send myserver "hello"                 # send text (no implicit newline)
pty send myserver $'hello\n'              # send text with newline (shell syntax)
pty send myserver --seq "git status" --seq key:return  # ordered sequence
pty send myserver --seq key:ctrl+c        # send control keys
pty send myserver --paste "$(cat prompt.md)"           # wrap as bracketed paste

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
pty gc                                    # remove all exited sessions
pty tag myserver role=web                 # set tags on a session
pty tag myserver --rm role                # remove a tag

pty supervisor start                      # start the session supervisor
pty supervisor stop                       # stop the supervisor
pty supervisor status                     # show supervised sessions
pty supervisor forget myserver            # stop supervising a session
pty supervisor reset myserver             # reset a failed session for retry
pty supervisor launchd install            # install launchd auto-start (macOS)
pty supervisor systemd install            # install user-level systemd auto-start (Linux)
pty supervisor runit install              # install runit service files

pty up                                    # start all sessions from ./pty.toml
pty up ./backend                          # start sessions from ./backend/pty.toml
pty up claude dev                         # start specific sessions from ./pty.toml
pty down                                  # stop all sessions from ./pty.toml
pty down claude                           # stop specific sessions

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

### Project Files

A project can include a `pty.toml` to declare its sessions:

```toml
[sessions.claude]
command = "claude --dangerously-skip-permissions"
tags = { role = "agent" }

[sessions.dev]
command = "deno task dev"
tags = { role = "build" }

[sessions.serve]
command = "bin/serve"
tags = { role = "server" }
```

Run `pty up` in the project directory (or `pty up /path/to/project`) to start all sessions. Run `pty down` to stop them. You can also start specific sessions: `pty up dev serve`.

### Supervisor

The supervisor keeps sessions alive by watching for the `strategy` tag:

```sh
# Tag a session as permanent
pty tag myserver strategy=permanent

# Start the supervisor
pty supervisor start

# If myserver exits, the supervisor restarts it with exponential backoff
# Max 5 restarts in 60 seconds before marking as [failed]

pty supervisor status            # show supervised sessions
pty supervisor forget myserver   # stop supervising
pty supervisor stop              # stop the supervisor
```

Sessions can be supervised from `pty.toml` by setting the `strategy` tag:

```toml
[sessions.serve]
command = "bin/serve"
tags = { strategy = "permanent" }
```

For auto-start:

- macOS: `pty supervisor launchd install` — compiles a small wrapper binary and prompts for Full Disk Access (required for sessions on external/removable volumes)
- Linux/systemd: `pty supervisor systemd install` — installs a user service in `~/.config/systemd/user/` and enables it immediately. If you want it to start at boot before login, enable linger with `sudo loginctl enable-linger $USER`.
- runit: `pty supervisor runit install` — writes a `run` script and symlinkable service directory. By default it uses `~/.config/runit/{sv,service}`; on systems like Void you can point it at `/etc/sv` and `/var/service`.

### Plugins

Like `git`, `pty` supports extensions: if you run `pty foo` and there's a `pty-foo` executable in your `$PATH`, pty will run it with the remaining arguments. This lets you build your own subcommands without modifying pty.

## Client API

@myobie/pty exposes a programmatic TypeScript API for building apps on top of pty sessions. Import from `@myobie/pty/client`.

```typescript
import {
  spawnDaemon, listSessions, getSession,
  SessionConnection, sendData, peekScreen, queryStats,
  EventFollower, readRecentEvents,
  extractFilterTags, matchesAllTags,
} from "@myobie/pty/client";
import { PtyServer } from "@myobie/pty/server";         // native addon (node-pty)
import { resolveKey } from "@myobie/pty/keys";           // browser-safe
import { PacketReader, MessageType } from "@myobie/pty/protocol"; // browser-safe
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

The `demos/` directory has four working apps built with the framework:

- **file-browser** — two-pane directory tree + file preview with soft-wrap and markdown highlighting
- **reminders** — full CRUD backed by `.md` files, three views (list, board, calendar), overlays
- **agent-teams** — live dashboard of a simulated AI agent hierarchy with real-time updates

Run them with `node --experimental-strip-types demos/{name}/main.ts` (or `./demos/run <name>`). Each demo includes unit tests and PTY integration tests that exercise the testing library.

## Skill Reference

For AI coding agents and automation, see **[docs/SKILL.md](docs/SKILL.md)** — a concise guide to running and managing background processes with pty, including session lifecycle, common patterns, and rules for well-behaved agents.

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
