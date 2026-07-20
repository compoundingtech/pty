import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, execFileSync } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const serverModule = path.join(__dirname, "..", "dist", "server.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-rmkill-"));
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
  return `t${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
}

async function startDaemon(
  sessionDir: string,
  name: string,
  command: string,
  args: string[] = [],
  opts: { ephemeral?: boolean; tags?: Record<string, string> } = {},
): Promise<number> {
  const config = JSON.stringify({
    name,
    command,
    args,
    displayCommand: command,
    cwd: os.tmpdir(),
    rows: 24,
    cols: 80,
    ephemeral: opts.ephemeral ?? false,
    tags: opts.tags,
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

function runCli(sessionDir: string, ...args: string[]): { stdout: string; status: number } {
  try {
    const stdout = execFileSync(nodeBin, [cliPath, ...args], {
      env: { ...process.env, PTY_SESSION_DIR: sessionDir },
      encoding: "utf-8",
      timeout: 10000,
    });
    return { stdout, status: 0 };
  } catch (err: any) {
    return { stdout: (err.stdout ?? "") + (err.stderr ?? ""), status: err.status ?? 1 };
  }
}

function sessionFiles(dir: string, name: string): string[] {
  try {
    return fs.readdirSync(dir).filter((f) => f.startsWith(name));
  } catch {
    return [];
  }
}

afterEach(() => {
  for (const pid of bgPids) {
    try { process.kill(pid, "SIGTERM"); } catch {}
  }
  bgPids = [];
  for (const dir of sessionDirs) {
    try {
      for (const e of fs.readdirSync(dir)) {
        try { fs.unlinkSync(path.join(dir, e)); } catch {}
      }
    } catch {}
  }
  sessionDirs = [];
});

// --- pty kill ---

describe("pty kill", () => {
  it("kills a running session and keeps metadata", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "cat");

    const result = runCli(dir, "kill", name);
    expect(result.stdout).toContain("killed");

    // Wait for process to die
    await new Promise((r) => setTimeout(r, 500));

    // Socket should be gone, but metadata should remain
    const files = sessionFiles(dir, name);
    expect(files.some((f) => f.endsWith(".json"))).toBe(true);
    expect(files.some((f) => f.endsWith(".sock"))).toBe(false);
  }, 15000);

  it("refuses to kill an exited session", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    // `keep=true` exempts the session from the daemon's exit-time self-reap,
    // so there is still an exited session for `kill` to refuse.
    await startDaemon(dir, name, "true", [], { tags: { keep: "true" } }); // exits immediately
    await new Promise((r) => setTimeout(r, 1000));

    const result = runCli(dir, "kill", name);
    expect(result.status).not.toBe(0);
    expect(result.stdout).toContain("not running");
  }, 15000);

  it("errors for nonexistent session", () => {
    const dir = makeSessionDir();
    const result = runCli(dir, "kill", "nope");
    expect(result.status).not.toBe(0);
    expect(result.stdout).toContain("not found");
  }, 15000);
});

// --- pty rm ---

describe("pty rm", () => {
  it("removes metadata for an exited session", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    // `keep=true` exempts the session from the daemon's exit-time self-reap,
    // so there is still metadata left for `rm` to remove.
    await startDaemon(dir, name, "true", [], { tags: { keep: "true" } }); // exits immediately
    await new Promise((r) => setTimeout(r, 1000));

    // Verify metadata exists
    expect(sessionFiles(dir, name).some((f) => f.endsWith(".json"))).toBe(true);

    const result = runCli(dir, "rm", name);
    expect(result.stdout).toContain("removed");

    // All files should be gone
    expect(sessionFiles(dir, name)).toEqual([]);
  }, 15000);

  it("refuses to remove a running session", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "cat");

    const result = runCli(dir, "rm", name);
    expect(result.status).not.toBe(0);
    expect(result.stdout).toContain("still running");
  }, 15000);

  it("errors for nonexistent session", () => {
    const dir = makeSessionDir();
    const result = runCli(dir, "rm", "nope");
    expect(result.status).not.toBe(0);
    expect(result.stdout).toContain("not found");
  }, 15000);
});

// --- --ephemeral ---

describe("--ephemeral", () => {
  it("cleans up all session files after process exits", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "sh", ["-c", "exit 0"], { ephemeral: true });

    // Wait for process to exit and cleanup
    await new Promise((r) => setTimeout(r, 2000));

    // All session files should be gone
    const files = sessionFiles(dir, name);
    expect(files).toEqual([]);
  }, 15000);

  it("session is visible while running", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "cat", [], { ephemeral: true });

    // Should appear in ls while running
    const result = runCli(dir, "ls", "--json");
    const sessions = JSON.parse(result.stdout);
    expect(sessions.some((s: any) => s.name === name)).toBe(true);
  }, 15000);

  it("disappears from ls after exit", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "sh", ["-c", "exit 0"], { ephemeral: true });

    await new Promise((r) => setTimeout(r, 2000));

    const result = runCli(dir, "ls", "--json");
    const sessions = JSON.parse(result.stdout);
    expect(sessions.some((s: any) => s.name === name)).toBe(false);
  }, 15000);
});
