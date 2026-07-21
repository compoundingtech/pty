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

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-tagmut-"));
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
  return `tm${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
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

describe("pty tag (mutable tags)", () => {
  it("sets tags on a running session", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "cat");

    const result = runCli(dir, "tag", name, "role=server", "env=dev");
    expect(result.status).toBe(0);
    expect(result.stdout).toContain("role=server");
    expect(result.stdout).toContain("env=dev");

    const meta = readMeta(dir, name);
    expect(meta.tags).toEqual({ role: "server", env: "dev" });
  }, 15000);

  it("updates existing tags", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "cat", [], { role: "old" });

    runCli(dir, "tag", name, "role=new");

    const meta = readMeta(dir, name);
    expect(meta.tags.role).toBe("new");
  }, 15000);

  it("removes tags with --rm", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "cat", [], { role: "server", env: "dev" });

    runCli(dir, "tag", name, "--rm", "env");

    const meta = readMeta(dir, name);
    expect(meta.tags).toEqual({ role: "server" });
    expect(meta.tags.env).toBeUndefined();
  }, 15000);

  it("removing all tags clears the field", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "cat", [], { only: "tag" });

    runCli(dir, "tag", name, "--rm", "only");

    const meta = readMeta(dir, name);
    expect(meta.tags).toBeUndefined();
  }, 15000);

  it("shows current tags with no arguments", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "cat", [], { role: "server" });

    const result = runCli(dir, "tag", name);
    expect(result.status).toBe(0);
    expect(result.stdout).toContain("role=server");
  }, 15000);

  it("shows 'no tags' when empty", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "cat");

    const result = runCli(dir, "tag", name);
    expect(result.stdout).toContain("No tags");
  }, 15000);

  it("works on exited sessions", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    // `keep=true` exempts the session from the daemon's exit-time self-reap,
    // so there is still an exited session left to tag.
    await startDaemon(dir, name, "true", [], { keep: "true" }); // exits immediately
    await new Promise((r) => setTimeout(r, 1000));

    const result = runCli(dir, "tag", name, "strategy=permanent");
    expect(result.status).toBe(0);

    const meta = readMeta(dir, name);
    expect(meta.tags).toEqual({ keep: "true", strategy: "permanent" });
  }, 15000);

  it("errors on nonexistent session", () => {
    const dir = makeSessionDir();

    const result = runCli(dir, "tag", "nonexistent", "foo=bar");
    expect(result.status).not.toBe(0);
    expect(result.stderr).toContain("not found");
  }, 15000);
});
