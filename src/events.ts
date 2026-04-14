import * as fs from "node:fs";
import * as fsp from "node:fs/promises";
import { getEventsPath, getSessionDir, ensureSessionDir } from "./sessions.ts";

export const EventType = {
  BELL: "bell",
  TITLE_CHANGE: "title_change",
  NOTIFICATION: "notification",
  FOCUS_REQUEST: "focus_request",
  CURSOR_VISIBLE: "cursor_visible",
  SESSION_RESTART: "session_restart",
  SESSION_FAILED: "session_failed",
  SUPERVISOR_START: "supervisor_start",
  SUPERVISOR_STOP: "supervisor_stop",
} as const;

export type EventType = (typeof EventType)[keyof typeof EventType];

export interface EventBase {
  session: string;
  type: EventType;
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

export type EventRecord =
  | BellEvent
  | TitleChangeEvent
  | NotificationEvent
  | FocusRequestEvent
  | CursorVisibleEvent
  | SessionRestartEvent
  | SessionFailedEvent
  | SupervisorStartEvent
  | SupervisorStopEvent;

const MAX_LINES = 1000;
const KEEP_LINES = 500;
const TRUNCATE_CHECK_INTERVAL = 100;

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
        this.watchFile(name);
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

  private watchFile(name: string): void {
    const filePath = getEventsPath(name);

    // Start at the end of the current file
    let offset = 0;
    try {
      offset = fs.statSync(filePath).size;
    } catch {}

    try {
      const watcher = fs.watch(filePath, () => {
        this.readNewLines(name, filePath);
      });
      this.watchers.set(name, { watcher, offset });
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

    // Watch existing .events.jsonl files
    try {
      for (const entry of fs.readdirSync(dir)) {
        if (entry.endsWith(".events.jsonl")) {
          const name = entry.replace(/\.events\.jsonl$/, "");
          this.watchFile(name);
        }
      }
    } catch {}

    // Watch directory for new .events.jsonl files
    try {
      this.dirWatcher = fs.watch(dir, (_eventType, filename) => {
        if (
          filename &&
          filename.endsWith(".events.jsonl") &&
          !this.watchers.has(filename.replace(/\.events\.jsonl$/, ""))
        ) {
          const name = filename.replace(/\.events\.jsonl$/, "");
          this.watchFile(name);
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
    case "session_restart":
      return `${prefix} restarted (attempt ${event.restartCount}, backoff ${event.backoffMs}ms)`;
    case "session_failed":
      return `${prefix} failed — ${event.reason}`;
    case "supervisor_start":
      return `${prefix} supervisor started`;
    case "supervisor_stop":
      return `${prefix} supervisor stopped`;
  }
}
