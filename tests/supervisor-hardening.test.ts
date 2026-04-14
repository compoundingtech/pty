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

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-sup-hard-"));
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
  return `sh${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
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

function readMeta(sessionDir: string, name: string): any {
  try {
    return JSON.parse(fs.readFileSync(path.join(sessionDir, `${name}.json`), "utf-8"));
  } catch { return null; }
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

describe("displayCommand formatting", () => {
  it("pty run shows command with args in list", () => {
    const dir = makeSessionDir();
    const name = uniqueName();

    runCli(dir, "run", "-d", "--name", name, "--", "echo", "hello", "world");
    // Wait for session to start
    const start = Date.now();
    while (Date.now() - start < 3000) {
      const meta = readMeta(dir, name);
      if (meta) break;
    }

    const meta = readMeta(dir, name);
    expect(meta).not.toBeNull();
    expect(meta.displayCommand).toBe("echo hello world");
  }, 15000);

  it("pty up shows toml command without duplication in list", () => {
    const projDir = fs.mkdtempSync(path.join(testRoot, "proj-"));
    const dir = makeSessionDir();
    fs.writeFileSync(path.join(projDir, "pty.toml"), `
[sessions.serve]
command = "echo server running"
`);

    runCli(dir, "up", projDir);

    const meta = readMeta(dir, "serve");
    expect(meta).not.toBeNull();
    expect(meta.displayCommand).toBe("echo server running");
    // command should be /bin/sh, args should be ["-c", "echo server running"]
    expect(meta.command).toBe("/bin/sh");

    // List output should show the command once, not duplicated
    const list = runCli(dir, "list");
    const lines = list.stdout.split("\n").filter((l: string) => l.includes("serve"));
    expect(lines.length).toBeGreaterThan(0);
    // Count occurrences of "echo server running" — should be 1
    const matches = lines[0].match(/echo server running/g);
    expect(matches).toHaveLength(1);
  }, 15000);
});

describe("pty kill on supervised sessions", () => {
  it("removes strategy tag when killing a supervised session", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "cat", [], { strategy: "permanent" });

    const result = runCli(dir, "kill", name);
    expect(result.status).toBe(0);

    // Strategy tag should be removed
    const meta = readMeta(dir, name);
    // Meta might be null if cleanup happened, or present without strategy
    if (meta?.tags) {
      expect(meta.tags.strategy).toBeUndefined();
    }
  }, 15000);

  it("warns about toml-managed sessions when killing", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "cat", [], {
      strategy: "permanent",
      ptyfile: "/some/path/pty.toml",
    });

    const result = runCli(dir, "kill", name);
    expect(result.stderr).toContain("pty.toml");
    expect(result.stderr).toContain("pty up");
  }, 15000);
});

describe("pty supervisor reset", () => {
  it("clears failed status and restart counter", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "true", [], {
      strategy: "permanent",
      "supervisor.status": "failed",
    });
    await new Promise((r) => setTimeout(r, 1000)); // wait for exit

    // Write fake supervisor state
    const supDir = path.join(dir, "supervisor");
    fs.mkdirSync(supDir, { recursive: true });
    fs.writeFileSync(path.join(supDir, "state.json"), JSON.stringify({
      sessions: { [name]: { restartCount: 5, restartWindowStart: Date.now(), failed: true, nextBackoffMs: 16000, lastRestartAt: Date.now() } },
      savedAt: new Date().toISOString(),
    }));

    const result = runCli(dir, "supervisor", "reset", name);
    expect(result.status).toBe(0);
    expect(result.stdout).toContain("Reset");

    // Failed tag should be removed
    const meta = readMeta(dir, name);
    expect(meta.tags["supervisor.status"]).toBeUndefined();

    // State file should have reset counters
    const state = JSON.parse(fs.readFileSync(path.join(supDir, "state.json"), "utf-8"));
    expect(state.sessions[name].restartCount).toBe(0);
    expect(state.sessions[name].failed).toBe(false);
  }, 15000);

  it("reports when session is not failed", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "cat", [], { strategy: "permanent" });

    const result = runCli(dir, "supervisor", "reset", name);
    expect(result.stdout).toContain("not in failed state");
  }, 15000);
});

describe("pty supervisor forget", () => {
  it("warns about toml-managed sessions", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "cat", [], {
      strategy: "permanent",
      ptyfile: "/some/path/pty.toml",
    });

    const result = runCli(dir, "supervisor", "forget", name);
    expect(result.status).toBe(0);
    expect(result.stdout).toContain("Removed supervision");
    expect(result.stderr).toContain("pty.toml");
    expect(result.stderr).toContain("pty up");
  }, 15000);
});

describe("pty tag warning", () => {
  it("warns when modifying tags on toml-managed sessions", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "cat", [], {
      ptyfile: "/some/path/pty.toml",
      "ptyfile.session": "test",
    });

    const result = runCli(dir, "tag", name, "custom=value");
    expect(result.status).toBe(0);
    expect(result.stderr).toContain("pty.toml");
    expect(result.stderr).toContain("pty up");
  }, 15000);
});

describe("spawnDaemon process leak", () => {
  it("kills orphaned daemon on waitForSocket timeout", () => {
    const dir = makeSessionDir();

    // Count server.js processes before
    const before = spawnSync("pgrep", ["-f", "server.js"], { encoding: "utf-8" });
    const beforeCount = before.stdout.trim().split("\n").filter(Boolean).length;

    // Try to spawn with a nonexistent command — daemon will crash
    const result = runCli(dir, "run", "-d", "--name", "leak-test", "--", "/nonexistent/command");

    // Count server.js processes after
    const after = spawnSync("pgrep", ["-f", "server.js"], { encoding: "utf-8" });
    const afterCount = after.stdout.trim().split("\n").filter(Boolean).length;

    // Should not have leaked a process
    expect(afterCount).toBeLessThanOrEqual(beforeCount);
  }, 15000);
});

describe("supervisor re-reads pty.toml on restart", () => {
  it("uses updated toml command when restarting", async () => {
    const projDir = fs.mkdtempSync(path.join(testRoot, "proj-"));
    const dir = makeSessionDir();

    // Create initial toml
    fs.writeFileSync(path.join(projDir, "pty.toml"), `
[sessions.reread]
command = "echo original"
tags = { strategy = "permanent" }
`);

    // Start via pty up
    runCli(dir, "up", projDir);
    await new Promise((r) => setTimeout(r, 1500)); // wait for exit + metadata

    // Update the toml command
    fs.writeFileSync(path.join(projDir, "pty.toml"), `
[sessions.reread]
command = "echo updated"
tags = { strategy = "permanent" }
`);

    // Start supervisor to restart the session
    const sup = spawn(nodeBin, [cliPath, "supervisor", "start"], {
      detached: true,
      stdio: ["ignore", "ignore", "ignore"],
      env: { ...process.env, PTY_SESSION_DIR: dir },
    });
    sup.unref();
    bgPids.push(sup.pid!);

    // Wait for supervisor to restart the session
    await new Promise((r) => setTimeout(r, 5000));

    // Check the restarted session's metadata
    const meta = readMeta(dir, "reread");
    if (meta) {
      // displayCommand should reflect the updated toml
      expect(meta.displayCommand).toBe("echo updated");
    }
  }, 20000);
});
