import * as fs from "node:fs";
import * as fsp from "node:fs/promises";
import {
  getEventsPath, getSessionDir, ensureSessionDir,
  atomicWriteFileSync, atomicWriteFile,
} from "./sessions.ts";

export const EventType = {
  BELL: "bell",
  TITLE_CHANGE: "title_change",
  NOTIFICATION: "notification",
  FOCUS_REQUEST: "focus_request",
  CURSOR_VISIBLE: "cursor_visible",
  SESSION_START: "session_start",
  SESSION_EXIT: "session_exit",
  SESSION_EXEC: "session_exec",
  SESSION_RESPAWN: "session_respawn",
  SESSION_ABANDONED: "session_abandoned",
} as const;

export type EventType = (typeof EventType)[keyof typeof EventType];

// PUBLIC FORMAT — `<name>.events.jsonl` is line-delimited JSON of these
// records. Adding a new event type, renaming an existing one, or changing
// a payload field MUST be reflected in `docs/disk-layout.md` and called
// out under `### Storage format` in the next CHANGELOG entry. A smoke
// test (`tests/disk-layout-docs.test.ts`) asserts every event-type literal
// appears in the docs.
export interface EventBase {
  session: string;
  /** Type string. Known system types live in the `EventType` enum; user
   *  events are `user.*`; state bag changes are `state.set` / `state.delete`.
   *  Kept as a plain `string` here so subtype interfaces can carry their
   *  own literal types without fighting the compiler. */
  type: string;
  ts: string;
}

export interface BellEvent extends EventBase {
  type: "bell";
}

export interface TitleChangeEvent extends EventBase {
  type: "title_change";
  value: string;
}

export interface NotificationEvent extends EventBase {
  type: "notification";
  title?: string;
  body?: string;
  source?: "osc9" | "osc99" | "osc777";
}

export interface FocusRequestEvent extends EventBase {
  type: "focus_request";
}

export interface CursorVisibleEvent extends EventBase {
  type: "cursor_visible";
}

export interface SessionStartEvent extends EventBase {
  type: "session_start";
  tags?: Record<string, string>;
}

export interface SessionExitEvent extends EventBase {
  type: "session_exit";
  exitCode: number;
}

export interface SessionExecEvent extends EventBase {
  type: "session_exec";
  previousCommand: string;
  command: string;
}

/** Emitted by `pty gc` whenever it respawns a `strategy=permanent`
 *  session that's exited/vanished. Carries no payload beyond the
 *  envelope — the restart is stateless, there is no attempt counter,
 *  and the cron interval is the rate limit. */
export interface SessionRespawnEvent extends EventBase {
  type: "session_respawn";
}

/** Emitted by `pty gc` when it reaps a live `strategy=permanent` session
 *  that's been detected as abandoned. Two shapes today:
 *
 *  - `cwd-gone`: the session's recorded `cwd` no longer resolves on disk
 *    (`fs.statSync` throws `ENOENT`). Reaped by default — cwd deletion
 *    is a strong low-false-positive signal.
 *  - `idle`: the session's `lastAttachAt` is older than `idleDays`. Only
 *    triggered when `pty gc --idle-days N` was passed OR the session
 *    carries a `strategy.idle-days=N` tag; there's no on-by-default
 *    idle threshold.
 *
 *  Abandonment reaps SIGTERM the daemon (if alive), `cleanupAll` the
 *  session files, and emit this event as the final record. The event is
 *  best-effort — if the events log has already been unlinked by
 *  `cleanupAll` on the previous tick, the append silently no-ops. */
export interface SessionAbandonedEvent extends EventBase {
  type: "session_abandoned";
  reason: "cwd-gone" | "idle";
  /** For `idle`: the number of days since `lastAttachAt` (rounded down).
   *  For `cwd-gone`: absent. */
  idleDays?: number;
}

/** User-published event. `type` must begin with `user.` — the CLI
 *  (`pty emit`) rejects anything else, and the client-API `emitEvent`
 *  helper throws on bad types. Payload is free-form JSON. */
export interface UserEvent extends EventBase {
  type: `user.${string}`;
  data?: unknown;
  text?: string;
}

/** Emitted automatically whenever `setState` writes a key. Mirrors
 *  what `pty state set` records. Consumers of the event stream can
 *  react to state changes without polling the metadata file. */
export interface StateSetEvent extends EventBase {
  type: "state.set";
  key: string;
  value: unknown;
}

export interface StateDeleteEvent extends EventBase {
  type: "state.delete";
  key: string;
}

/** Emitted whenever `setDisplayName` actually changes the stored value.
 *  `previous` / `value` are `null` when absent. Skipped on no-op writes
 *  so consumers don't get spurious refresh pings. */
export interface DisplayNameChangeEvent extends EventBase {
  type: "display_name_change";
  previous: string | null;
  value: string | null;
}

/** Emitted whenever `updateTags` effectively changes the tags map.
 *  Snapshots both the previous and new full tag maps so consumers
 *  can diff without having to reason about `updates` vs `removals`. */
export interface TagsChangeEvent extends EventBase {
  type: "tags_change";
  previous: Record<string, string>;
  value: Record<string, string>;
}

export type EventRecord =
  | BellEvent
  | TitleChangeEvent
  | NotificationEvent
  | FocusRequestEvent
  | CursorVisibleEvent
  | SessionStartEvent
  | SessionExitEvent
  | SessionExecEvent
  | SessionRespawnEvent
  | SessionAbandonedEvent
  | UserEvent
  | StateSetEvent
  | StateDeleteEvent
  | DisplayNameChangeEvent
  | TagsChangeEvent;

/** Type guard: narrows an EventRecord to a UserEvent. */
export function isUserEvent(e: EventRecord): e is UserEvent {
  return typeof e.type === "string" && e.type.startsWith("user.");
}

/** Validate a user-emitted event type. Returns null if valid, an error
 *  message otherwise. Shared between the CLI and the client-API helper
 *  so both surface the same message. */
export function validateUserEventType(type: string): string | null {
  if (typeof type !== "string" || type.length === 0) {
    return "event type must be a non-empty string";
  }
  if (!type.startsWith("user.")) {
    return `custom events must start with "user." (got ${JSON.stringify(type)})`;
  }
  if (type === "user.") {
    return `event type "user." needs a suffix (e.g. "user.build-done")`;
  }
  // Reserve ASCII whitespace / control chars — they'd break JSONL round-trip
  // and any shell that tries to grep the events file.
  if (/[\s\x00-\x1f]/.test(type)) {
    return `event type may not contain whitespace or control characters`;
  }
  return null;
}

const MAX_LINES = 1000;
const KEEP_LINES = 500;
const TRUNCATE_CHECK_INTERVAL = 100;

/** One-shot helper to append a single event to a session's events log
 *  without keeping an EventWriter around. Used by CLI subcommands like
 *  `pty emit` and `pty state set` that run outside the daemon process.
 *  Applies the same MAX_LINES/KEEP_LINES retention as `EventWriter` so
 *  scripts that write in a loop don't grow the log unbounded — the
 *  truncate path is skipped via a cheap stat when the file is small. */
export async function appendEvent(name: string, event: EventRecord): Promise<void> {
  ensureSessionDir();
  const filePath = getEventsPath(name);
  const line = JSON.stringify(event) + "\n";
  await fsp.appendFile(filePath, line);
  await maybeTruncate(filePath);
}

/** Synchronous twin of `appendEvent` — lets synchronous metadata-mutation
 *  helpers (setDisplayName, updateTags, setState, deleteState) emit their
 *  change events inline without forcing their signatures to go async.
 *  Uses the same retention path with a sync stat fast-path.
 *
 *  Concurrency note: `fs.appendFileSync` issues a single `write()` with
 *  `O_APPEND`, which POSIX guarantees is atomic for payloads up to
 *  `PIPE_BUF` bytes (typically 4096 on Linux/macOS). All built-in events
 *  are well under that. If a caller passes a `user.*` event or
 *  `state.set` with a > 4KB payload, concurrent appends could interleave
 *  — keep large payloads out of the event stream and in state. */
export function appendEventSync(name: string, event: EventRecord): void {
  ensureSessionDir();
  const filePath = getEventsPath(name);
  const line = JSON.stringify(event) + "\n";
  fs.appendFileSync(filePath, line);
  maybeTruncateSync(filePath);
}

function maybeTruncateSync(filePath: string): void {
  try {
    const stat = fs.statSync(filePath);
    if (stat.size < MAX_LINES * 40) return;
    const content = fs.readFileSync(filePath, "utf-8");
    const lines = content.trimEnd().split("\n");
    if (lines.length >= MAX_LINES) {
      // Tmp+rename so readers never see a half-written rewrite. Concurrent
      // appenders after our read but before our rename will have their
      // lines lost (their append goes to the old inode that we unlink
      // via rename); that's a "concurrent writes overwrite each other"
      // case, not corruption — readers always see a valid JSONL file.
      atomicWriteFileSync(filePath, lines.slice(-KEEP_LINES).join("\n") + "\n");
    }
  } catch {
    // File might have been concurrently removed — ignore.
  }
}

/** Cheap retention check. Only reads + rewrites when the file's byte size
 *  suggests it might exceed MAX_LINES — avoids paying a readFile per
 *  append in the common case. */
async function maybeTruncate(filePath: string): Promise<void> {
  try {
    const stat = await fsp.stat(filePath);
    // Very conservative lower bound: shortest plausible JSONL event line is
    // ~40 bytes. If the file is smaller than MAX_LINES * 40, skip the line
    // count entirely. In practice most events are 100-400 bytes so this
    // fast path covers the overwhelming majority of calls.
    if (stat.size < MAX_LINES * 40) return;
    await truncate(filePath);
  } catch {
    // File might have been concurrently removed — ignore.
  }
}

/** Validate + append a user.* event. Throws on an invalid type so the
 *  caller surfaces the error cleanly. Timestamped here so callers don't
 *  need to remember to set `ts`. */
export async function emitUserEvent(
  sessionName: string,
  type: string,
  opts: { data?: unknown; text?: string } = {},
): Promise<UserEvent> {
  const err = validateUserEventType(type);
  if (err) throw new Error(err);
  const event: UserEvent = {
    session: sessionName,
    type: type as `user.${string}`,
    ts: new Date().toISOString(),
    ...(opts.data !== undefined ? { data: opts.data } : {}),
    ...(opts.text !== undefined ? { text: opts.text } : {}),
  };
  await appendEvent(sessionName, event);
  return event;
}

/** Manages async, serialized writes to a session's events JSONL file. */
export class EventWriter {
  private chain: Promise<void> = Promise.resolve();
  private appendCount = 0;
  private name: string;

  constructor(name: string) {
    this.name = name;
  }

  /** Queue an event for writing. Returns immediately; I/O happens async. */
  append(event: EventRecord): void {
    this.chain = this.chain
      .then(() => {
        const line = JSON.stringify(event) + "\n";
        return fsp.appendFile(getEventsPath(this.name), line);
      })
      .then(() => {
        this.appendCount++;
        if (this.appendCount >= TRUNCATE_CHECK_INTERVAL) {
          this.appendCount = 0;
          return truncate(getEventsPath(this.name));
        }
      })
      .catch(() => {});
  }

  /** Wait for all pending writes to complete. */
  flush(): Promise<void> {
    return this.chain;
  }
}

async function truncate(filePath: string): Promise<void> {
  const content = await fsp.readFile(filePath, "utf-8");
  const lines = content.trimEnd().split("\n");
  if (lines.length >= MAX_LINES) {
    // Tmp+rename so readers never see a half-written rewrite — same
    // reasoning as `maybeTruncateSync`.
    await atomicWriteFile(filePath, lines.slice(-KEEP_LINES).join("\n") + "\n");
  }
}

export function clearEvents(name: string): void {
  ensureSessionDir();
  try {
    fs.writeFileSync(getEventsPath(name), "");
  } catch {}
}

export function removeEvents(name: string): void {
  try {
    fs.unlinkSync(getEventsPath(name));
  } catch {}
}

export function readRecentEvents(name: string, count = 50): EventRecord[] {
  try {
    const content = fs.readFileSync(getEventsPath(name), "utf-8");
    const lines = content.trimEnd().split("\n").filter((l) => l.length > 0);
    return lines.slice(-count).map((l) => JSON.parse(l));
  } catch {
    return [];
  }
}

export interface FollowerOptions {
  names?: string[];
  onEvent: (event: EventRecord) => void;
}

export class EventFollower {
  private watchers = new Map<
    string,
    { watcher: fs.FSWatcher; offset: number }
  >();
  private dirWatcher: fs.FSWatcher | null = null;
  private options: FollowerOptions;

  constructor(options: FollowerOptions) {
    this.options = options;
  }

  start(): void {
    if (this.options.names) {
      for (const name of this.options.names) {
        this.watchFile(name, { fromStart: false });
      }
    } else {
      this.scanAndWatchAll();
    }
  }

  stop(): void {
    for (const { watcher } of this.watchers.values()) {
      watcher.close();
    }
    this.watchers.clear();
    this.dirWatcher?.close();
    this.dirWatcher = null;
  }

  private watchFile(name: string, opts: { fromStart: boolean }): void {
    const filePath = getEventsPath(name);

    // Pre-existing files: start at current EOF (don't replay history).
    // Freshly-created files detected by the dirWatcher: start at offset 0 so
    // the session_start line, which is almost always already in the file by
    // the time the directory event fires, isn't skipped.
    let offset = 0;
    if (!opts.fromStart) {
      try {
        offset = fs.statSync(filePath).size;
      } catch {}
    }

    try {
      const watcher = fs.watch(filePath, () => {
        this.readNewLines(name, filePath);
      });
      this.watchers.set(name, { watcher, offset });
      // Seed: if the file already has content we want to replay, do it now.
      if (opts.fromStart) {
        this.readNewLines(name, filePath);
      }
    } catch {}
  }

  private readNewLines(name: string, filePath: string): void {
    const entry = this.watchers.get(name);
    if (!entry) return;

    try {
      const stat = fs.statSync(filePath);
      if (stat.size < entry.offset) {
        // File was truncated — reset to beginning
        entry.offset = 0;
      }
      if (stat.size === entry.offset) return;

      const fd = fs.openSync(filePath, "r");
      const buf = Buffer.alloc(stat.size - entry.offset);
      fs.readSync(fd, buf, 0, buf.length, entry.offset);
      fs.closeSync(fd);
      entry.offset = stat.size;

      const chunk = buf.toString("utf-8");
      const lines = chunk.split("\n").filter((l) => l.length > 0);
      for (const line of lines) {
        try {
          const event = JSON.parse(line) as EventRecord;
          this.options.onEvent(event);
        } catch {}
      }
    } catch {}
  }

  private scanAndWatchAll(): void {
    const dir = getSessionDir();

    // Watch existing .events.jsonl files — they've been running, so start at
    // EOF rather than replaying their history.
    try {
      for (const entry of fs.readdirSync(dir)) {
        if (entry.endsWith(".events.jsonl")) {
          const name = entry.replace(/\.events\.jsonl$/, "");
          this.watchFile(name, { fromStart: false });
        }
      }
    } catch {}

    // Watch directory for new .events.jsonl files. For a brand-new file the
    // session_start line is almost always already present by the time the
    // directory-change event fires, so start at offset 0 to include it.
    try {
      this.dirWatcher = fs.watch(dir, (_eventType, filename) => {
        if (
          filename &&
          filename.endsWith(".events.jsonl") &&
          !this.watchers.has(filename.replace(/\.events\.jsonl$/, ""))
        ) {
          const name = filename.replace(/\.events\.jsonl$/, "");
          this.watchFile(name, { fromStart: true });
        }
      });
    } catch {}
  }
}

export function formatEvent(event: EventRecord): string {
  const time = new Date(event.ts).toLocaleTimeString("en-US", {
    hour12: false,
  });
  const prefix = `[${time}] ${event.session}:`;

  switch (event.type) {
    case "bell":
      return `${prefix} bell`;
    case "title_change":
      return `${prefix} title -> "${event.value}"`;
    case "notification": {
      const parts = [prefix, "notification"];
      if (event.title) parts.push(`-- "${event.title}"`);
      if (event.body) parts.push(event.body);
      return parts.join(" ");
    }
    case "focus_request":
      return `${prefix} focus requested`;
    case "cursor_visible":
      return `${prefix} cursor restored`;
    case "session_start": {
      const tagStr = event.tags ? " " + Object.entries(event.tags).map(([k, v]) => `${k}=${v}`).join(" ") : "";
      return `${prefix} started${tagStr}`;
    }
    case "session_exit":
      return `${prefix} exited (code ${event.exitCode})`;
    case "session_exec":
      return `${prefix} exec ${event.command} (was ${event.previousCommand})`;
    case "session_respawn":
      return `${prefix} respawned`;
    case "session_abandoned":
      return event.reason === "idle" && event.idleDays !== undefined
        ? `${prefix} abandoned (idle ${event.idleDays}d)`
        : `${prefix} abandoned (${event.reason})`;
    case "state.set":
      return `${prefix} state.set ${event.key} = ${JSON.stringify(event.value)}`;
    case "state.delete":
      return `${prefix} state.delete ${event.key}`;
    case "display_name_change":
      return `${prefix} display_name -> ${JSON.stringify(event.value)} (was ${JSON.stringify(event.previous)})`;
    case "tags_change": {
      const fmt = (t: Record<string, string>) =>
        Object.keys(t).length === 0 ? "{}" : Object.entries(t).map(([k, v]) => `${k}=${v}`).join(" ");
      return `${prefix} tags -> ${fmt(event.value)} (was ${fmt(event.previous)})`;
    }
    default: {
      // user.* events + anything else unknown-at-compile-time.
      const e = event as EventBase & { data?: unknown; text?: string };
      const suffix =
        e.text != null ? ` "${e.text}"`
        : e.data !== undefined ? ` ${JSON.stringify(e.data)}`
        : "";
      return `${prefix} ${e.type}${suffix}`;
    }
  }
}
