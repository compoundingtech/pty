import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, spawnSync, execFileSync } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const serverModule = path.join(__dirname, "..", "dist", "server.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-nesting-"));
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
  return `nest${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
}

async function startDaemon(
  sessionDir: string,
  name: string,
  command: string,
  args: string[] = [],
): Promise<number> {
  const config = JSON.stringify({
    name,
    command,
    args,
    displayCommand: command,
    cwd: os.tmpdir(),
    rows: 24,
    cols: 80,
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
  return execFileSync(nodeBin, [cliPath, ...args], {
    env: { ...process.env, PTY_SESSION_DIR: sessionDir },
    encoding: "utf-8",
    timeout: 10000,
  });
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

describe("pty nesting prevention", () => {
  it("sets PTY_SESSION in child process environment", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    // Use cat to keep the process alive after printing the env var
    await startDaemon(dir, name, "sh", ["-c", "echo PTY_SESSION=$PTY_SESSION; exec cat"]);

    // Wait for the echo to produce output
    await new Promise((r) => setTimeout(r, 500));

    const output = runCli(dir, "peek", "--plain", name);
    expect(output).toContain(`PTY_SESSION=${name}`);
  }, 15000);

  it("detects nesting and runs command directly", () => {
    const dir = makeSessionDir();
    const result = spawnSync(nodeBin, [cliPath, "run", "--", "echo", "hello"], {
      env: { ...process.env, PTY_SESSION_DIR: dir, PTY_SESSION: "outer-session" },
      encoding: "utf-8",
      timeout: 10000,
    });

    expect(result.stdout).toContain("hello");
    expect(result.stderr).toContain("Already inside pty session");
    expect(result.stderr).toContain("outer-session");
    expect(result.status).toBe(0);

    // No session should have been created
    const sessions = runCli(dir, "list", "--json");
    expect(JSON.parse(sessions)).toEqual([]);
  }, 15000);

  it("detects nesting with -a flag (wrap script path)", () => {
    const dir = makeSessionDir();
    const result = spawnSync(nodeBin, [cliPath, "run", "-a", "--", "echo", "wrapped"], {
      env: { ...process.env, PTY_SESSION_DIR: dir, PTY_SESSION: "outer-session" },
      encoding: "utf-8",
      timeout: 10000,
    });

    expect(result.stdout).toContain("wrapped");
    expect(result.stderr).toContain("Already inside pty session");
    expect(result.status).toBe(0);

    // No session should have been created
    const sessions = runCli(dir, "list", "--json");
    expect(JSON.parse(sessions)).toEqual([]);
  }, 15000);

  it("-d flag bypasses nesting check", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const result = spawnSync(nodeBin, [cliPath, "run", "-d", "--id", name, "--", "cat"], {
      env: { ...process.env, PTY_SESSION_DIR: dir, PTY_SESSION: "outer-session" },
      encoding: "utf-8",
      timeout: 10000,
    });

    expect(result.status).toBe(0);
    expect(result.stderr).not.toContain("Already inside pty session");

    // Session should have been created
    const sessions = JSON.parse(runCli(dir, "list", "--json"));
    const found = sessions.find((s: any) => s.name === name);
    expect(found).toBeDefined();
    expect(found.status).toBe("running");

    // Clean up the daemon
    try { process.kill(found.pid, "SIGTERM"); } catch {}
  }, 15000);

  it("propagates exit code from nested command", () => {
    const dir = makeSessionDir();
    const result = spawnSync(nodeBin, [cliPath, "run", "--", "sh", "-c", "exit 42"], {
      env: { ...process.env, PTY_SESSION_DIR: dir, PTY_SESSION: "outer-session" },
      encoding: "utf-8",
      timeout: 10000,
    });

    expect(result.status).toBe(42);
    expect(result.stderr).toContain("Already inside pty session");
  }, 15000);

  it("does not check for nesting when PTY_SESSION is not set", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();

    // Remove PTY_SESSION from env explicitly
    const env = { ...process.env, PTY_SESSION_DIR: dir };
    delete env.PTY_SESSION;

    const result = spawnSync(nodeBin, [cliPath, "run", "-d", "--id", name, "--", "cat"], {
      env,
      encoding: "utf-8",
      timeout: 10000,
    });

    expect(result.status).toBe(0);
    expect(result.stderr).not.toContain("Already inside pty session");

    // Session should have been created normally
    const sessions = JSON.parse(runCli(dir, "list", "--json"));
    const found = sessions.find((s: any) => s.name === name);
    expect(found).toBeDefined();

    // Clean up the daemon
    try { process.kill(found.pid, "SIGTERM"); } catch {}
  }, 15000);
});
