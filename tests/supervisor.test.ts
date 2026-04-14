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
const supervisorModule = path.join(__dirname, "..", "dist", "supervisor.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-sup-"));
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
  return `sup${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
}

async function startDaemon(
  sessionDir: string,
  name: string,
  command: string,
  args: string[] = [],
  tags?: Record<string, string>,
): Promise<number> {
  const config = JSON.stringify({
    name, command, args, displayCommand: command,
    cwd: os.tmpdir(), rows: 24, cols: 80, tags,
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

function startSupervisor(sessionDir: string): number {
  const child = spawn(nodeBin, [cliPath, "supervisor", "start"], {
    detached: true,
    stdio: ["ignore", "ignore", "ignore"],
    env: { ...process.env, PTY_SESSION_DIR: sessionDir },
  });
  child.unref();
  bgPids.push(child.pid!);
  return child.pid!;
}

function waitForFile(filePath: string, timeoutMs = 5000): Promise<void> {
  const start = Date.now();
  return new Promise((resolve, reject) => {
    function check() {
      if (Date.now() - start > timeoutMs) {
        reject(new Error(`Timeout waiting for ${filePath}`));
        return;
      }
      try {
        fs.statSync(filePath);
        resolve();
        return;
      } catch {}
      setTimeout(check, 100);
    }
    check();
  });
}

function readMeta(sessionDir: string, name: string): any {
  try {
    return JSON.parse(fs.readFileSync(path.join(sessionDir, `${name}.json`), "utf-8"));
  } catch { return null; }
}

afterEach(() => {
  for (const pid of bgPids) { try { process.kill(pid, "SIGTERM"); } catch {} }
  bgPids = [];
  // Give supervisor time to release lock
  const start = Date.now();
  while (Date.now() - start < 200) {}
  for (const dir of sessionDirs) {
    try {
      for (const e of fs.readdirSync(dir)) {
        try { fs.unlinkSync(path.join(dir, e)); } catch {}
      }
    } catch {}
  }
  sessionDirs = [];
});

describe("supervisor", () => {
  it("restarts a permanent session that exits", async () => {
    const dir = makeSessionDir();
    process.env.PTY_SESSION_DIR = dir;

    // Start a session that exits after 0.5s
    await startDaemon(dir, "restartable", "sh", ["-c", "sleep 0.5"], { strategy: "permanent" });

    // Start the supervisor
    startSupervisor(dir);
    await new Promise((r) => setTimeout(r, 300)); // let supervisor start

    // Wait for session to exit + supervisor scan + backoff + restart
    // Session exits after 0.5s, supervisor scans every 30s but also uses fs.watch,
    // initial backoff is 1s, then spawnDaemon takes a moment
    const sockPath = path.join(dir, "restartable.sock");
    const start = Date.now();
    let restarted = false;
    while (Date.now() - start < 10000) {
      await new Promise((r) => setTimeout(r, 500));
      // Check if a new socket appeared (the old one gets cleaned up by cleanupAll)
      const meta = readMeta(dir, "restartable");
      // After restart, metadata should have no exitedAt (fresh session)
      if (meta && !meta.exitedAt) {
        try { fs.statSync(sockPath); restarted = true; break; } catch {}
      }
    }

    expect(restarted).toBe(true);
  }, 20000);

  it("cleans up temporary sessions on exit", async () => {
    const dir = makeSessionDir();
    process.env.PTY_SESSION_DIR = dir;

    // Start a temporary session that exits immediately
    await startDaemon(dir, "tempjob", "true", [], { strategy: "temporary" });

    // Start the supervisor
    startSupervisor(dir);
    await new Promise((r) => setTimeout(r, 300));

    // Wait for exit + cleanup
    await new Promise((r) => setTimeout(r, 2000));

    // Metadata should be gone
    const meta = readMeta(dir, "tempjob");
    expect(meta).toBeNull();
  }, 15000);

  it("does not restart sessions without strategy tag", async () => {
    const dir = makeSessionDir();
    process.env.PTY_SESSION_DIR = dir;

    // Start a session without strategy that exits
    await startDaemon(dir, "nosupervise", "sh", ["-c", "sleep 0.3"]);

    startSupervisor(dir);
    await new Promise((r) => setTimeout(r, 300));

    // Wait for exit + potential restart window
    await new Promise((r) => setTimeout(r, 3000));

    // Should NOT have restarted — no socket
    const sockPath = path.join(dir, "nosupervise.sock");
    let exists = false;
    try { fs.statSync(sockPath); exists = true; } catch {}
    expect(exists).toBe(false);
  }, 15000);

  it("pty tag can add strategy to a running session", async () => {
    const dir = makeSessionDir();
    process.env.PTY_SESSION_DIR = dir;

    await startDaemon(dir, "latejoin", "cat");

    const result = spawnSync(nodeBin, [cliPath, "tag", "latejoin", "strategy=permanent"], {
      env: { ...process.env, PTY_SESSION_DIR: dir },
      encoding: "utf-8",
    });
    expect(result.status).toBe(0);

    const meta = readMeta(dir, "latejoin");
    expect(meta.tags.strategy).toBe("permanent");
  }, 15000);

  it("pty supervisor forget removes strategy tag", async () => {
    const dir = makeSessionDir();
    process.env.PTY_SESSION_DIR = dir;

    await startDaemon(dir, "forgetme", "cat", [], { strategy: "permanent" });

    const result = spawnSync(nodeBin, [cliPath, "supervisor", "forget", "forgetme"], {
      env: { ...process.env, PTY_SESSION_DIR: dir },
      encoding: "utf-8",
    });
    expect(result.status).toBe(0);
    expect(result.stdout).toContain("Removed supervision");

    const meta = readMeta(dir, "forgetme");
    expect(meta.tags?.strategy).toBeUndefined();
  }, 15000);

  it("pty down stops supervised sessions and removes strategy tag", async () => {
    const projDir = fs.mkdtempSync(path.join(testRoot, "proj-"));
    const dir = makeSessionDir();
    fs.writeFileSync(path.join(projDir, "pty.toml"), `
[sessions.guarded]
command = "cat"
tags = { strategy = "permanent" }
`);

    // Start via pty up
    spawnSync(nodeBin, [cliPath, "up", projDir], {
      env: { ...process.env, PTY_SESSION_DIR: dir },
      encoding: "utf-8",
    });

    // Stop it
    const result = spawnSync(nodeBin, [cliPath, "down", projDir], {
      env: { ...process.env, PTY_SESSION_DIR: dir },
      encoding: "utf-8",
    });

    expect(result.status).toBe(0);
    expect(result.stdout).toContain("stopped");
    expect(result.stdout).toContain("removed from supervision");

    // Strategy tag should be gone
    const meta = readMeta(dir, "guarded");
    expect(meta?.tags?.strategy).toBeUndefined();
  }, 15000);

  it("pty list shows strategy markers", async () => {
    const dir = makeSessionDir();
    process.env.PTY_SESSION_DIR = dir;

    await startDaemon(dir, "marked", "cat", [], { strategy: "permanent" });

    const result = spawnSync(nodeBin, [cliPath, "list"], {
      env: { ...process.env, PTY_SESSION_DIR: dir },
      encoding: "utf-8",
    });

    expect(result.stdout).toContain("marked");
    expect(result.stdout).toContain("[permanent]");
  }, 15000);
});
