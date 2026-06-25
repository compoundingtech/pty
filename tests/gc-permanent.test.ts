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

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-gcp-"));
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
  // Short — socket paths must fit under SUN_PATH_MAX (104 bytes).
  return `gp${++nameCounter}${Math.random().toString(36).slice(2, 5)}`;
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

function readMeta(sessionDir: string, name: string) {
  return JSON.parse(fs.readFileSync(path.join(sessionDir, `${name}.json`), "utf-8"));
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
});

describe("pty gc — strategy=permanent respawn", () => {
  it("respawns an exited permanent session", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    // First daemon runs `true` and exits immediately — gives us the
    // exited-permanent setup gc should react to.
    const firstPid = await startDaemon(dir, name, "true", [], { strategy: "permanent" });
    await new Promise((r) => setTimeout(r, 800));

    const before = readMeta(dir, name);
    expect(before.exitedAt).toBeTruthy();

    const result = runCli(dir, "gc");
    expect(result.status).toBe(0);
    expect(result.stdout).toContain(`Respawned: ${name}`);

    // session_respawn event was written before the respawned daemon's
    // own session_start, so the event log contains both. Verify the
    // respawn event regardless of whether the new `true` daemon has
    // also already exited by the time we read.
    await new Promise((r) => setTimeout(r, 200));
    const events = readEvents(dir, name);
    expect(events.some((e) => e.type === "session_respawn")).toBe(true);

    // The new daemon has a different pid than the first one (proves a
    // real respawn happened, not just a no-op).
    try {
      const pidStr = fs.readFileSync(path.join(dir, `${name}.pid`), "utf-8").trim();
      const pid = parseInt(pidStr, 10);
      if (Number.isFinite(pid)) {
        bgPids.push(pid);
        expect(pid).not.toBe(firstPid);
      }
    } catch {
      // pid file may already be cleaned by the new daemon exiting — that
      // just confirms a fresh daemon ran.
    }
  }, 20000);

  it("does NOT respawn an exited session without strategy=permanent", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "true");
    await new Promise((r) => setTimeout(r, 800));

    const result = runCli(dir, "gc");
    expect(result.status).toBe(0);
    expect(result.stdout).not.toContain(`Respawned: ${name}`);
    expect(result.stdout).toContain(`Removed: ${name}`);
    // Metadata gone (was reaped by step 3).
    expect(fs.existsSync(path.join(dir, `${name}.json`))).toBe(false);
  }, 15000);

  it("--dry-run previews respawn without spawning anything", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "true", [], { strategy: "permanent" });
    await new Promise((r) => setTimeout(r, 800));

    const dry = runCli(dir, "gc", "--dry-run");
    expect(dry.status).toBe(0);
    expect(dry.stdout).toContain(`Would respawn: ${name}`);
    expect(dry.stdout).toContain("Dry run");

    // Still exited — no actual respawn happened. Metadata's exitedAt
    // should still be present.
    const meta = readMeta(dir, name);
    expect(meta.exitedAt).toBeTruthy();
    expect(fs.existsSync(path.join(dir, `${name}.pid`))).toBe(false);
  }, 15000);

  it("respawn does not loop within one invocation", async () => {
    // If a permanent session's command exits immediately (`true`), one
    // gc invocation should respawn it exactly once — not loop until the
    // process settles or stack-overflow on a sub-second exit.
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "true", [], { strategy: "permanent" });
    await new Promise((r) => setTimeout(r, 800));

    const result = runCli(dir, "gc");
    expect(result.status).toBe(0);
    // Exactly one "Respawned:" line, regardless of whether the new
    // daemon has also exited by the time gc returns.
    const respawnLines = (result.stdout.match(/^Respawned: /gm) || []).length;
    expect(respawnLines).toBe(1);

    // Track whatever pid we ended up with so afterEach can clean up.
    try {
      const pid = parseInt(fs.readFileSync(path.join(dir, `${name}.pid`), "utf-8").trim(), 10);
      if (Number.isFinite(pid)) bgPids.push(pid);
    } catch {}
  }, 15000);

  it("pty.toml respawn re-reads the toml so command edits take effect", async () => {
    // Seed a pty.toml in a tmp dir and use `pty up` to spawn a permanent
    // session referencing it.
    const dir = makeSessionDir();
    const projectDir = fs.mkdtempSync(path.join(testRoot, "proj-"));
    sessionDirs.push(projectDir);
    const tomlPath = path.join(projectDir, "pty.toml");
    const v1Marker = `/tmp/pty-gc-permv1-${Date.now()}.flag`;
    const v2Marker = `/tmp/pty-gc-permv2-${Date.now()}.flag`;
    // v1: write v1Marker and exit.
    fs.writeFileSync(tomlPath, `[sessions.perm]
command = "touch ${v1Marker}"
tags = { strategy = "permanent" }
`);

    const projectShort = path.basename(projectDir);
    void projectShort;

    const up = runCli(dir, "up", projectDir);
    expect(up.status).toBe(0);
    // Under the decoupled name/displayName model, the session's on-disk
    // name is a random id; the toml-derived "perm" is the displayName.
    // Resolve the actual on-disk name via `pty list --json`.
    const listOut = runCli(dir, "list", "--json").stdout;
    const sessions = JSON.parse(listOut);
    const found = sessions.find((s: any) => s.displayName === "perm");
    expect(found).toBeDefined();
    const sessionName: string = found.name;

    // Wait for first run to complete (it touches v1Marker and exits).
    const start = Date.now();
    while (Date.now() - start < 5000) {
      if (fs.existsSync(v1Marker)) break;
      await new Promise((r) => setTimeout(r, 100));
    }
    expect(fs.existsSync(v1Marker)).toBe(true);
    fs.unlinkSync(v1Marker);

    // Now wait for the daemon to write its exit record.
    await new Promise((r) => setTimeout(r, 800));
    const meta = readMeta(dir, sessionName);
    expect(meta.exitedAt).toBeTruthy();
    expect(meta.tags?.ptyfile).toBe(tomlPath);

    // Edit the toml to write v2Marker instead.
    fs.writeFileSync(tomlPath, `[sessions.perm]
command = "touch ${v2Marker}"
tags = { strategy = "permanent" }
`);

    const result = runCli(dir, "gc");
    expect(result.status).toBe(0);
    expect(result.stdout).toContain(`Respawned: ${sessionName} (pty.toml re-read)`);

    // The respawned session should write v2Marker.
    const start2 = Date.now();
    while (Date.now() - start2 < 5000) {
      if (fs.existsSync(v2Marker)) break;
      await new Promise((r) => setTimeout(r, 100));
    }
    expect(fs.existsSync(v2Marker)).toBe(true);
    try { fs.unlinkSync(v2Marker); } catch {}

    // Track the new pid for teardown.
    try {
      const pid = parseInt(fs.readFileSync(path.join(dir, `${sessionName}.pid`), "utf-8").trim(), 10);
      if (Number.isFinite(pid)) bgPids.push(pid);
    } catch {}
  }, 25000);
});
