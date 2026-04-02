import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, execFileSync, type ChildProcess } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const serverModule = path.join(__dirname, "..", "dist", "server.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-stats-"));
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
  return `s${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
}

async function startDaemon(
  sessionDir: string,
  name: string,
  command: string,
  args: string[] = [],
): Promise<number> {
  const config = JSON.stringify({
    name,
    command,
    args,
    displayCommand: command,
    cwd: os.tmpdir(),
    rows: 24,
    cols: 80,
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

function runStats(sessionDir: string, ...args: string[]): string {
  return execFileSync(nodeBin, [cliPath, "stats", ...args], {
    env: { ...process.env, PTY_SESSION_DIR: sessionDir },
    encoding: "utf-8",
    timeout: 10000,
  });
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

describe("pty stats CLI", () => {
  it("prints stats for a named session", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "cat");

    const output = runStats(dir, name);

    expect(output).toContain(`Session: ${name}`);
    expect(output).toContain("Terminal:");
    expect(output).toContain("Scrollback:");
    expect(output).toContain("Clients:");
    expect(output).toContain("Process:");
    expect(output).toContain("Modes:");
    expect(output).toContain("running");
  }, 15000);

  it("returns valid JSON with --json flag", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "cat");

    const output = runStats(dir, "--json", name);
    const stats = JSON.parse(output);

    expect(stats.name).toBe(name);
    expect(stats.terminal).toBeDefined();
    expect(stats.terminal.cols).toBe(80);
    expect(stats.terminal.rows).toBe(24);
    expect(stats.terminal.scrollbackCapacity).toBe(24 + 10000);
    expect(stats.process.alive).toBe(true);
    expect(stats.clients).toBeDefined();
    expect(stats.modes).toBeDefined();
  }, 15000);

  it("queries all running sessions when no name given", async () => {
    const dir = makeSessionDir();
    const name1 = uniqueName();
    const name2 = uniqueName();
    await startDaemon(dir, name1, "cat");
    await startDaemon(dir, name2, "cat");

    const output = runStats(dir);

    expect(output).toContain(`Session: ${name1}`);
    expect(output).toContain(`Session: ${name2}`);
  }, 15000);

  it("exits with error for nonexistent session", async () => {
    const dir = makeSessionDir();

    try {
      runStats(dir, "nonexistent");
      expect.fail("should have thrown");
    } catch (err: any) {
      expect(err.status).not.toBe(0);
    }
  }, 15000);

  it("shows exited message for dead session", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "true"); // exits immediately
    await new Promise((r) => setTimeout(r, 1000)); // wait for exit

    const output = runStats(dir, name);
    expect(output).toContain("exited");
  }, 15000);
});
