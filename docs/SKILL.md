# PTY — Run and manage background processes

Run `pty --help` to see the full command reference.

Use `pty` to run processes in managed terminal sessions. Prefer pty over raw
background commands (`&`, `nohup`, piping) for better lifecycle control,
readable output, and the ability to wait for results.

## When to use

- Running any long-lived or background process (dev servers, test suites, builds)
- Interactive CLI tools that need a real terminal (auth via keychain, TUI, REPL)
- Any task where you want to start work, do something else, then check results
- Processes you may need to re-read output from later

## Session lifecycle

### 1. Create a detached session

```bash
pty run -d --name <descriptive-name> --tag owner=<agent> -- <command> [args...]
```

Use `--cwd` to run in a specific directory:

```bash
pty run -d --name <name> --cwd /path/to/project --tag owner=<agent> -- <command>
```

Tag sessions so they are identifiable. Never touch sessions you did not create.

### 2. Wait for the process to be ready

```bash
pty peek --wait "expected text" --plain <name> -t 10
```

Check the output to confirm the process has started (e.g. "Authenticated",
"Listening", a prompt character).

### 3. Send input (if interactive)

```bash
pty send <name> "your message here"
pty send <name> --seq key:return
```

For multi-step input use `--seq` chains:

```bash
pty send <name> --seq "first line" --seq key:return
```

Use `--with-delay` if the tool needs time between inputs:

```bash
pty send <name> --with-delay 0.5 --seq "text" --seq key:return
```

### 4. Wait for output and read results

Wait for specific text to appear:

```bash
pty peek --wait "expected output" --plain <name> -t 120
```

Read the full scrollback (not just the visible screen):

```bash
pty peek --full --plain <name>
```

Read just the current visible screen:

```bash
pty peek --plain <name>
```

Check recent events without following:

```bash
pty events --recent <name>
```

### 5. Clean up

Always kill sessions you created when done:

```bash
pty kill <name>
```

## Rules

- **Always use `--plain` with peek** so output is readable (no ANSI escapes)
- **Always use `-t` (timeout)** on `--wait` so you don't block forever
- **Always tag sessions** so they are identifiable and attributable
- **Always clean up** — kill sessions when you are done
- **Never touch sessions you did not create** — check `pty list --tags` if unsure
- **Use `--full` when output may exceed the screen** — peek without it only
  shows the visible terminal buffer

## Naming conventions

Name sessions by purpose: `gemini-review`, `test-runner`, `dev-server`, etc.
Keep names lowercase with hyphens.

## Quick reference

| Action | Command |
|---|---|
| Create detached | `pty run -d --name <n> --tag owner=<a> -- <cmd>` |
| Peek (plain) | `pty peek --plain <n>` |
| Peek (full scrollback) | `pty peek --full --plain <n>` |
| Wait for text | `pty peek --wait "text" --plain <n> -t 30` |
| Send text | `pty send <n> "text"` |
| Send enter | `pty send <n> --seq key:return` |
| Recent events | `pty events --recent <n>` |
| Follow events | `pty events <n>` |
| List sessions | `pty list --tags` |
| Kill session | `pty kill <n>` |

## Common patterns

### Run a dev server and wait for it

```bash
pty run -d --name dev-server --tag owner=agent -- npm run dev
pty peek --wait "Listening" --plain dev-server -t 30
# Server is ready — do your work
pty kill dev-server
```

### Run tests and read results

```bash
pty run -d --name tests --tag owner=agent -- npm test
pty peek --wait "passed\|failed" --plain tests -t 120
pty peek --full --plain tests
pty kill tests
```

### Run a build and check for errors

```bash
pty run -d --name build --tag owner=agent -- npm run build
pty peek --wait "error\|successfully" --plain build -t 60
pty peek --full --plain build
pty kill build
```
