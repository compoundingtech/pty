// In-memory log store + pure filter/search over entries.
//
// Design
// ------
// Backing storage is a single mutable array (`buffer`). Append pushes
// onto it (amortized O(1)) and triggers a DEBOUNCED change notification
// via `debouncedSignal` — writers mark the store dirty as often as they
// like; subscribers only hear about it once per `setImmediate` tick.
// That keeps a firehose child from saturating the reactive graph and
// gives the TUI a predictable at-most-one-render-per-tick cadence
// without the render layer needing to know anything about throttling.
//
// `applyView` is a pure function over an array snapshot so it's easy
// to unit-test.

import { debouncedSignal, stripAnsi, type DebouncedSignal } from "../../src/tui/index.ts";

export type LogSource = "out" | "err";

export interface LogEntry {
  /** Monotonic per-store sequence. Survives ring-buffer drops. */
  seq: number;
  /** Epoch ms when the line was captured. */
  ts: number;
  source: LogSource;
  /** Raw line, possibly containing ANSI escape sequences. No trailing \n. */
  line: string;
  /** Lazy cache of `stripAnsi(line).toLowerCase()` — materialized on
   *  first search to avoid redoing the work on every render. */
  _plainLower?: string;
}

export type FilterMode = "both" | "out" | "err";

export interface ReadableEntries {
  /** Returns the current window of entries (at most `scrollbackLimit`).
   *  Registers a dep on the underlying debounced signal so reactive
   *  effects re-run on append (coalesced to once per tick). Returned
   *  array MUST NOT be mutated by callers. */
  get(): readonly LogEntry[];
  /** Same as `get()` but without subscribing. Safe outside effects. */
  peek(): readonly LogEntry[];
}

export interface LogStore {
  readonly entries: ReadableEntries;
  append(source: LogSource, line: string, now?: number): LogEntry;
  clear(): void;
  /** Force the debounced change notification to fire synchronously.
   *  Useful in tests that want to observe post-append state without
   *  waiting a tick. */
  flush(): void;
  readonly scrollbackLimit: number;
}

export function createStore(scrollbackLimit: number): LogStore {
  if (scrollbackLimit <= 0) throw new Error("scrollbackLimit must be positive");

  const buffer: LogEntry[] = [];
  const version: DebouncedSignal = debouncedSignal();
  let nextSeq = 0;
  const compactAt = scrollbackLimit * 2;

  function snapshot(): readonly LogEntry[] {
    if (buffer.length > scrollbackLimit) {
      return buffer.slice(buffer.length - scrollbackLimit);
    }
    return buffer;
  }

  return {
    scrollbackLimit,
    entries: {
      get(): readonly LogEntry[] {
        version.get(); // subscribe (coalesced) — triggers at-most-once-per-tick
        return snapshot();
      },
      peek(): readonly LogEntry[] {
        return snapshot();
      },
    },
    append(source, line, now = Date.now()) {
      const entry: LogEntry = { seq: nextSeq++, ts: now, source, line };
      buffer.push(entry);
      if (buffer.length >= compactAt) {
        buffer.splice(0, buffer.length - scrollbackLimit);
      }
      version.bump();
      return entry;
    },
    clear() {
      buffer.length = 0;
      version.bump();
    },
    flush() {
      version.flush();
    },
  };
}

/** Apply the active filter + search to a list of entries. Pure function.
 *  Search is case-insensitive substring match against the *plain text*
 *  of the line (ANSI stripped, lowercased), cached on the entry so the
 *  expensive regex doesn't re-run per render. */
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
      // Cache the plain-text form; stripAnsi + toLowerCase is the hot
      // path for searches over a large scrollback.
      let plain = entry._plainLower;
      if (plain === undefined) {
        plain = stripAnsi(entry.line).toLowerCase();
        entry._plainLower = plain;
      }
      if (!plain.includes(q)) continue;
    }
    result.push(entry);
  }
  return result;
}
