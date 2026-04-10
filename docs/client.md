# Client API Reference

Import from `@myobie/pty/client`.

```typescript
import { SessionConnection, spawnDaemon, listSessions } from "@myobie/pty/client";
import { PtyServer } from "@myobie/pty/server";
```

## Session Management

### `listSessions(): Promise<SessionInfo[]>`

List all sessions (running + exited within 24h).

### `getSession(name: string): Promise<SessionInfo | null>`

Get a single session by name.

### `validateName(name: string): void`

Throws if the name is invalid. Names must match `[a-zA-Z0-9._-]` and be at most 255 characters.

### `getSessionDir(): string`

Returns the session directory path (`PTY_SESSION_DIR` env var or `~/.local/state/pty`).

### `getSocketPath(name: string): string`

Returns the Unix socket path for a session.

### `gc(): Promise<string[]>`

Remove all exited sessions. Returns the names of removed sessions.

```typescript
const removed = await gc();
console.log(`Cleaned up ${removed.length} sessions`);
```

### `cleanupSocket(name: string): void`

Remove a session's `.sock` and `.pid` files.

### `cleanupAll(name: string): void`

Remove all files for a session (socket, pid, metadata, events, lock).

### Types

```typescript
interface SessionInfo {
  name: string;
  socketPath: string;
  pid: number | null;
  status: "running" | "exited";
  metadata: SessionMetadata | null;
}

interface SessionMetadata {
  command: string;
  args: string[];
  displayCommand: string;
  cwd: string;
  createdAt: string;
  exitCode?: number;
  exitedAt?: string;
  lastLines?: string[];
  tags?: Record<string, string>;
}
```

## Session Creation

### `spawnDaemon(options: SpawnDaemonOptions): Promise<void>`

Spawn a new session daemon. Resolves once the daemon is listening.

```typescript
interface SpawnDaemonOptions {
  name: string;
  command: string;
  args: string[];
  displayCommand: string;
  cwd?: string;                      // defaults to process.cwd()
  ephemeral?: boolean;               // auto-remove on exit
  rows?: number;                     // defaults to process.stdout.rows ?? 24
  cols?: number;                     // defaults to process.stdout.columns ?? 80
  tags?: Record<string, string>;     // key-value metadata (e.g. { owner: "forge" })
}
```

### `resolveCommand(cmd: string): string`

Resolve a command name to an absolute path (like `which`). Throws if not found.

### `waitForSocket(name: string, timeoutMs: number, earlyCheck?: () => void): Promise<void>`

Wait for a session's Unix socket to appear on disk.

### `@myobie/pty/server`

Import the server class from `@myobie/pty/server` when you need to embed a pty server directly:

```typescript
import { PtyServer } from "@myobie/pty/server";

const server = new PtyServer({
  name: "embedded",
  command: "bash",
  args: [],
  displayCommand: "bash",
  cwd: process.cwd(),
  rows: 24,
  cols: 80,
  onExit: (code) => console.log(`Exited: ${code}`),
});

await server.ready;
// server is now listening on its Unix socket
```

## Session Interaction (Programmatic)

These functions do not use `process.stdin`, `process.stdout`, or call `process.exit()`. Safe for use in GUI apps, servers, and libraries.

### `SessionConnection`

Bidirectional, event-driven connection to a session.

```typescript
const conn = new SessionConnection({ name: "myserver", rows: 24, cols: 80 });
const initialScreen = await conn.connect();

conn.on("data", (data: string) => { /* terminal output */ });
conn.on("exit", (code: number) => { /* process exited */ });
conn.on("close", () => { /* connection closed */ });
conn.on("error", (err: Error) => { /* connection error */ });

conn.write("hello\r");          // send raw data
conn.press("ctrl+c");           // send named key
conn.resize(30, 100);           // resize terminal
conn.disconnect();               // close connection
```

**Properties:**
- `connected: boolean` — whether the connection is active

**Events:**
| Event | Payload | Description |
|---|---|---|
| `data` | `string` | Terminal output from the session |
| `screen` | `string` | Initial screen replay on connect |
| `exit` | `number` | Session process exited with code |
| `close` | — | Connection closed |
| `error` | `Error` | Connection error |

### `sendData(options: SendDataOptions): Promise<void>`

Send data to a session without connecting interactively. Resolves on success, rejects on error.

```typescript
await sendData({ name: "myserver", data: ["hello\r"] });

// With delay between items
await sendData({ name: "myserver", data: ["git status\r", "git diff\r"], delayMs: 500 });
```

### `peekScreen(options: PeekScreenOptions): Promise<string>`

Get the current screen content as a string.

```typescript
const screen = await peekScreen({ name: "myserver" });         // ANSI output
const plain = await peekScreen({ name: "myserver", plain: true }); // plain text
```

### `queryStats(name: string, timeoutMs?: number): Promise<StatsResult>`

Query live metrics from a running session.

```typescript
interface StatsResult {
  name: string;
  terminal: {
    cols: number; rows: number;
    cursorX: number; cursorY: number;
    scrollbackUsed: number; scrollbackCapacity: number;
  };
  process: {
    alive: boolean; exitCode: number | null;
    pid: number | null;
    resources: ProcessResources | null;
  };
  daemon: {
    pid: number;
    resources: ProcessResources | null;
  };
  clients: { total: number; attached: number; readOnly: number };
  modes: {
    sgrMouse: boolean; cursorHidden: boolean;
    kittyKeyboard: boolean; kittyKeyboardFlags: number[];
  };
  uptimeSeconds: number | null;
  createdAt: string | null;
}

interface ProcessResources {
  rssKb: number;
  cpuPercent: number;
}
```

## Session Interaction (CLI-oriented)

These functions use `process.stdin`/`process.stdout` directly and may call `process.exit()`. They are re-exported for tools that want CLI-like behavior.

### `attach(options: AttachOptions): void`

Interactive attach with bidirectional I/O. Takes over stdin/stdout. Ctrl+\ to detach (double-tap to send through).

### `peek(options: PeekOptions): void`

Read-only view. Writes directly to stdout.

### `send(options: SendOptions): void`

Send data to a session. Calls `process.exit(0)` on success, `process.exit(1)` on error.

## Events

### `EventFollower`

Follow events from one or more sessions in real-time.

```typescript
const follower = new EventFollower({
  names: ["myserver"],           // or omit for all sessions
  onEvent: (event) => {
    console.log(event.type, event.ts);
  },
});
follower.start();
// later:
follower.stop();
```

### `readRecentEvents(name: string, count?: number): EventRecord[]`

Read the last N events (default 50) for a session.

### `formatEvent(event: EventRecord): string`

Format an event for console output with timestamp.

### `EventType`

```typescript
const EventType = {
  BELL: "bell",
  TITLE_CHANGE: "title_change",
  NOTIFICATION: "notification",
  FOCUS_REQUEST: "focus_request",
  CURSOR_VISIBLE: "cursor_visible",
};
```

### Event types

```typescript
type EventRecord =
  | BellEvent
  | TitleChangeEvent
  | NotificationEvent
  | FocusRequestEvent
  | CursorVisibleEvent;
```

Each extends `EventBase { session: string; type: EventType; ts: string }`.

`NotificationEvent` adds `title?`, `body?`, `source?: "osc9" | "osc99" | "osc777"`.
`TitleChangeEvent` adds `value: string`.

## Keys

### `resolveKey(spec: string): string`

Resolve a key name to its byte sequence. Supports:

- Named keys: `return`, `tab`, `escape`, `space`, `backspace`, `delete`
- Arrows: `up`, `down`, `left`, `right`
- Navigation: `home`, `end`, `pageup`, `pagedown`
- Modifiers: `ctrl+c`, `alt+x`, `shift+a`

### `parseSeqValue(value: string): string`

If value starts with `key:`, resolves the key name. Otherwise returns the literal string.

## Protocol (Advanced)

Low-level protocol types for building custom clients. Also available as a standalone browser-safe import via `@myobie/pty/protocol` (no Node-only dependencies).

### `PacketReader`

Streaming packet parser. Feed raw socket data, get parsed packets.

```typescript
const reader = new PacketReader();
socket.on("data", (raw) => {
  const packets = reader.feed(raw);
  for (const packet of packets) {
    // packet.type: MessageType, packet.payload: Buffer
  }
});
```

### `MessageType`

```typescript
const MessageType = {
  DATA: 0,     // Terminal output / input
  ATTACH: 1,   // Client attach with size
  DETACH: 2,   // Client detach
  RESIZE: 3,   // Terminal resize
  EXIT: 4,     // Process exited
  SCREEN: 5,   // Screen replay
  PEEK: 6,     // Read-only peek request
  STATUS: 7,   // Stats query/response
};
```

### `TERMINAL_SANITIZE: string`

ANSI sequence that resets all terminal modes (mouse tracking, cursor visibility, alternate screen, etc.). Useful after disconnecting from a session.
