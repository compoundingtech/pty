import { spawn, execFileSync } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";
import * as tty from "node:tty";
import { fileURLToPath } from "node:url";
import { getSocketPath } from "./sessions.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

export async function spawnDaemon(
  name: string,
  command: string,
  args: string[],
  displayCommand: string,
  cwd?: string
): Promise<void> {
  const stdout = process.stdout as tty.WriteStream;
  const rows = stdout.rows ?? 24;
  const cols = stdout.columns ?? 80;

  const serverModule = path.join(__dirname, "server.js");
  const config = JSON.stringify({
    name,
    command,
    args,
    displayCommand,
    cwd: cwd ?? process.cwd(),
    rows,
    cols,
  });

  const child = spawn(process.execPath, [serverModule], {
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

  await waitForSocket(name, 3000, () => {
    if (earlyExit) {
      const details = stderrOutput.trim();
      const msg = `Daemon process exited immediately (code ${earlyExitCode ?? "unknown"}).`;
      throw new Error(details ? `${msg}\n${details}` : `${msg} Is the command valid?`);
    }
  });
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
