# pty

Persistent terminal sessions. Run a process, detach, reconnect later. From anywhere, locally and over SSH.

Uses [@xterm/headless](https://github.com/xtermjs/xterm.js/tree/master/headless) internally.

## Install

```sh
git clone https://github.com/myobie/pty.git
cd pty
npm install
npm link
```

Or install directly from GitHub:

```sh
npm install -g github:myobie/pty
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
pty run myserver -- node server.js        # start a session and attach
pty run -d myserver -- node server.js     # start in the background
pty run -a myserver -- node server.js     # create or attach if already running

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

pty restart myserver                      # restart an exited session
pty kill myserver                         # terminate a session
```

Detach with `Ctrl+\`. (Press `Ctrl+\` twice to send it through to the process.)

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
