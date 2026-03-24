# ptym/testing

A terminal testing library — like Playwright, but for the terminal.

Spawn any process in a real PTY, send keystrokes, capture screenshots of the
terminal buffer, and assert on screen state. Works with interactive TUI apps,
shell sessions, and simple command output.

## Install

```sh
npm install ptym
```

Import from `ptym/testing`:

```typescript
import { Session } from "ptym/testing";
```

## Quick Start

```typescript test
import { Session } from "ptym/testing";

const session = Session.spawn("echo", ["hello world"]);
const ss = await session.waitForText("hello world");
expect(ss.text).toContain("hello world");
await session.close();
```

## Session.spawn()

`Session.spawn()` creates a direct PTY process. Use this for testing CLI tools,
shell commands, or any process where you control stdin/stdout directly.

```typescript
const session = Session.spawn(command, args?, opts?)
```

**Options:**
- `rows` — terminal rows (default: 24)
- `cols` — terminal columns (default: 80)
- `cwd` — working directory
- `env` — extra environment variables (merged with `process.env`)

```typescript test
import { Session } from "ptym/testing";

const session = Session.spawn("sh", ["-c", "echo 'spawned!'; sleep 1"]);
const ss = await session.waitForText("spawned!");
expect(ss.text).toContain("spawned!");
await session.close();
```

## Session.server()

`Session.server()` creates a persistent session backed by a `PtyServer`. Use
this when you need detach/reattach, multiple clients, or resize support.

```typescript
const session = await Session.server(command, args?, opts?)
```

**Options:**
- `name` — session name (auto-generated if omitted)
- `rows`, `cols`, `cwd` — same as spawn

After creating, call `session.attach()` to start receiving output:

```typescript test
import { Session } from "ptym/testing";

const session = await Session.server("sh", ["-c", "echo 'served!'; sleep 30"]);
await session.attach();
const ss = await session.waitForText("served!");
expect(ss.text).toContain("served!");
await session.close();
```

## Sending Input

### sendKeys(keys)

Send raw keystrokes:

```typescript test
import { Session } from "ptym/testing";

const session = Session.spawn("cat");
session.sendKeys("hello\n");
const ss = await session.waitForText("hello");
expect(ss.text).toContain("hello");
await session.close();
```

### press(keyName)

Send a named key. Supports modifiers with `+`:

```typescript test
import { Session } from "ptym/testing";

const session = Session.spawn("cat");
session.sendKeys("test line\n");
await session.waitForText("test line");
session.press("ctrl+c");
await session.close();
```

### type(text)

Alias for `sendKeys()` — sends text character by character:

```typescript test
import { Session } from "ptym/testing";

const session = Session.spawn("cat");
session.type("typed text\n");
const ss = await session.waitForText("typed text");
expect(ss.text).toContain("typed text");
await session.close();
```

## Screenshots

`session.screenshot()` captures the current terminal state:

```typescript
const ss = session.screenshot();
ss.lines;  // string[] — each line of the terminal, trailing whitespace trimmed
ss.text;   // string   — all lines joined with "\n"
ss.ansi;   // string   — ANSI-serialized terminal state (includes escape codes)
```

The `lines` array includes scrollback. Trailing empty lines are trimmed.

## Waiting

### waitForText(text, timeoutMs?)

Poll until the terminal contains the given text. Returns the matching screenshot.
Default timeout: 5000ms.

```typescript test
import { Session } from "ptym/testing";

const session = Session.spawn("sh", ["-c", "sleep 0.1; echo 'delayed'"]);
const ss = await session.waitForText("delayed", 3000);
expect(ss.text).toContain("delayed");
await session.close();
```

### waitForAbsent(text, timeoutMs?)

Poll until the terminal no longer contains the given text:

```typescript test
import { Session } from "ptym/testing";

const session = Session.spawn("sh", ["-c", "echo 'gone'; sleep 0.1; printf '\\033[2J\\033[H'"]);
await session.waitForText("gone");
// Screen clears after 100ms
const ss = await session.waitForAbsent("gone", 3000);
expect(ss.text).not.toContain("gone");
await session.close();
```

### waitFor(predicate, timeoutMs?, description?)

Poll until a custom predicate returns true:

```typescript test
import { Session } from "ptym/testing";

const session = Session.spawn("sh", ["-c", "echo 'line 1'; echo 'line 2'; sleep 1"]);
const ss = await session.waitFor(
  (ss) => ss.lines.length >= 2,
  3000,
  "at least 2 lines"
);
expect(ss.lines.length).toBeGreaterThanOrEqual(2);
await session.close();
```

## Key Names

The `press()` method accepts these key names:

| Key | Name(s) |
|-----|---------|
| Enter | `return`, `enter` |
| Tab | `tab` |
| Escape | `escape`, `esc` |
| Space | `space` |
| Backspace | `backspace` |
| Delete | `delete` |
| Arrow Up | `up` |
| Arrow Down | `down` |
| Arrow Left | `left` |
| Arrow Right | `right` |
| Home | `home` |
| End | `end` |
| Page Up | `pageup` |
| Page Down | `pagedown` |

Modifiers: `ctrl+`, `alt+`, `shift+`

Examples: `ctrl+c`, `ctrl+z`, `alt+x`, `shift+a`, `ctrl+backspace`

## Patterns

### Testing a CLI tool

```typescript test
import { Session } from "ptym/testing";

const session = Session.spawn("sh", ["-c", "echo 'Usage: mytool [options]'; sleep 1"]);
const ss = await session.waitForText("Usage:");
expect(ss.text).toContain("mytool");
await session.close();
```

### Testing colored output

Use the `ansi` field to verify ANSI escape codes:

```typescript test
import { Session } from "ptym/testing";

const session = Session.spawn("sh", ["-c", "printf '\\033[31mERROR\\033[0m\\n'; sleep 1"]);
const ss = await session.waitForText("ERROR");
expect(ss.text).toContain("ERROR");
expect(ss.ansi).toMatch(/\x1b\[31m/);
await session.close();
```

### Testing an interactive TUI (vim)

Full-screen TUI apps work out of the box — xterm-headless tracks the alternate
screen buffer, cursor position, and all rendering. Here's a complete example
that launches vim, enters insert mode, types text, and verifies the result:

```typescript test
import { Session } from "ptym/testing";

const session = Session.spawn("vim", ["--clean"], { rows: 24, cols: 80 });

// Wait for vim to start — the welcome screen shows "VIM - Vi IMproved"
await session.waitForText("VIM - Vi IMproved", 10000);

// Enter insert mode and verify the mode indicator
session.sendKeys("i");
await session.waitForText("INSERT");

// Type some text and verify it appears
session.sendKeys("Hello from a test!");
const ss = await session.waitForText("Hello from a test!");
expect(ss.text).toContain("Hello from a test!");
expect(ss.text).toMatch(/INSERT/i);

// Exit: Escape to normal mode, then :q! to quit without saving
session.press("escape");
session.sendKeys(":q!\n");
await session.close();
```

### Testing an interactive shell session

For programs that show a prompt and accept commands, use `waitFor()` with a
regex to detect the prompt, then send commands and assert on their output:

```typescript test
import { Session } from "ptym/testing";

const session = Session.spawn("bash", ["--norc", "--noprofile"]);

// Wait for the shell prompt ($ or #)
await session.waitFor((ss) => /[$#]/.test(ss.text), 5000, "shell prompt");

// Run a command and check the output
session.sendKeys("echo hello-from-test\n");
const ss = await session.waitForText("hello-from-test");
expect(ss.text).toContain("hello-from-test");

// Ctrl+C interrupts a long-running command
session.sendKeys("sleep 999\n");
await new Promise((r) => setTimeout(r, 200));
session.press("ctrl+c");

// Shell should give us a prompt back
await session.waitFor((ss) => /[$#]\s*$/.test(ss.lines[ss.lines.length - 1] ?? ""), 3000, "prompt after ctrl+c");

session.sendKeys("exit\n");
await session.close();
```

## Server-Mode Methods

These methods are only available on sessions created with `Session.server()`.

### attach()

Start receiving output from the server. Required after `Session.server()`:

```typescript
const session = await Session.server("bash", ["--norc"]);
await session.attach();
await session.waitForText("$");
```

### reconnect()

Simulate a detach + reattach cycle. Destroys the connection, resets the
terminal, and reconnects:

```typescript
const session = await Session.server("bash", ["--norc"]);
await session.attach();
await session.waitForText("$");
session.sendKeys("echo hello\n");
await session.waitForText("hello");

// Simulate disconnect and reconnect
await session.reconnect();
// Screen state is replayed — "hello" should still be visible
await session.waitForText("hello");
```

### resize(rows, cols)

Resize the terminal dimensions:

```typescript
session.resize(40, 120);
```

### connectToExisting(session)

Create a second client attached to the same server process:

```typescript
const session1 = await Session.server("bash", ["--norc"]);
await session1.attach();

const session2 = await Session.connectToExisting(session1);
await session2.attach();

// Both clients see the same terminal output
session1.sendKeys("echo shared\n");
await session2.waitForText("shared");
```

### Properties

- `session.hasExited` — whether the process has exited (always `false` for spawn-mode)
- `session.name` — the session name (server-mode only)
- `session.server` — the underlying `PtyServer` instance (server-mode only)
- `session.rows` / `session.cols` — current terminal dimensions

## Running Tests

Use the `pty test` command (a thin vitest wrapper):

```sh
pty test                  # run all tests
pty test watch            # watch mode
pty test -t "pattern"     # run matching tests
```

Or use vitest directly:

```sh
npx vitest run
npx vitest
```

## Tips

- **Timeouts**: Increase timeout for slow-starting programs (vim, nano). Pass
  a longer `timeoutMs` to `waitForText()` and set a longer vitest timeout on
  the test itself.

- **Debugging**: When a test fails, the error includes the current screen
  content. Use `console.log(session.screenshot().text)` to inspect the terminal
  at any point.

- **Working directory isolation**: Create a temp directory per test to avoid
  test pollution:

  ```typescript
  import * as fs from "node:fs";
  import * as os from "node:os";
  import * as path from "node:path";

  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "mytest-"));
  const session = Session.spawn("ls", [tmpDir]);
  ```

- **Server mode for detach/reattach**: Use `Session.server()` when testing
  reconnection behavior. Call `session.reconnect()` to simulate a detach +
  reattach cycle.
