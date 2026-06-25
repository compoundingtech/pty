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

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-gcpc-"));
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
function uniqueName(prefix = "pc"): string {
  return `${prefix}${++nameCounter}${Math.random().toString(36).slice(2, 4)}`;
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

describe("pty gc — parent-child orphan-kill", () => {
  it("kills a child when its parent's daemon is dead (no exit record)", async () => {
    const dir = makeSessionDir();
    const parent = uniqueName("par");
    const child = uniqueName("ch");

    const parentPid = await startDaemon(dir, parent, "cat");
    await startDaemon(dir, child, "cat", [], { parent });

    // SIGKILL the parent daemon — vanished status (no exitedAt).
    try { process.kill(parentPid, "SIGKILL"); } catch {}
    await new Promise((r) => setTimeout(r, 300));

    const result = runCli(dir, "gc");
    expect(result.status).toBe(0);
    expect(result.stdout).toContain(`Killed orphan child: ${child} (parent ${parent}`);
    // Child metadata is gone after cleanupAll.
    expect(fs.existsSync(path.join(dir, `${child}.json`))).toBe(false);
  }, 15000);

  it("kills a child when parent metadata is missing entirely", async () => {
    const dir = makeSessionDir();
    const child = uniqueName("ch");
    // No parent ever existed. The tag points at a name that doesn't
    // resolve. Step 1 should still kill the child.
    await startDaemon(dir, child, "cat", [], { parent: "nonexistent-parent" });

    const result = runCli(dir, "gc");
    expect(result.status).toBe(0);
    expect(result.stdout).toContain(`Killed orphan child: ${child} (parent nonexistent-parent missing)`);
    expect(fs.existsSync(path.join(dir, `${child}.json`))).toBe(false);
  }, 15000);

  it("preserves a child whose parent is alive", async () => {
    const dir = makeSessionDir();
    const parent = uniqueName("par");
    const child = uniqueName("ch");

    await startDaemon(dir, parent, "cat");
    await startDaemon(dir, child, "cat", [], { parent });

    const result = runCli(dir, "gc");
    expect(result.status).toBe(0);
    expect(result.stdout).not.toContain(`Killed orphan child: ${child}`);
    // Both metadata files still on disk.
    expect(fs.existsSync(path.join(dir, `${parent}.json`))).toBe(true);
    expect(fs.existsSync(path.join(dir, `${child}.json`))).toBe(true);
  }, 15000);

  it("cycle A→B, B→A — name-sorted resolution kills both deterministically", async () => {
    // Pick names where 'a' sorts before 'b' for determinism.
    const dir = makeSessionDir();
    const a = `a${Math.random().toString(36).slice(2, 5)}`;
    const b = `b${Math.random().toString(36).slice(2, 5)}`;

    // Spawn A pointing at B and B pointing at A. Then SIGKILL both
    // daemons so both look vanished — by the orphan-kill rule, since
    // each parent is dead, both children should be killed.
    const aPid = await startDaemon(dir, a, "cat", [], { parent: b });
    const bPid = await startDaemon(dir, b, "cat", [], { parent: a });
    try { process.kill(aPid, "SIGKILL"); } catch {}
    try { process.kill(bPid, "SIGKILL"); } catch {}
    await new Promise((r) => setTimeout(r, 300));

    const result = runCli(dir, "gc");
    expect(result.status).toBe(0);
    // Both get killed-as-orphan in the same pass — A first (sorts
    // earlier), B second.
    expect(result.stdout).toContain(`Killed orphan child: ${a}`);
    expect(result.stdout).toContain(`Killed orphan child: ${b}`);
  }, 15000);

  it("combined parent= AND strategy=permanent: child is killed, not respawned", async () => {
    // Orphan-kill (step 1) runs BEFORE permanent respawn (step 2), so a
    // child with both tags whose parent has died should be removed —
    // not respawned as a permanent.
    const dir = makeSessionDir();
    const parent = uniqueName("par");
    const child = uniqueName("ch");

    const parentPid = await startDaemon(dir, parent, "cat");
    await startDaemon(dir, child, "cat", [], { parent, strategy: "permanent" });

    try { process.kill(parentPid, "SIGKILL"); } catch {}
    await new Promise((r) => setTimeout(r, 300));

    const result = runCli(dir, "gc");
    expect(result.status).toBe(0);
    expect(result.stdout).toContain(`Killed orphan child: ${child}`);
    expect(result.stdout).not.toContain(`Respawned: ${child}`);
    expect(fs.existsSync(path.join(dir, `${child}.json`))).toBe(false);
  }, 15000);

  it("--dry-run previews orphan-kill without mutating anything", async () => {
    const dir = makeSessionDir();
    const child = uniqueName("ch");
    await startDaemon(dir, child, "cat", [], { parent: "nonexistent-parent" });

    const dry = runCli(dir, "gc", "--dry-run");
    expect(dry.status).toBe(0);
    expect(dry.stdout).toContain(`Would kill orphan child: ${child} (parent nonexistent-parent missing)`);
    expect(dry.stdout).toContain("Dry run");
    // Child still alive on disk and its daemon untouched.
    expect(fs.existsSync(path.join(dir, `${child}.json`))).toBe(true);
  }, 15000);
});
