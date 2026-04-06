import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, spawnSync } from "node:child_process";
import { queryStats } from "../src/client.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const serverModule = path.join(__dirname, "..", "dist", "server.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-spawn-opts-"));
afterAll(() => {
  fs.rmSync(testRoot, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

let bgPids: number[] = [];
let sessionDirs: string[] = [];

function makeSessionDir(): string {
  const dir = fs.mkdtempSync(path.join(testRoot, "d-"));
  sessionDirs.push(dir);
  return dir;
}

let nameCounter = 0;
function uniqueName(): string {
  return `spawn-opt${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
}

async function startDaemonWithSize(
  sessionDir: string,
  name: string,
  command: string,
  args: string[] = [],
  rows = 24,
  cols = 80,
): Promise<number> {
  const config = JSON.stringify({
    name,
    command,
    args,
    displayCommand: command,
    cwd: os.tmpdir(),
    rows,
    cols,
  });

  const child = spawn(nodeBin, [serverModule], {
    detached: true,
    stdio: ["ignore", "ignore", "pipe"],
    env: {
      ...process.env,
      PTY_SERVER_CONFIG: config,
      PTY_SESSION_DIR: sessionDir,
    },
  });

  let stderr = "";
  child.stderr?.on("data", (d: Buffer) => { stderr += d.toString(); });
  let exitCode: number | null = null;
  child.on("exit", (code) => { exitCode = code; });
  (child.stderr as any)?.unref?.();
  child.unref();

  const socketPath = path.join(sessionDir, `${name}.sock`);
  const start = Date.now();
  while (Date.now() - start < 5000) {
    if (exitCode !== null) {
      throw new Error(`Daemon exited with code ${exitCode}. stderr:\n${stderr}`);
    }
    try {
      fs.statSync(socketPath);
      await new Promise((r) => setTimeout(r, 100));
      bgPids.push(child.pid!);
      return child.pid!;
    } catch {}
    await new Promise((r) => setTimeout(r, 50));
  }
  throw new Error(`Timeout waiting for daemon socket: ${socketPath}`);
}

afterEach(() => {
  for (const pid of bgPids) {
    try { process.kill(pid, "SIGTERM"); } catch {}
  }
  bgPids = [];
  for (const dir of sessionDirs) {
    try {
      const entries = fs.readdirSync(dir);
      for (const e of entries) {
        try { fs.unlinkSync(path.join(dir, e)); } catch {}
      }
    } catch {}
  }
  sessionDirs = [];
});

describe("spawnDaemon options", () => {
  it("CLI passes through to spawnDaemon with options object", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();

    // Use CLI to spawn (exercises spawnDaemon with options object internally)
    const result = spawnSync(nodeBin, [cliPath, "run", "-d", "--name", name, "--", "cat"], {
      env: { ...process.env, PTY_SESSION_DIR: dir },
      encoding: "utf-8",
      timeout: 10000,
    });

    expect(result.status).toBe(0);

    // Verify session is running
    process.env.PTY_SESSION_DIR = dir;
    const stats = await queryStats(name);
    expect(stats.name).toBe(name);
    expect(stats.process.alive).toBe(true);

    // Clean up
    const pidFile = path.join(dir, `${name}.pid`);
    const pid = parseInt(fs.readFileSync(pidFile, "utf-8").trim(), 10);
    bgPids.push(pid);
  }, 15000);

  it("custom rows and cols are applied via server config", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();

    // Start daemon directly with custom dimensions
    await startDaemonWithSize(dir, name, "cat", [], 40, 120);

    process.env.PTY_SESSION_DIR = dir;
    const stats = await queryStats(name);
    expect(stats.terminal.rows).toBe(40);
    expect(stats.terminal.cols).toBe(120);
  }, 15000);

  it("default rows and cols are reasonable", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();

    // Start daemon with default dimensions
    await startDaemonWithSize(dir, name, "cat");

    process.env.PTY_SESSION_DIR = dir;
    const stats = await queryStats(name);
    expect(stats.terminal.rows).toBe(24);
    expect(stats.terminal.cols).toBe(80);
  }, 15000);
});
