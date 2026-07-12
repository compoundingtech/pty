import { describe, it, expect, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const serverModule = path.join(__dirname, "..", "dist", "server.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-backstop-"));
const spawnedPids: number[] = [];
afterAll(() => {
  for (const pid of spawnedPids) { try { process.kill(pid, "SIGKILL"); } catch {} }
  fs.rmSync(testRoot, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

function isAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    return (err as NodeJS.ErrnoException).code === "EPERM";
  }
}

async function waitUntilDead(pid: number, timeoutMs: number): Promise<boolean> {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    if (!isAlive(pid)) return true;
    await new Promise((r) => setTimeout(r, 50));
  }
  return false;
}

function waitForFile(p: string, timeoutMs: number): Promise<void> {
  const start = Date.now();
  return new Promise((resolve, reject) => {
    const tick = () => {
      try { fs.statSync(p); resolve(); return; } catch {}
      if (Date.now() - start > timeoutMs) { reject(new Error(`Timeout waiting for ${p}`)); return; }
      setTimeout(tick, 50);
    };
    tick();
  });
}

/** Spawn a daemon directly (the real server.js entry point), running `command`
 *  with `args`, and with the given extra env (e.g. PTY_SHUTDOWN_DEADLINE_MS).
 *  Returns the daemon PID once its socket exists. */
async function startDaemon(
  sessionDir: string,
  name: string,
  command: string,
  args: string[],
  extraEnv: Record<string, string> = {},
): Promise<number> {
  const config = JSON.stringify({
    name, command, args, displayCommand: `${command} ${args.join(" ")}`,
    cwd: os.tmpdir(), rows: 24, cols: 80,
  });
  const child = spawn(nodeBin, [serverModule], {
    detached: true,
    stdio: ["ignore", "ignore", "ignore"],
    env: {
      ...(process.env as Record<string, string>),
      PTY_SERVER_CONFIG: config,
      PTY_SESSION_DIR: sessionDir,
      ...extraEnv,
    },
  });
  child.unref();
  spawnedPids.push(child.pid!);
  await waitForFile(path.join(sessionDir, `${name}.sock`), 5000);
  return child.pid!;
}

describe("daemon shutdown-hang backstop", () => {
  it("force-exits and reaps a frozen child when graceful shutdown exceeds the deadline", async () => {
    const sessionDir = fs.mkdtempSync(path.join(testRoot, "d-"));
    const name = `bs-${Math.random().toString(36).slice(2, 6)}`;
    const childPidFile = path.join(sessionDir, "child.pid");

    // A child that traps SIGHUP: close()'s graceful `ptyProcess.kill()` (SIGHUP)
    // is ignored, so childExited never resolves and the graceful shutdown drags.
    // Only the backstop's SIGKILL can reap it — which is exactly what we assert.
    const script = `echo $$ > "${childPidFile}"; trap "" HUP; while true; do sleep 1; done`;
    const daemonPid = await startDaemon(
      sessionDir, name, "sh", ["-c", script],
      { PTY_SHUTDOWN_DEADLINE_MS: "300" },
    );

    await waitForFile(childPidFile, 5000);
    const childPid = Number(fs.readFileSync(childPidFile, "utf8").trim());
    spawnedPids.push(childPid);
    expect(isAlive(childPid)).toBe(true);

    // Initiate shutdown. The graceful path can't complete (frozen child), so the
    // 300ms backstop must fire: force-exit the daemon AND SIGKILL the child.
    process.kill(daemonPid, "SIGTERM");

    // Daemon self-exits despite the wedged close() (no kill -9 needed).
    expect(await waitUntilDead(daemonPid, 4000)).toBe(true);
    // The frozen child is reaped, not left orphaned to init. This is the
    // timing-independent proof: without the backstop the child only ever
    // receives a (trapped) SIGHUP and would survive.
    expect(await waitUntilDead(childPid, 4000)).toBe(true);
  }, 15000);

  it("does not disturb a normal, prompt shutdown", async () => {
    const sessionDir = fs.mkdtempSync(path.join(testRoot, "d-"));
    const name = `bs-ok-${Math.random().toString(36).slice(2, 6)}`;

    // Plain child that exits on the graceful SIGHUP — the default (5s) deadline
    // never comes near firing; graceful close() completes and exits promptly.
    const daemonPid = await startDaemon(sessionDir, name, "sleep", ["3600"]);
    expect(isAlive(daemonPid)).toBe(true);

    process.kill(daemonPid, "SIGTERM");
    expect(await waitUntilDead(daemonPid, 3000)).toBe(true);
  }, 10000);
});
