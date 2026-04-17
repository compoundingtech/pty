// In-memory log store + pure filter/search over entries.
//
// Design notes
// ------------
// Backing storage is a single mutable array (`buffer`). We never allocate
// a new array on append — that would be O(n) per append and for a
// high-volume source like `find /` quickly drags the event loop to a
// halt. Instead we `.push()` (amortized O(1)) and do a bulk
// `.splice(0, …)` only when we've drifted past `scrollbackLimit * 2`,
// which amortizes ring-buffer maintenance to O(1) per append.
//
// Reactivity is a `version` signal bumped on each mutation. `entries.get`
// registers a dep on `version` so the effect-driven render loop re-runs
// when new lines arrive, without allocating a fresh array each time.
// `applyView` is a pure function over a snapshot — unit-testable without
// the store.

import { signal, stripAnsi } from "../../src/tui/index.ts";

export type LogSource = "out" | "err";

export interface LogEntry {
  /** Monotonic per-store sequence. Survives ring-buffer drops. */
  seq: number;
  /** Epoch ms when the line was captured. */
  ts: number;
  source: LogSource;
  /** Raw line, possibly containing ANSI escape sequences. No trailing \n. */
  line: string;
}

export type FilterMode = "both" | "out" | "err";

export interface ReadableEntries {
  /** Returns the current window of entries (at most `scrollbackLimit`).
   *  Registers a dep on the underlying version signal so reactive effects
   *  re-run on append. Returned array MUST NOT be mutated by callers. */
  get(): readonly LogEntry[];
}

export interface LogStore {
  readonly entries: ReadableEntries;
  append(source: LogSource, line: string, now?: number): LogEntry;
  clear(): void;
  readonly scrollbackLimit: number;
}

export function createStore(scrollbackLimit: number): LogStore {
  if (scrollbackLimit <= 0) throw new Error("scrollbackLimit must be positive");

  const buffer: LogEntry[] = [];
  const version = signal(0);
  let nextSeq = 0;
  const compactAt = scrollbackLimit * 2;

  return {
    scrollbackLimit,
    entries: {
      get(): readonly LogEntry[] {
        version.get(); // register dep so effects re-fire on mutation
        // Trim the viewer's window to the scrollback size without
        // compacting the underlying buffer on every read.
        if (buffer.length > scrollbackLimit) {
          return buffer.slice(buffer.length - scrollbackLimit);
        }
        return buffer;
      },
    },
    append(source, line, now = Date.now()) {
      const entry: LogEntry = { seq: nextSeq++, ts: now, source, line };
      buffer.push(entry);
      // Amortized O(1): only compact when we've drifted past 2x the
      // scrollback limit. Single bulk splice, not one-shift-per-append.
      if (buffer.length >= compactAt) {
        buffer.splice(0, buffer.length - scrollbackLimit);
      }
      version.set(version.peek() + 1);
      return entry;
    },
    clear() {
      buffer.length = 0;
      version.set(version.peek() + 1);
    },
  };
}

/** Apply the active filter + search to a list of entries. Pure function.
 *  Search is case-insensitive substring match against the *plain text* of
 *  the line (ANSI stripped), so users can search for literal text that
 *  might be wrapped in color codes. */
export function applyView(
  entries: readonly LogEntry[],
  filter: FilterMode,
  query: string,
): LogEntry[] {
  const q = query.trim().toLowerCase();
  const result: LogEntry[] = [];
  for (const entry of entries) {
    if (filter === "out" && entry.source !== "out") continue;
    if (filter === "err" && entry.source !== "err") continue;
    if (q) {
      const plain = stripAnsi(entry.line).toLowerCase();
      if (!plain.includes(q)) continue;
    }
    result.push(entry);
  }
  return result;
}
