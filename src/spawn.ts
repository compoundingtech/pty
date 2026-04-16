import { spawn, execFileSync } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";
import * as tty from "node:tty";
import { fileURLToPath } from "node:url";
import { getSocketPath } from "./sessions.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

/** Allow overriding the server module path (used by the bundled supervisor). */
let _serverModulePath: string | null = null;
export function setServerModulePath(p: string): void { _serverModulePath = p; }

export interface SpawnDaemonOptions {
  name: string;
  command: string;
  args: string[];
  displayCommand: string;
  cwd?: string;
  ephemeral?: boolean;
  rows?: number;
  cols?: number;
  tags?: Record<string, string>;
  /** Optional human-friendly alias for the session, stored in
   *  SessionMetadata.displayName. `name` stays the immutable id. */
  displayName?: string;
  /** When true, strip the daemon's env down to a small allow-list before
   *  spawning the session child — prevents cloud tokens / OAuth / SSH agent
   *  vars from leaking into a session that may be reached via pty-relay.
   *  See BUG-4. */
  isolateEnv?: boolean;
  /** Additional `KEY=VALUE` env entries to add on top of the isolation
   *  allow-list. Ignored unless `isolateEnv` is true. */
  extraEnv?: Record<string, string>;
  /** Use this env dict verbatim for the spawned child — no inheritance from
   *  the daemon's `process.env`, no allow-list. `PTY_SESSION` is always
   *  injected on top so nesting detection and `pty exec` keep working.
   *
   *  Mutually exclusive with `isolateEnv` / `extraEnv` — passing `env`
   *  together with either will throw at daemon startup. */
  env?: Record<string, string>;
  /** Override the runtime used to launch the detached daemon process.
   *
   *  By default the daemon is spawned with `process.execPath` — the same
   *  runtime as the caller. That breaks when the caller is running under a
   *  non-Node runtime (e.g. Bun): the daemon needs to be launched under Node
   *  so the PTY server can load its `node-pty` native addon.
   *
   *  Set `launcher` to point at a Node binary (and optional leading args) to
   *  route daemon launches through it, regardless of the caller's runtime.
   *
   *  @example
   *  ```ts
   *  await spawnDaemon({
   *    // ...existing fields...
   *    launcher: { command: "/usr/local/bin/node" },
   *  });
   *  ```
   */
  launcher?: { command: string; args?: string[] };
}

export async function spawnDaemon(options: SpawnDaemonOptions): Promise<void> {
  const stdout = process.stdout as tty.WriteStream;
  const rows = options.rows ?? stdout.rows ?? 24;
  const cols = options.cols ?? stdout.columns ?? 80;

  const serverModule = _serverModulePath ?? path.join(__dirname, "server.js");
  const config = JSON.stringify({
    name: options.name,
    command: options.command,
    args: options.args,
    displayCommand: options.displayCommand,
    cwd: options.cwd ?? process.cwd(),
    rows,
    cols,
    ephemeral: options.ephemeral ?? false,
    ...(options.tags && Object.keys(options.tags).length > 0 ? { tags: options.tags } : {}),
    ...(options.displayName ? { displayName: options.displayName } : {}),
    ...(options.isolateEnv ? { isolateEnv: true } : {}),
    ...(options.extraEnv && Object.keys(options.extraEnv).length > 0 ? { extraEnv: options.extraEnv } : {}),
    ...(options.env ? { env: options.env } : {}),
  });

  const launcherCmd = options.launcher?.command ?? process.execPath;
  const launcherArgs = options.launcher?.args ?? [];
  const child = spawn(launcherCmd, [...launcherArgs, serverModule], {
    detached: true,
    stdio: ["ignore", "ignore", "pipe"],
    env: { ...process.env, PTY_SERVER_CONFIG: config },
  });

  // Capture stderr for better error reporting
  let stderrOutput = "";
  child.stderr?.on("data", (data: Buffer) => {
    stderrOutput += data.toString();
  });

  // Detect early daemon crash before the socket appears
  let earlyExit = false;
  let earlyExitCode: number | null = null;
  child.on("exit", (code) => {
    earlyExit = true;
    earlyExitCode = code;
  });

  (child.stderr as any)?.unref?.();
  child.unref();

  try {
    await waitForSocket(options.name, 3000, () => {
      if (earlyExit) {
        const details = stderrOutput.trim();
        const msg = `Daemon process exited immediately (code ${earlyExitCode ?? "unknown"}).`;
        throw new Error(details ? `${msg}\n${details}` : `${msg} Is the command valid?`);
      }
    });
  } catch (err) {
    // Kill the orphaned daemon process so it doesn't leak
    if (!earlyExit && child.pid) {
      try { process.kill(child.pid, "SIGTERM"); } catch {}
    }
    throw err;
  }
}

export function waitForSocket(
  name: string,
  timeoutMs: number,
  earlyCheck?: () => void
): Promise<void> {
  const socketPath = getSocketPath(name);
  const start = Date.now();

  return new Promise((resolve, reject) => {
    function check(): void {
      // Check for early daemon failure
      try {
        earlyCheck?.();
      } catch (e) {
        reject(e);
        return;
      }

      if (Date.now() - start > timeoutMs) {
        reject(new Error(`Timeout waiting for session "${name}" to start`));
        return;
      }

      try {
        const stat = fs.statSync(socketPath);
        if (stat) {
          setTimeout(resolve, 100);
          return;
        }
      } catch {}

      setTimeout(check, 50);
    }
    check();
  });
}

export function resolveCommand(cmd: string): string {
  // Already absolute — just verify it exists
  if (path.isAbsolute(cmd)) {
    if (!fs.existsSync(cmd)) {
      throw new Error(`Command not found: ${cmd}`);
    }
    return cmd;
  }

  // Relative path (contains /) — resolve against cwd
  if (cmd.includes("/")) {
    const resolved = path.resolve(cmd);
    if (!fs.existsSync(resolved)) {
      throw new Error(`Command not found: ${cmd}`);
    }
    return resolved;
  }

  // Bare command name — look up in PATH
  try {
    return execFileSync("which", [cmd], { encoding: "utf8" }).trim();
  } catch {
    throw new Error(`Command not found: ${cmd}`);
  }
}
