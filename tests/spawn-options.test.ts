import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, spawnSync } from "node:child_process";
import { queryStats } from "../src/client.ts";
import { spawnDaemon } from "../src/spawn.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const serverModule = path.join(__dirname, "..", "dist", "server.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-spawn-opts-"));
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
  return `spawn-opt${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
}

async function startDaemonWithSize(
  sessionDir: string,
  name: string,
  command: string,
  args: string[] = [],
  rows = 24,
  cols = 80,
): Promise<number> {
  const config = JSON.stringify({
    name,
    command,
    args,
    displayCommand: command,
    cwd: os.tmpdir(),
    rows,
    cols,
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

async function startDaemonExpectFailure(
  sessionDir: string,
  name: string,
  cwd: string,
  command = "cat",
  args: string[] = [],
): Promise<{ exitCode: number | null; stderr: string }> {
  const config = JSON.stringify({
    name,
    command,
    args,
    displayCommand: command,
    cwd,
    rows: 24,
    cols: 80,
  });

  const child = spawn(nodeBin, [serverModule], {
    stdio: ["ignore", "ignore", "pipe"],
    env: {
      ...process.env,
      PTY_SERVER_CONFIG: config,
      PTY_SESSION_DIR: sessionDir,
    },
  });

  let stderr = "";
  child.stderr?.on("data", (d: Buffer) => { stderr += d.toString(); });

  const exitCode = await new Promise<number | null>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("Timed out waiting for daemon failure")), 5000);
    child.on("exit", (code) => {
      clearTimeout(timer);
      resolve(code);
    });
  });

  return { exitCode, stderr };
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

describe("spawnDaemon options", () => {
  it("CLI passes through to spawnDaemon with options object", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();

    // Use CLI to spawn (exercises spawnDaemon with options object internally)
    const result = spawnSync(nodeBin, [cliPath, "run", "-d", "--name", name, "--", "cat"], {
      env: { ...process.env, PTY_SESSION_DIR: dir },
      encoding: "utf-8",
      timeout: 10000,
    });

    expect(result.status).toBe(0);

    // Verify session is running
    process.env.PTY_SESSION_DIR = dir;
    const stats = await queryStats(name);
    expect(stats.name).toBe(name);
    expect(stats.process.alive).toBe(true);

    // Clean up
    const pidFile = path.join(dir, `${name}.pid`);
    const pid = parseInt(fs.readFileSync(pidFile, "utf-8").trim(), 10);
    bgPids.push(pid);
  }, 15000);

  it("custom rows and cols are applied via server config", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();

    // Start daemon directly with custom dimensions
    await startDaemonWithSize(dir, name, "cat", [], 40, 120);

    process.env.PTY_SESSION_DIR = dir;
    const stats = await queryStats(name);
    expect(stats.terminal.rows).toBe(40);
    expect(stats.terminal.cols).toBe(120);
  }, 15000);

  it("default rows and cols are reasonable", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();

    // Start daemon with default dimensions
    await startDaemonWithSize(dir, name, "cat");

    process.env.PTY_SESSION_DIR = dir;
    const stats = await queryStats(name);
    expect(stats.terminal.rows).toBe(24);
    expect(stats.terminal.cols).toBe(80);
  }, 15000);

  it("launcher override routes the daemon through the given command", async () => {
    const dir = makeSessionDir();
    const argFile = path.join(dir, "launcher-args.txt");
    const launcherScript = path.join(dir, "launcher.sh");
    fs.writeFileSync(launcherScript, `#!/bin/sh\nprintf '%s\\n' "$@" > "${argFile}"\nexit 0\n`);
    fs.chmodSync(launcherScript, 0o755);

    process.env.PTY_SESSION_DIR = dir;
    const name = uniqueName();

    // The stub launcher exits without starting a real daemon, so spawnDaemon
    // will reject on socket timeout — we only care that the launcher was invoked.
    await expect(spawnDaemon({
      name,
      command: "cat",
      args: [],
      displayCommand: "cat",
      cwd: dir,
      launcher: { command: launcherScript, args: ["--prelude"] },
    })).rejects.toThrow();

    const recorded = fs.readFileSync(argFile, "utf-8").split("\n").filter(Boolean);
    expect(recorded[0]).toBe("--prelude");
    expect(recorded[1]).toMatch(/server\.js$/);
  }, 15000);

  it("--isolate-env scrubs inherited environment variables from the session child (BUG-4)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();

    // Run the child with a command that prints its env, set a secret var that
    // should NOT propagate to the isolated child. `pty run -d` spawns the
    // session, then we peek.
    const secret = "pty_isolated_test_secret_must_not_leak";

    const runResult = spawnSync(nodeBin, [
      cliPath, "run", "-d", "--name", name, "--isolate-env",
      "--", "sh", "-c", "env > /tmp/pty-iso-env.txt; sleep 30",
    ], {
      env: { ...process.env, PTY_SESSION_DIR: dir, PTY_SECRET_TEST: secret },
      encoding: "utf-8",
      timeout: 10000,
    });
    expect(runResult.status).toBe(0);

    // Give the child a tick to write.
    await new Promise((r) => setTimeout(r, 500));

    const dumped = fs.readFileSync("/tmp/pty-iso-env.txt", "utf-8");
    expect(dumped).not.toContain("PTY_SECRET_TEST");
    expect(dumped).toContain("PATH="); // PATH still propagates
    expect(dumped).toContain(`PTY_SESSION=${name}`); // set unconditionally

    // Clean up
    const pidFile = path.join(dir, `${name}.pid`);
    try {
      const pid = parseInt(fs.readFileSync(pidFile, "utf-8").trim(), 10);
      bgPids.push(pid);
    } catch {}
    try { fs.unlinkSync("/tmp/pty-iso-env.txt"); } catch {}
  }, 15000);

  it("without --isolate-env, custom env vars propagate to the session child (legacy behaviour)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();

    const marker = "pty_legacy_env_test_marker";
    const runResult = spawnSync(nodeBin, [
      cliPath, "run", "-d", "--name", name,
      "--", "sh", "-c", "env > /tmp/pty-legacy-env.txt; sleep 30",
    ], {
      env: { ...process.env, PTY_SESSION_DIR: dir, PTY_LEGACY_MARKER: marker },
      encoding: "utf-8",
      timeout: 10000,
    });
    expect(runResult.status).toBe(0);
    await new Promise((r) => setTimeout(r, 500));

    const dumped = fs.readFileSync("/tmp/pty-legacy-env.txt", "utf-8");
    expect(dumped).toContain(`PTY_LEGACY_MARKER=${marker}`);

    const pidFile = path.join(dir, `${name}.pid`);
    try {
      const pid = parseInt(fs.readFileSync(pidFile, "utf-8").trim(), 10);
      bgPids.push(pid);
    } catch {}
    try { fs.unlinkSync("/tmp/pty-legacy-env.txt"); } catch {}
  }, 15000);

  it("surfaces a missing cwd explicitly instead of failing silently", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const missingDir = path.join(testRoot, `missing-${name}`);

    const result = await startDaemonExpectFailure(dir, name, missingDir);

    expect(result.exitCode).toBe(1);
    expect(result.stderr).toContain(`Working directory does not exist: ${missingDir}`);
    expect(result.stderr).toContain(`Cannot start session "${name}"`);
  }, 15000);

  it("surfaces a non-directory cwd explicitly instead of reporting posix_spawnp", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const filePath = path.join(testRoot, `file-${name}`);
    fs.writeFileSync(filePath, "not a directory");

    const result = await startDaemonExpectFailure(dir, name, filePath);

    expect(result.exitCode).toBe(1);
    expect(result.stderr).toContain(`Working directory is not a directory: ${filePath}`);
    expect(result.stderr).not.toContain("posix_spawnp failed");
  }, 15000);

  it("non-interactive CLI commands still work when the caller cwd was deleted", () => {
    const dir = makeSessionDir();
    const deletedCwd = fs.mkdtempSync(path.join(testRoot, "deleted-cwd-"));
    const script = `cd ${JSON.stringify(deletedCwd)} && rmdir ${JSON.stringify(deletedCwd)} && exec ${JSON.stringify(nodeBin)} ${JSON.stringify(cliPath)} list`;

    const result = spawnSync("sh", ["-lc", script], {
      env: { ...process.env, PTY_SESSION_DIR: dir },
      encoding: "utf-8",
      timeout: 10000,
    });

    expect(result.status).toBe(0);
    expect(result.stderr).not.toContain("uv_cwd");
  }, 15000);
});
