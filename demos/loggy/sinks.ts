// Optional file writers for --out, --err, --log.
//
// Each configured path opens an append-only write stream at startup. Writes
// are fire-and-forget (queued by Node's stream buffer); `close()` flushes
// and closes them. No file rotation — user rotates externally if needed.

import * as fs from "node:fs";
import type { LogSource } from "./store.ts";

export interface SinksConfig {
  /** Tee raw stdout bytes to this path (as-received). */
  out: string | null;
  /** Tee raw stderr bytes to this path (as-received). */
  err: string | null;
  /** Tee both streams to this path, tagged + timestamped plain-text. */
  log: string | null;
}

export interface Sinks {
  /** Write a line to the appropriate sinks. `line` must not include a
   *  trailing newline; one is added here so callers don't double-add. */
  write(source: LogSource, line: string, ts?: number): void;
  close(): Promise<void>;
}

export function createSinks(config: SinksConfig): Sinks {
  const outStream = config.out ? fs.createWriteStream(config.out, { flags: "a" }) : null;
  const errStream = config.err ? fs.createWriteStream(config.err, { flags: "a" }) : null;
  const logStream = config.log ? fs.createWriteStream(config.log, { flags: "a" }) : null;

  return {
    write(source, line, ts = Date.now()) {
      if (source === "out" && outStream) outStream.write(line + "\n");
      if (source === "err" && errStream) errStream.write(line + "\n");
      if (logStream) logStream.write(formatTaggedLine(ts, source, line) + "\n");
    },
    close() {
      return new Promise<void>((resolve) => {
        let pending = 0;
        const done = () => { if (--pending <= 0) resolve(); };
        for (const s of [outStream, errStream, logStream]) {
          if (!s) continue;
          pending++;
          s.end(done);
        }
        if (pending === 0) resolve();
      });
    },
  };
}

/** Format one line for the combined `--log` sink:
 *  `[HH:MM:SS.mmm out] the line` or `[... err] the line`.
 *  Exported for unit testing. */
export function formatTaggedLine(ts: number, source: LogSource, line: string): string {
  const d = new Date(ts);
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  const ss = String(d.getSeconds()).padStart(2, "0");
  const ms = String(d.getMilliseconds()).padStart(3, "0");
  return `[${hh}:${mm}:${ss}.${ms} ${source}] ${line}`;
}
