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

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-tags-"));
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
  return `tag${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
}

async function startDaemon(
  sessionDir: string,
  name: string,
  command: string,
  args: string[] = [],
  tags?: Record<string, string>,
): Promise<number> {
  const config = JSON.stringify({
    name,
    command,
    args,
    displayCommand: command,
    cwd: os.tmpdir(),
    rows: 24,
    cols: 80,
    tags,
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

function runCli(sessionDir: string, ...args: string[]): string {
  return spawnSync(nodeBin, [cliPath, ...args], {
    env: { ...process.env, PTY_SESSION_DIR: sessionDir },
    encoding: "utf-8",
    timeout: 10000,
  }).stdout;
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

describe("session tags", () => {
  it("tags are persisted in session metadata", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "cat", [], { owner: "forge", env: "dev" });

    const metaPath = path.join(dir, `${name}.json`);
    const meta = JSON.parse(fs.readFileSync(metaPath, "utf-8"));
    expect(meta.tags).toEqual({ owner: "forge", env: "dev" });
  }, 15000);

  it("tags appear in pty list --json", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "cat", [], { owner: "myapp" });

    const output = runCli(dir, "list", "--json");
    const sessions = JSON.parse(output);
    const session = sessions.find((s: any) => s.name === name);
    expect(session).toBeDefined();
    expect(session.tags).toEqual({ owner: "myapp" });
  }, 15000);

  it("pty list --filter-tag filters JSON output to matching sessions", async () => {
    const dir = makeSessionDir();
    const matchName = uniqueName();
    const otherName = uniqueName();
    await startDaemon(dir, matchName, "cat", [], { layout: "work", role: "srv" });
    await startDaemon(dir, otherName, "cat", [], { layout: "play" });

    const output = runCli(dir, "list", "--json", "--filter-tag", "layout=work");
    const sessions = JSON.parse(output);
    expect(sessions.map((s: any) => s.name)).toEqual([matchName]);
  }, 15000);

  it("pty list --filter-tag requires all tags to match (AND)", async () => {
    const dir = makeSessionDir();
    const bothName = uniqueName();
    const oneName = uniqueName();
    await startDaemon(dir, bothName, "cat", [], { layout: "work", role: "srv" });
    await startDaemon(dir, oneName, "cat", [], { layout: "work" });

    const output = runCli(dir, "list", "--json", "--filter-tag", "layout=work", "--filter-tag", "role=srv");
    const sessions = JSON.parse(output);
    expect(sessions.map((s: any) => s.name)).toEqual([bothName]);
  }, 15000);

  it("pty list --filter-tag filters text output too", async () => {
    const dir = makeSessionDir();
    const matchName = uniqueName();
    const otherName = uniqueName();
    await startDaemon(dir, matchName, "cat", [], { layout: "work" });
    await startDaemon(dir, otherName, "cat", [], { layout: "play" });

    const output = runCli(dir, "list", "--filter-tag", "layout=work");
    expect(output).toContain(matchName);
    expect(output).not.toContain(otherName);
  }, 15000);

  it("sessions without tags have no tags field in metadata", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "cat");

    const metaPath = path.join(dir, `${name}.json`);
    const meta = JSON.parse(fs.readFileSync(metaPath, "utf-8"));
    expect(meta.tags).toBeUndefined();
  }, 15000);

  it("tags survive process exit (persisted in exit metadata)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "true", [], { owner: "ci" }); // exits immediately
    await new Promise((r) => setTimeout(r, 1000)); // wait for exit + metadata write

    const metaPath = path.join(dir, `${name}.json`);
    const meta = JSON.parse(fs.readFileSync(metaPath, "utf-8"));
    expect(meta.tags).toEqual({ owner: "ci" });
    expect(meta.exitCode).toBe(0);
  }, 15000);

  it("CLI --tag flag sets tags on session", () => {
    const dir = makeSessionDir();
    const name = uniqueName();

    const result = spawnSync(nodeBin, [
      cliPath, "run", "-d", "--name", name,
      "--tag", "owner=forge", "--tag", "env=staging",
      "--", "cat",
    ], {
      env: { ...process.env, PTY_SESSION_DIR: dir },
      encoding: "utf-8",
      timeout: 10000,
    });

    expect(result.status).toBe(0);

    const metaPath = path.join(dir, `${name}.json`);
    const meta = JSON.parse(fs.readFileSync(metaPath, "utf-8"));
    expect(meta.tags).toEqual({ owner: "forge", env: "staging" });

    // Clean up daemon
    const pidFile = path.join(dir, `${name}.pid`);
    const pid = parseInt(fs.readFileSync(pidFile, "utf-8").trim(), 10);
    try { process.kill(pid, "SIGTERM"); } catch {}
  }, 15000);

  it("tags survive restart via CLI", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();

    // Create a session with tags that exits immediately
    await startDaemon(dir, name, "true", [], { owner: "forge", env: "prod" });
    await new Promise((r) => setTimeout(r, 1000)); // wait for exit + metadata

    // Verify tags are on the exited session
    const metaBefore = JSON.parse(fs.readFileSync(path.join(dir, `${name}.json`), "utf-8"));
    expect(metaBefore.tags).toEqual({ owner: "forge", env: "prod" });
    expect(metaBefore.exitCode).toBe(0);

    // Restart via CLI
    const result = spawnSync(nodeBin, [cliPath, "restart", "-y", name], {
      env: { ...process.env, PTY_SESSION_DIR: dir },
      encoding: "utf-8",
      timeout: 10000,
    });
    // restart attaches, so it will exit with non-zero since there's no TTY
    // but the session should be created

    // Wait for the restarted session to write metadata
    await new Promise((r) => setTimeout(r, 500));

    const metaAfter = JSON.parse(fs.readFileSync(path.join(dir, `${name}.json`), "utf-8"));
    expect(metaAfter.tags).toEqual({ owner: "forge", env: "prod" });
    // createdAt should differ from the original session (it was recreated)
    expect(metaAfter.createdAt).not.toBe(metaBefore.createdAt);
  }, 15000);

  it("tags preserved when run -a recreates exited session", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();

    // Create a tagged session that exits immediately
    await startDaemon(dir, name, "true", [], { owner: "ci" });
    await new Promise((r) => setTimeout(r, 1000)); // wait for exit

    // Recreate with run -a (no new --tag flags)
    const result = spawnSync(nodeBin, [cliPath, "run", "-a", "-d", "--name", name, "--", "cat"], {
      env: { ...process.env, PTY_SESSION_DIR: dir },
      encoding: "utf-8",
      timeout: 10000,
    });
    expect(result.status).toBe(0);

    // Verify tags carried over
    const meta = JSON.parse(fs.readFileSync(path.join(dir, `${name}.json`), "utf-8"));
    expect(meta.tags).toEqual({ owner: "ci" });

    // Clean up daemon
    const pidFile = path.join(dir, `${name}.pid`);
    try {
      const pid = parseInt(fs.readFileSync(pidFile, "utf-8").trim(), 10);
      process.kill(pid, "SIGTERM");
    } catch {}
  }, 15000);

  it("new --tag flags override previous tags on run -a", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();

    // Create a tagged session that exits immediately
    await startDaemon(dir, name, "true", [], { owner: "old" });
    await new Promise((r) => setTimeout(r, 1000)); // wait for exit

    // Recreate with run -a with NEW tags
    const result = spawnSync(nodeBin, [
      cliPath, "run", "-a", "-d", "--name", name,
      "--tag", "owner=new",
      "--", "cat",
    ], {
      env: { ...process.env, PTY_SESSION_DIR: dir },
      encoding: "utf-8",
      timeout: 10000,
    });
    expect(result.status).toBe(0);

    // Verify new tags replaced old ones
    const meta = JSON.parse(fs.readFileSync(path.join(dir, `${name}.json`), "utf-8"));
    expect(meta.tags).toEqual({ owner: "new" });

    // Clean up daemon
    const pidFile = path.join(dir, `${name}.pid`);
    try {
      const pid = parseInt(fs.readFileSync(pidFile, "utf-8").trim(), 10);
      process.kill(pid, "SIGTERM");
    } catch {}
  }, 15000);

  it("CLI --tag with invalid format is rejected", () => {
    const dir = makeSessionDir();

    const result = spawnSync(nodeBin, [
      cliPath, "run", "-d", "--name", "bad-tag",
      "--tag", "no-equals-sign",
      "--", "cat",
    ], {
      env: { ...process.env, PTY_SESSION_DIR: dir },
      encoding: "utf-8",
      timeout: 10000,
    });

    expect(result.status).not.toBe(0);
    expect(result.stderr).toContain("key=value");
  }, 15000);
});
