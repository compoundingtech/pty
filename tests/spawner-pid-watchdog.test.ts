import { describe, it, expect, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, spawnSync } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const serverModule = path.join(__dirname, "..", "dist", "server.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-watchdog-"));
afterAll(() => {
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
    await new Promise((r) => setTimeout(r, 100));
  }
  return false;
}

function waitForFile(p: string, timeoutMs: number): Promise<void> {
  const start = Date.now();
  return new Promise((resolve, reject) => {
    const tick = () => {
      try {
        fs.statSync(p);
        resolve();
        return;
      } catch {}
      if (Date.now() - start > timeoutMs) {
        reject(new Error(`Timeout waiting for ${p}`));
        return;
      }
      setTimeout(tick, 50);
    };
    tick();
  });
}

/**
 * Start a daemon directly (no spawner), with an explicit PTY_SPAWNER_PID
 * env var pointing at `spawnerPid`. Returns the daemon PID.
 */
async function startDaemonWithSpawnerPid(
  sessionDir: string,
  name: string,
  spawnerPid: number | undefined,
): Promise<number> {
  const config = JSON.stringify({
    name,
    command: "sleep",
    args: ["3600"],
    displayCommand: "sleep 3600",
    cwd: os.tmpdir(),
    rows: 24,
    cols: 80,
  });

  const env: Record<string, string> = {
    ...(process.env as Record<string, string>),
    PTY_SERVER_CONFIG: config,
    PTY_SESSION_DIR: sessionDir,
  };
  if (spawnerPid !== undefined) env.PTY_SPAWNER_PID = String(spawnerPid);
  else delete env.PTY_SPAWNER_PID;

  const child = spawn(nodeBin, [serverModule], {
    detached: true,
    stdio: ["ignore", "ignore", "pipe"],
    env,
  });
  child.unref();

  await waitForFile(path.join(sessionDir, `${name}.sock`), 5000);
  return child.pid!;
}

describe("PTY_SPAWNER_PID watchdog", () => {
  it("shuts down the daemon when the spawner PID dies", async () => {
    // Stand-in spawner: an idle node process we control. We start the
    // daemon while it's alive, then kill the spawner and observe shutdown.
    const spawner = spawn(nodeBin, ["-e", "setInterval(() => {}, 1_000_000)"], {
      stdio: "ignore",
    });
    const spawnerPid = spawner.pid!;
    const sessionDir = fs.mkdtempSync(path.join(testRoot, "d-"));
    const name = `wd-${Math.random().toString(36).slice(2, 6)}`;

    const daemonPid = await startDaemonWithSpawnerPid(sessionDir, name, spawnerPid);
    expect(isAlive(daemonPid)).toBe(true);

    // Kill the spawner and wait for it to fully exit.
    process.kill(spawnerPid, "SIGKILL");
    await new Promise<void>((resolve) => spawner.on("exit", () => resolve()));
    expect(isAlive(spawnerPid)).toBe(false);

    // Watchdog polls every 5s; allow up to ~10s for the daemon to notice.
    const died = await waitUntilDead(daemonPid, 12_000);
    if (!died) {
      try { process.kill(daemonPid, "SIGTERM"); } catch {}
    }
    expect(died).toBe(true);
  }, 20_000);

  it("exits immediately if the spawner PID is already dead at startup", async () => {
    // Spawn-and-reap a child to obtain a pid that's guaranteed dead.
    const dead = spawnSync(nodeBin, ["-e", ""], { stdio: "ignore" });
    const deadPid = dead.pid!;
    expect(isAlive(deadPid)).toBe(false);

    const sessionDir = fs.mkdtempSync(path.join(testRoot, "d-"));
    const name = `wd-dead-${Math.random().toString(36).slice(2, 6)}`;

    // Don't waitForFile here — the daemon may exit before the socket appears.
    const config = JSON.stringify({
      name,
      command: "sleep",
      args: ["3600"],
      displayCommand: "sleep 3600",
      cwd: os.tmpdir(),
      rows: 24,
      cols: 80,
    });
    const child = spawn(nodeBin, [serverModule], {
      detached: true,
      stdio: ["ignore", "ignore", "pipe"],
      env: {
        ...(process.env as Record<string, string>),
        PTY_SERVER_CONFIG: config,
        PTY_SESSION_DIR: sessionDir,
        PTY_SPAWNER_PID: String(deadPid),
      },
    });
    child.unref();

    const died = await waitUntilDead(child.pid!, 8_000);
    if (!died) {
      try { process.kill(child.pid!, "SIGTERM"); } catch {}
    }
    expect(died).toBe(true);
  }, 15_000);

  it("ignores invalid PTY_SPAWNER_PID values (no behaviour change)", async () => {
    const sessionDir = fs.mkdtempSync(path.join(testRoot, "d-"));
    const name = `wd-bad-${Math.random().toString(36).slice(2, 6)}`;

    const config = JSON.stringify({
      name,
      command: "sleep",
      args: ["3600"],
      displayCommand: "sleep 3600",
      cwd: os.tmpdir(),
      rows: 24,
      cols: 80,
    });
    const child = spawn(nodeBin, [serverModule], {
      detached: true,
      stdio: ["ignore", "ignore", "pipe"],
      env: {
        ...(process.env as Record<string, string>),
        PTY_SERVER_CONFIG: config,
        PTY_SESSION_DIR: sessionDir,
        PTY_SPAWNER_PID: "not-a-pid",
      },
    });
    child.unref();

    try {
      await waitForFile(path.join(sessionDir, `${name}.sock`), 5000);
      expect(isAlive(child.pid!)).toBe(true);
      // Daemon should still be alive a moment later — invalid PID disables
      // the watchdog rather than shutting down.
      await new Promise((r) => setTimeout(r, 500));
      expect(isAlive(child.pid!)).toBe(true);
    } finally {
      try { process.kill(child.pid!, "SIGTERM"); } catch {}
      await waitUntilDead(child.pid!, 3000);
    }
  }, 10_000);
});
