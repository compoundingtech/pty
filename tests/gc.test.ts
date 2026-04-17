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

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-gc-"));
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
  return `gc${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
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

function runCli(sessionDir: string, ...args: string[]) {
  return spawnSync(nodeBin, [cliPath, ...args], {
    env: { ...process.env, PTY_SESSION_DIR: sessionDir },
    encoding: "utf-8",
    timeout: 10000,
  });
}

function readMeta(sessionDir: string, name: string) {
  return JSON.parse(fs.readFileSync(path.join(sessionDir, `${name}.json`), "utf-8"));
}

/** Find an unused PID so ESRCH is deterministic. We probe from 999999
 *  downward since typical systems reuse low PIDs. */
function findDeadPid(): number {
  for (let p = 999999; p > 900000; p -= 7) {
    try { process.kill(p, 0); } catch { return p; }
  }
  throw new Error("Could not find an unused PID for the test");
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

describe("pty gc", () => {
  it("removes exited sessions", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "true");
    await new Promise((r) => setTimeout(r, 1000));

    const before = fs.existsSync(path.join(dir, `${name}.json`));
    expect(before).toBe(true);

    const result = runCli(dir, "gc");
    expect(result.status).toBe(0);
    expect(result.stdout).toContain(`Removed: ${name}`);
    expect(fs.existsSync(path.join(dir, `${name}.json`))).toBe(false);
  }, 15000);

  it("prunes `:l<pid>-<rand>` tags whose PID is dead", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const deadPid = findDeadPid();
    const tagKey = `:l${deadPid}-abc`;

    await startDaemon(dir, name, "cat", [], {
      role: "web",
      [tagKey]: "1",
    });

    // Sanity: tag is on the session before gc.
    expect(readMeta(dir, name).tags[tagKey]).toBe("1");

    const result = runCli(dir, "gc");
    expect(result.status).toBe(0);
    expect(result.stdout).toContain(`Pruned orphan tags on ${name}: #${tagKey}`);

    // Session still exists (it's running); the orphan tag is gone;
    // normal tags are untouched.
    const meta = readMeta(dir, name);
    expect(meta.tags[tagKey]).toBeUndefined();
    expect(meta.tags.role).toBe("web");
  }, 15000);

  it("keeps `:l<pid>-<rand>` tags whose PID is still alive", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const liveTagKey = `:l${process.pid}-xyz`;

    await startDaemon(dir, name, "cat", [], {
      [liveTagKey]: "1",
    });

    const result = runCli(dir, "gc");
    expect(result.status).toBe(0);
    expect(result.stdout).not.toContain(liveTagKey);

    const meta = readMeta(dir, name);
    expect(meta.tags[liveTagKey]).toBe("1");
  }, 15000);

  it("does not prune non-layout `:` tags", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();

    // `:` prefix alone is reserved/display-hidden but NOT an orphan
    // layout tag — gc should leave it alone even if no PID is encoded.
    await startDaemon(dir, name, "cat", [], {
      ":layout": "grid",
      ":other": "x",
    });

    const result = runCli(dir, "gc");
    expect(result.status).toBe(0);

    const meta = readMeta(dir, name);
    expect(meta.tags[":layout"]).toBe("grid");
    expect(meta.tags[":other"]).toBe("x");
  }, 15000);

  it("reports nothing to clean up when no exited sessions and no orphan tags", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "cat");

    const result = runCli(dir, "gc");
    expect(result.status).toBe(0);
    expect(result.stdout).toContain("Nothing to clean up.");
  }, 15000);
});
