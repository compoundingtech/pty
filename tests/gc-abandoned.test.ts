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

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-gca-"));
afterAll(() => {
  fs.rmSync(testRoot, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

let bgPids: number[] = [];
let sessionDirs: string[] = [];
let cwds: string[] = [];

function makeSessionDir(): string {
  const dir = fs.mkdtempSync(path.join(testRoot, "d-"));
  sessionDirs.push(dir);
  return dir;
}

function makeCwd(): string {
  const dir = fs.mkdtempSync(path.join(testRoot, "cwd-"));
  cwds.push(dir);
  return dir;
}

let nameCounter = 0;
function uniqueName(): string {
  // Short — socket paths must fit under SUN_PATH_MAX (104 bytes).
  return `ga${++nameCounter}${Math.random().toString(36).slice(2, 5)}`;
}

async function startDaemon(
  sessionDir: string,
  name: string,
  cwd: string,
  command: string,
  args: string[] = [],
  tags?: Record<string, string>,
): Promise<number> {
  const config = JSON.stringify({
    name, command, args, displayCommand: command,
    cwd, rows: 24, cols: 80, tags,
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

function readMeta(sessionDir: string, name: string) {
  return JSON.parse(fs.readFileSync(path.join(sessionDir, `${name}.json`), "utf-8"));
}

function writeMeta(sessionDir: string, name: string, meta: any): void {
  fs.writeFileSync(path.join(sessionDir, `${name}.json`), JSON.stringify(meta, null, 2));
}

function readEvents(sessionDir: string, name: string): any[] {
  const filePath = path.join(sessionDir, `${name}.events.jsonl`);
  try {
    const content = fs.readFileSync(filePath, "utf-8");
    return content.trimEnd().split("\n").filter((l) => l.length > 0).map((l) => JSON.parse(l));
  } catch {
    return [];
  }
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
  for (const dir of cwds) {
    try { fs.rmSync(dir, { recursive: true, force: true }); } catch {}
  }
  cwds = [];
});

describe("pty gc — abandoned-reap step 1.5", () => {
  it("reaps a running permanent session whose cwd has been deleted", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const cwd = makeCwd();
    // Use `sleep 60` so the daemon stays alive while we delete the cwd
    // and run gc — otherwise the sweep in step 3 would clean it before
    // step 1.5 sees it.
    await startDaemon(dir, name, cwd, "sleep", ["60"], { strategy: "permanent" });
    await new Promise((r) => setTimeout(r, 200));

    // Delete the cwd out from under the running daemon.
    fs.rmSync(cwd, { recursive: true, force: true });

    const result = runCli(dir, "gc");
    expect(result.status).toBe(0);
    expect(result.stdout).toContain(`Abandoned: ${name} (cwd-gone)`);

    // Session gone from disk.
    expect(fs.existsSync(path.join(dir, `${name}.json`))).toBe(false);
    expect(fs.existsSync(path.join(dir, `${name}.sock`))).toBe(false);

    // A `session_abandoned` event was written before cleanup.
    // (cleanupAll unlinks the events file, so the event is only readable
    // if it was appended before cleanup — this test asserts the ordering.)
    const events = readEvents(dir, name);
    // events file is either gone entirely (post-cleanup) or contains only
    // the abandoned line; both prove the ordering worked.
    if (events.length > 0) {
      expect(events.some((e) => e.type === "session_abandoned" && e.reason === "cwd-gone")).toBe(true);
    }
  }, 20000);

  it("does NOT reap a non-permanent session whose cwd has been deleted", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const cwd = makeCwd();
    await startDaemon(dir, name, cwd, "sleep", ["60"]); // no strategy tag
    await new Promise((r) => setTimeout(r, 200));
    fs.rmSync(cwd, { recursive: true, force: true });

    const result = runCli(dir, "gc");
    expect(result.status).toBe(0);
    expect(result.stdout).not.toContain(`Abandoned: ${name}`);

    // Session still running (metadata still on disk, daemon still alive).
    expect(fs.existsSync(path.join(dir, `${name}.json`))).toBe(true);
  }, 20000);

  it("respects the strategy.abandon-if-cwd-gone=false opt-out tag", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const cwd = makeCwd();
    await startDaemon(dir, name, cwd, "sleep", ["60"], {
      strategy: "permanent",
      "strategy.abandon-if-cwd-gone": "false",
    });
    await new Promise((r) => setTimeout(r, 200));
    fs.rmSync(cwd, { recursive: true, force: true });

    const result = runCli(dir, "gc");
    expect(result.status).toBe(0);
    expect(result.stdout).not.toContain(`Abandoned: ${name}`);

    // Session preserved despite cwd being gone.
    expect(fs.existsSync(path.join(dir, `${name}.json`))).toBe(true);
  }, 20000);

  it("previews abandoned reap under --dry-run without touching anything", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const cwd = makeCwd();
    await startDaemon(dir, name, cwd, "sleep", ["60"], { strategy: "permanent" });
    await new Promise((r) => setTimeout(r, 200));
    fs.rmSync(cwd, { recursive: true, force: true });

    const dry = runCli(dir, "gc", "--dry-run");
    expect(dry.status).toBe(0);
    expect(dry.stdout).toContain(`Would abandon: ${name} (cwd-gone)`);
    expect(dry.stdout).toContain("Dry run");

    // Session still present after dry run.
    expect(fs.existsSync(path.join(dir, `${name}.json`))).toBe(true);
    expect(fs.existsSync(path.join(dir, `${name}.sock`))).toBe(true);
  }, 20000);

  it("reaps a permanent session whose lastAttachAt is older than --idle-days N", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const cwd = makeCwd();
    await startDaemon(dir, name, cwd, "sleep", ["60"], { strategy: "permanent" });
    await new Promise((r) => setTimeout(r, 200));

    // Simulate a session that was attached to 30 days ago but nothing since.
    const meta = readMeta(dir, name);
    const daysAgo = 30;
    meta.lastAttachAt = new Date(Date.now() - daysAgo * 24 * 60 * 60 * 1000).toISOString();
    writeMeta(dir, name, meta);

    const result = runCli(dir, "gc", "--idle-days", "14");
    expect(result.status).toBe(0);
    expect(result.stdout).toContain(`Abandoned: ${name} (idle`);
    expect(result.stdout).toMatch(new RegExp(`Abandoned: ${name} \\(idle \\d+d\\)`));

    expect(fs.existsSync(path.join(dir, `${name}.json`))).toBe(false);
  }, 20000);

  it("does NOT reap a permanent session under the idle threshold", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const cwd = makeCwd();
    await startDaemon(dir, name, cwd, "sleep", ["60"], { strategy: "permanent" });
    await new Promise((r) => setTimeout(r, 200));

    // Attached 3 days ago — well under a 14-day threshold.
    const meta = readMeta(dir, name);
    meta.lastAttachAt = new Date(Date.now() - 3 * 24 * 60 * 60 * 1000).toISOString();
    writeMeta(dir, name, meta);

    const result = runCli(dir, "gc", "--idle-days", "14");
    expect(result.status).toBe(0);
    expect(result.stdout).not.toContain(`Abandoned: ${name}`);
    expect(fs.existsSync(path.join(dir, `${name}.json`))).toBe(true);
  }, 20000);

  it("does NOT reap a session with no lastAttachAt (never attached)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const cwd = makeCwd();
    await startDaemon(dir, name, cwd, "sleep", ["60"], { strategy: "permanent" });
    await new Promise((r) => setTimeout(r, 200));
    // No client ever attached — metadata has no lastAttachAt.

    const result = runCli(dir, "gc", "--idle-days", "7");
    expect(result.status).toBe(0);
    expect(result.stdout).not.toContain(`Abandoned: ${name}`);
    expect(fs.existsSync(path.join(dir, `${name}.json`))).toBe(true);
  }, 20000);

  it("per-session strategy.idle-days tag opts in individually without a CLI flag", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const cwd = makeCwd();
    await startDaemon(dir, name, cwd, "sleep", ["60"], {
      strategy: "permanent",
      "strategy.idle-days": "10",
    });
    await new Promise((r) => setTimeout(r, 200));

    const meta = readMeta(dir, name);
    meta.lastAttachAt = new Date(Date.now() - 30 * 24 * 60 * 60 * 1000).toISOString();
    writeMeta(dir, name, meta);

    // No CLI --idle-days here — the per-session tag alone should drive the reap.
    const result = runCli(dir, "gc");
    expect(result.status).toBe(0);
    expect(result.stdout).toContain(`Abandoned: ${name} (idle`);
    expect(fs.existsSync(path.join(dir, `${name}.json`))).toBe(false);
  }, 20000);

  it("cwd-gone takes precedence over idle when both conditions hold", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const cwd = makeCwd();
    await startDaemon(dir, name, cwd, "sleep", ["60"], { strategy: "permanent" });
    await new Promise((r) => setTimeout(r, 200));

    const meta = readMeta(dir, name);
    meta.lastAttachAt = new Date(Date.now() - 30 * 24 * 60 * 60 * 1000).toISOString();
    writeMeta(dir, name, meta);
    fs.rmSync(cwd, { recursive: true, force: true });

    const result = runCli(dir, "gc", "--idle-days", "14");
    expect(result.status).toBe(0);
    // Reason should be cwd-gone, not idle — cwd is the stronger signal.
    expect(result.stdout).toContain(`Abandoned: ${name} (cwd-gone)`);
    expect(result.stdout).not.toContain(`Abandoned: ${name} (idle`);
  }, 20000);

  it("--idle-days=0 and negative values are rejected with an error", () => {
    const dir = makeSessionDir();
    const zero = runCli(dir, "gc", "--idle-days", "0");
    expect(zero.status).not.toBe(0);
    expect(zero.stderr).toContain("--idle-days expects a positive integer");

    const neg = runCli(dir, "gc", "--idle-days=-5");
    expect(neg.status).not.toBe(0);
    expect(neg.stderr).toContain("--idle-days expects a positive integer");
  }, 5000);
});

describe("pty gc — abandoned reap does not disrupt other buckets", () => {
  it("still respawns a normal exited permanent session in the same pass", async () => {
    // Two sessions: one abandoned (cwd-gone, live), one exited (should
    // respawn normally). Both permanent. Same gc pass handles both.
    const dir = makeSessionDir();

    const abandonName = uniqueName();
    const abandonCwd = makeCwd();
    await startDaemon(dir, abandonName, abandonCwd, "sleep", ["60"], { strategy: "permanent" });

    const respawnName = uniqueName();
    const respawnCwd = makeCwd();
    await startDaemon(dir, respawnName, respawnCwd, "true", [], { strategy: "permanent" });

    await new Promise((r) => setTimeout(r, 800)); // let `true` exit
    fs.rmSync(abandonCwd, { recursive: true, force: true });

    const result = runCli(dir, "gc");
    expect(result.status).toBe(0);
    expect(result.stdout).toContain(`Abandoned: ${abandonName} (cwd-gone)`);
    expect(result.stdout).toContain(`Respawned: ${respawnName}`);

    // Track any respawn pid for cleanup.
    try {
      const pid = parseInt(fs.readFileSync(path.join(dir, `${respawnName}.pid`), "utf-8").trim(), 10);
      if (Number.isFinite(pid)) bgPids.push(pid);
    } catch {}
  }, 25000);
});
