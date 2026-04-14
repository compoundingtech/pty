import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, spawnSync } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const serverModule = path.join(__dirname, "..", "dist", "server.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-peekwait-"));
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
  return `pw${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
}

async function startDaemon(
  sessionDir: string,
  name: string,
  command: string,
  args: string[] = [],
): Promise<number> {
  const config = JSON.stringify({
    name, command, args, displayCommand: command,
    cwd: os.tmpdir(), rows: 24, cols: 80,
  });
  const child = spawn(nodeBin, [serverModule], {
    detached: true,
    stdio: ["ignore", "ignore", "pipe"],
    env: { ...process.env, PTY_SERVER_CONFIG: config, PTY_SESSION_DIR: sessionDir },
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
    if (exitCode !== null) throw new Error(`Daemon exited: ${stderr}`);
    try {
      fs.statSync(socketPath);
      await new Promise((r) => setTimeout(r, 100));
      bgPids.push(child.pid!);
      return child.pid!;
    } catch {}
    await new Promise((r) => setTimeout(r, 50));
  }
  throw new Error("Timeout waiting for daemon");
}

function runCli(sessionDir: string, ...args: string[]) {
  return spawnSync(nodeBin, [cliPath, ...args], {
    env: { ...process.env, PTY_SESSION_DIR: sessionDir },
    encoding: "utf-8",
    timeout: 15000,
  });
}

afterEach(() => {
  for (const pid of bgPids) { try { process.kill(pid, "SIGTERM"); } catch {} }
  bgPids = [];
  for (const dir of sessionDirs) {
    try {
      for (const e of fs.readdirSync(dir)) { try { fs.unlinkSync(path.join(dir, e)); } catch {} }
    } catch {}
  }
  sessionDirs = [];
});

describe("pty peek --full", () => {
  it("shows full scrollback", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "sh", ["-c", "for i in $(seq 1 100); do echo line$i; done; exec cat"]);
    await new Promise((r) => setTimeout(r, 500));

    const normal = runCli(dir, "peek", "--plain", name);
    const full = runCli(dir, "peek", "--plain", "--full", name);

    const normalLines = normal.stdout.trim().split("\n").length;
    const fullLines = full.stdout.trim().split("\n").length;

    expect(fullLines).toBeGreaterThan(normalLines);
    expect(fullLines).toBeGreaterThanOrEqual(100);
    expect(full.stdout).toContain("line1");
    expect(full.stdout).toContain("line100");
  }, 15000);
});

describe("pty peek --wait", () => {
  it("waits until text appears", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    // Print "READY" after a short delay
    await startDaemon(dir, name, "sh", ["-c", "sleep 0.5; echo READY; exec cat"]);

    const result = runCli(dir, "peek", "--wait", "READY", "-t", "5", "--plain", name);

    expect(result.status).toBe(0);
    expect(result.stdout).toContain("READY");
  }, 15000);

  it("times out when text never appears", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "cat");

    const result = runCli(dir, "peek", "--wait", "NEVER", "-t", "1", "--plain", name);

    expect(result.status).toBe(1);
    expect(result.stderr).toContain("Timed out");
    expect(result.stderr).toContain("NEVER");
  }, 15000);

  it("returns immediately if text is already on screen", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "sh", ["-c", "echo ALREADY; exec cat"]);
    await new Promise((r) => setTimeout(r, 300));

    const start = Date.now();
    const result = runCli(dir, "peek", "--wait", "ALREADY", "-t", "5", "--plain", name);
    const elapsed = Date.now() - start;

    expect(result.status).toBe(0);
    expect(result.stdout).toContain("ALREADY");
    expect(elapsed).toBeLessThan(2000);
  }, 15000);
});
