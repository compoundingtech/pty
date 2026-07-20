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

  it("--dry-run previews exited-session removal without deleting", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "true");
    await new Promise((r) => setTimeout(r, 1000));

    expect(fs.existsSync(path.join(dir, `${name}.json`))).toBe(true);

    const dry = runCli(dir, "gc", "--dry-run");
    expect(dry.status).toBe(0);
    expect(dry.stdout).toContain(`Would remove: ${name}`);
    expect(dry.stdout).toContain("Dry run");
    // Metadata still on disk after dry-run.
    expect(fs.existsSync(path.join(dir, `${name}.json`))).toBe(true);

    // And the real gc then actually removes it.
    const real = runCli(dir, "gc");
    expect(real.status).toBe(0);
    expect(real.stdout).toContain(`Removed: ${name}`);
    expect(fs.existsSync(path.join(dir, `${name}.json`))).toBe(false);
  }, 15000);

  it("--dry-run previews orphan tag pruning without mutating metadata", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const deadPid = findDeadPid();
    const tagKey = `:l${deadPid}-abc`;

    await startDaemon(dir, name, "cat", [], { [tagKey]: "1" });
    expect(readMeta(dir, name).tags[tagKey]).toBe("1");

    const dry = runCli(dir, "gc", "--dry-run");
    expect(dry.status).toBe(0);
    expect(dry.stdout).toContain(`Would prune orphan tags on ${name}: #${tagKey}`);
    // Tag is still there after dry-run.
    expect(readMeta(dir, name).tags[tagKey]).toBe("1");

    // And a real gc then actually removes it.
    const real = runCli(dir, "gc");
    expect(real.status).toBe(0);
    expect(real.stdout).toContain(`Pruned orphan tags on ${name}: #${tagKey}`);
    // The pruned key was the only tag on the session, so `tags` is cleared
    // entirely by updateTags — either form proves the orphan is gone.
    expect(readMeta(dir, name).tags?.[tagKey]).toBeUndefined();
  }, 15000);

  it("-n is accepted as an alias for --dry-run", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "true");
    await new Promise((r) => setTimeout(r, 1000));

    const result = runCli(dir, "gc", "-n");
    expect(result.status).toBe(0);
    expect(result.stdout).toContain(`Would remove: ${name}`);
    expect(fs.existsSync(path.join(dir, `${name}.json`))).toBe(true);
  }, 15000);

  it("reaps vanished sessions (dead PID, no exit record)", () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    // Simulate a vanished session: metadata file with no exitedAt/exitCode
    // and no pid/sock files. listSessions will infer status=vanished.
    const metaPath = path.join(dir, `${name}.json`);
    fs.writeFileSync(metaPath, JSON.stringify({
      command: "cat",
      args: [],
      displayCommand: "cat",
      cwd: os.tmpdir(),
      createdAt: new Date().toISOString(),
    }));

    const result = runCli(dir, "gc");
    expect(result.status).toBe(0);
    expect(result.stdout).toContain(`Removed: ${name}`);
    expect(fs.existsSync(metaPath)).toBe(false);
  }, 10000);

  it("--print-launchd-plist emits a valid-looking plist", () => {
    const dir = makeSessionDir();
    const result = runCli(dir, "gc", "--print-launchd-plist");
    expect(result.status).toBe(0);
    expect(result.stdout).toContain("<!DOCTYPE plist");
    // Non-default root (tests use a tmp registry) — Label carries the
    // basename suffix. Match either the default or the non-default shape.
    expect(result.stdout).toMatch(/<string>com\.compoundingtech\.pty\.gc(?:\.[A-Za-z0-9._-]+)?<\/string>/);
    expect(result.stdout).toContain("<key>StartInterval</key>");
    expect(result.stdout).toContain("<integer>30</integer>");
    // Phase-2: emitted env var is PTY_ROOT (canonical). Legacy
    // PTY_SESSION_DIR readers migrate via the alias in getSessionDir().
    expect(result.stdout).toContain("<key>PTY_ROOT</key>");
  });

  it("--print-launchd-plist --interval=N sets the interval", () => {
    const dir = makeSessionDir();
    const result = runCli(dir, "gc", "--print-launchd-plist", "--interval=15");
    expect(result.status).toBe(0);
    expect(result.stdout).toContain("<integer>15</integer>");
  });

  it("rejects --interval=0 and non-numeric intervals", () => {
    const dir = makeSessionDir();
    const r1 = runCli(dir, "gc", "--print-launchd-plist", "--interval=0");
    expect(r1.status).not.toBe(0);
    const r2 = runCli(dir, "gc", "--print-launchd-plist", "--interval=abc");
    expect(r2.status).not.toBe(0);
  });
});
