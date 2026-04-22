import * as fs from "node:fs";
import * as fsp from "node:fs/promises";
import { getEventsPath, getSessionDir, ensureSessionDir } from "./sessions.ts";

export const EventType = {
  BELL: "bell",
  TITLE_CHANGE: "title_change",
  NOTIFICATION: "notification",
  FOCUS_REQUEST: "focus_request",
  CURSOR_VISIBLE: "cursor_visible",
  SESSION_START: "session_start",
  SESSION_EXIT: "session_exit",
  SESSION_EXEC: "session_exec",
  SESSION_RESTART: "session_restart",
  SESSION_FAILED: "session_failed",
  SUPERVISOR_START: "supervisor_start",
  SUPERVISOR_STOP: "supervisor_stop",
} as const;

export type EventType = (typeof EventType)[keyof typeof EventType];

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

export interface SessionRestartEvent extends EventBase {
  type: "session_restart";
  restartCount: number;
  backoffMs: number;
}

export interface SessionFailedEvent extends EventBase {
  type: "session_failed";
  restartCount: number;
  reason: string;
}

export interface SupervisorStartEvent extends EventBase {
  type: "supervisor_start";
}

export interface SupervisorStopEvent extends EventBase {
  type: "supervisor_stop";
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

export type EventRecord =
  | BellEvent
  | TitleChangeEvent
  | NotificationEvent
  | FocusRequestEvent
  | CursorVisibleEvent
  | SessionStartEvent
  | SessionExitEvent
  | SessionExecEvent
  | SessionRestartEvent
  | SessionFailedEvent
  | SupervisorStartEvent
  | SupervisorStopEvent
  | UserEvent
  | StateSetEvent
  | StateDeleteEvent;

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
    await fsp.writeFile(filePath, lines.slice(-KEEP_LINES).join("\n") + "\n");
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
    case "session_restart":
      return `${prefix} restarted (attempt ${event.restartCount}, backoff ${event.backoffMs}ms)`;
    case "session_failed":
      return `${prefix} failed — ${event.reason}`;
    case "supervisor_start":
      return `${prefix} supervisor started`;
    case "supervisor_stop":
      return `${prefix} supervisor stopped`;
    case "state.set":
      return `${prefix} state.set ${event.key} = ${JSON.stringify(event.value)}`;
    case "state.delete":
      return `${prefix} state.delete ${event.key}`;
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
