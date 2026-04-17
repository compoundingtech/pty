// Spawn a child process with piped stdout/stderr (NOT a PTY), line-buffer
// each stream, and emit tagged `onLine` callbacks.
//
// We explicitly avoid node-pty here: the whole point of loggy is to keep
// stdout and stderr distinguishable, and a PTY merges them at the kernel
// level. The tradeoff is that the child process doesn't get a terminal —
// TUI apps like vim/htop won't work, but line-oriented logs (CI, builds,
// servers) are exactly right.

import { spawn, type ChildProcess } from "node:child_process";
import type { LogSource } from "./store.ts";

export interface SpawnChildOptions {
  forceColor: boolean;
  onLine: (source: LogSource, line: string) => void;
  onExit: (exitCode: number | null, signal: NodeJS.Signals | null) => void;
  /** Called when spawn itself fails (ENOENT etc.). */
  onError?: (err: Error) => void;
}

export interface ChildHandle {
  readonly pid: number | undefined;
  /** Ask the child to exit. SIGTERM first; after `graceMs` if still alive,
   *  SIGKILL. No-op if the child has already exited. */
  stop(graceMs?: number): Promise<void>;
  /** True once the child's `exit` event has fired. */
  readonly exited: boolean;
}

export function spawnChild(
  command: string,
  args: readonly string[],
  opts: SpawnChildOptions,
): ChildHandle {
  const env: Record<string, string> = { ...(process.env as Record<string, string>) };
  if (opts.forceColor && !env.FORCE_COLOR) env.FORCE_COLOR = "1";

  const child: ChildProcess = spawn(command, args as string[], {
    stdio: ["ignore", "pipe", "pipe"],
    env,
  });

  let exited = false;
  child.on("exit", (code, signal) => {
    exited = true;
    // Flush any partial buffered line on exit so we don't drop the last
    // piece of output that didn't end in \n.
    flushRemainder("out");
    flushRemainder("err");
    opts.onExit(code, signal);
  });

  if (opts.onError) {
    child.on("error", opts.onError);
  }

  const remainders: Record<LogSource, string> = { out: "", err: "" };

  function feed(source: LogSource, chunk: Buffer | string) {
    const text = remainders[source] + (typeof chunk === "string" ? chunk : chunk.toString("utf-8"));
    const lines = text.split("\n");
    // The last element is the partial line after the final \n (or the whole
    // thing if there was no \n at all).
    remainders[source] = lines.pop() ?? "";
    for (const line of lines) {
      opts.onLine(source, line);
    }
  }

  function flushRemainder(source: LogSource) {
    const partial = remainders[source];
    if (partial.length > 0) {
      remainders[source] = "";
      opts.onLine(source, partial);
    }
  }

  child.stdout?.on("data", (chunk) => feed("out", chunk));
  child.stderr?.on("data", (chunk) => feed("err", chunk));
  child.stdout?.on("end", () => flushRemainder("out"));
  child.stderr?.on("end", () => flushRemainder("err"));

  return {
    get pid() { return child.pid; },
    get exited() { return exited; },
    stop(graceMs = 2000) {
      return new Promise<void>((resolve) => {
        if (exited) { resolve(); return; }

        try { child.kill("SIGTERM"); } catch {}

        const killTimer = setTimeout(() => {
          if (!exited) {
            try { child.kill("SIGKILL"); } catch {}
          }
        }, graceMs);

        const onExit = () => {
          clearTimeout(killTimer);
          resolve();
        };
        if (exited) { clearTimeout(killTimer); resolve(); }
        else child.once("exit", onExit);
      });
    },
  };
}
