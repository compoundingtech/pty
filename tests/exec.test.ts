import { describe, it, expect, afterEach, afterAll } from "vitest";
import { terminateAndWait } from "./setup/processes.ts";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, spawnSync } from "node:child_process";
import { readRecentEvents } from "../src/events.ts";
import { acquireLock, releaseLock } from "../src/sessions.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const serverModule = path.join(__dirname, "..", "dist", "server.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-exec-"));
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
  return `ex${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
}

async function startDaemon(
  sessionDir: string,
  name: string,
  command: string,
  args: string[] = [],
  tags?: Record<string, string>,
): Promise<number> {
  const config = JSON.stringify({
    name, command, args, displayCommand: [command, ...args].join(" "),
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

function readMeta(sessionDir: string, name: string): any {
  try {
    return JSON.parse(fs.readFileSync(path.join(sessionDir, `${name}.json`), "utf-8"));
  } catch { return null; }
}

function readEvents(sessionDir: string, name: string): any[] {
  try {
    return fs.readFileSync(path.join(sessionDir, `${name}.events.jsonl`), "utf-8")
      .trimEnd()
      .split("\n")
      .filter(Boolean)
      .map((line) => JSON.parse(line));
  } catch {
    return [];
  }
}

function execEnv(sessionDir: string, name: string, generation?: string): NodeJS.ProcessEnv {
  return {
    ...process.env,
    PTY_SESSION_DIR: sessionDir,
    PTY_SESSION: name,
    PTY_SESSION_GENERATION: generation ?? readMeta(sessionDir, name).generation,
  };
}

afterEach(async () => {
  await terminateAndWait(bgPids);
  bgPids = [];
  for (const dir of sessionDirs) {
    try {
      for (const e of fs.readdirSync(dir)) { try { fs.unlinkSync(path.join(dir, e)); } catch {} }
    } catch {}
  }
  sessionDirs = [];
});

describe("pty exec", () => {
  it("updates metadata and runs the new command", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "bash");

    // Run pty exec inside the session (simulate via PTY_SESSION env var)
    const result = spawnSync(nodeBin, [cliPath, "exec", "--", "echo", "hello-from-exec"], {
      env: execEnv(dir, name),
      encoding: "utf-8",
      timeout: 10000,
    });

    expect(result.status).toBe(0);
    expect(result.stdout).toContain("hello-from-exec");

    // Metadata should be updated
    const meta = readMeta(dir, name);
    expect(meta.displayCommand).toBe("echo hello-from-exec");
    expect(meta.args).toEqual(["hello-from-exec"]);
  }, 15000);

  it("errors when not inside a pty session", () => {
    const dir = makeSessionDir();
    const env: Record<string, string | undefined> = { ...process.env, PTY_SESSION_DIR: dir };
    delete env.PTY_SESSION;

    const result = spawnSync(nodeBin, [cliPath, "exec", "--", "echo", "hi"], {
      env,
      encoding: "utf-8",
      timeout: 10000,
    });

    expect(result.status).not.toBe(0);
    expect(result.stderr).toContain("not inside a pty session");
  }, 15000);

  it("errors on toml-managed sessions", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "bash", [], {
      ptyfile: "/some/path/pty.toml",
      "ptyfile.session": "test",
    });

    const result = spawnSync(nodeBin, [cliPath, "exec", "--", "echo", "hi"], {
      env: execEnv(dir, name),
      encoding: "utf-8",
      timeout: 10000,
    });

    expect(result.status).not.toBe(0);
    expect(result.stderr).toContain("pty.toml");
  }, 15000);

  it("preserves existing tags when updating metadata", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "bash", [], { role: "dev", strategy: "permanent" });

    spawnSync(nodeBin, [cliPath, "exec", "--", "echo", "tagged"], {
      env: execEnv(dir, name),
      encoding: "utf-8",
      timeout: 10000,
    });

    const meta = readMeta(dir, name);
    expect(meta.displayCommand).toBe("echo tagged");
    expect(meta.tags.role).toBe("dev");
    expect(meta.tags.strategy).toBe("permanent");
  }, 15000);

  it("propagates exit code from the exec'd command", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "bash");

    const result = spawnSync(nodeBin, [cliPath, "exec", "--", "sh", "-c", "exit 42"], {
      env: execEnv(dir, name),
      encoding: "utf-8",
      timeout: 10000,
    });

    expect(result.status).toBe(42);
  }, 15000);

  it("errors when no command provided", () => {
    const result = spawnSync(nodeBin, [cliPath, "exec"], {
      env: { ...process.env, PTY_SESSION: "test" },
      encoding: "utf-8",
      timeout: 10000,
    });

    expect(result.status).not.toBe(0);
    expect(result.stderr).toContain("Usage");
  }, 15000);

  it("emits session_exec event", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "bash");

    process.env.PTY_SESSION_DIR = dir;
    spawnSync(nodeBin, [cliPath, "exec", "--", "echo", "swapped"], {
      env: execEnv(dir, name),
      encoding: "utf-8",
      timeout: 10000,
    });

    const events = readRecentEvents(name);
    const execEvents = events.filter((e: any) => e.type === "session_exec");
    expect(execEvents.length).toBeGreaterThanOrEqual(1);
    const ev = execEvents[execEvents.length - 1] as any;
    expect(ev.session).toBe(name);
    expect(ev.command).toBe("echo swapped");
    expect(ev.previousCommand).toBeDefined();
  }, 15000);

  it("errors on nonexistent command", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "bash");

    const result = spawnSync(nodeBin, [cliPath, "exec", "--", "/nonexistent/cmd"], {
      env: execEnv(dir, name),
      encoding: "utf-8",
      timeout: 10000,
    });

    expect(result.status).not.toBe(0);
    expect(result.stderr).toContain("not found");
  }, 15000);

  it("does not overwrite a concurrent metadata writer while its lock is held", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "bash", [], { role: "original" });
    const previousRoot = process.env.PTY_SESSION_DIR;
    process.env.PTY_SESSION_DIR = dir;
    expect(acquireLock(name)).toBe(true);
    try {
      const current = readMeta(dir, name);
      fs.writeFileSync(path.join(dir, `${name}.json`), JSON.stringify({
        ...current,
        displayName: "concurrent-label",
        tags: { ...current.tags, role: "concurrent" },
      }));
      const result = spawnSync(nodeBin, [cliPath, "exec", "--", "echo", "must-not-run"], {
        env: execEnv(dir, name),
        encoding: "utf-8",
        timeout: 10000,
      });
      expect(result.status).not.toBe(0);
      expect(result.stderr).toContain("busy");
      expect(readMeta(dir, name)).toMatchObject({
        displayName: "concurrent-label",
        tags: { role: "concurrent" },
        displayCommand: "bash",
      });
    } finally {
      releaseLock(name);
      if (previousRoot === undefined) delete process.env.PTY_SESSION_DIR;
      else process.env.PTY_SESSION_DIR = previousRoot;
    }
  }, 15000);

  it("refuses to mutate or execute against a replacement generation", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "bash", [], { role: "old" });
    const oldGeneration = readMeta(dir, name).generation;
    const marker = path.join(dir, "replacement-race-marker");
    const replacement = {
      ...readMeta(dir, name),
      generation: "replacement-generation",
      command: "/bin/sh",
      args: ["-c", "sleep 30"],
      displayCommand: "replacement command",
      displayName: "replacement label",
      tags: { role: "replacement" },
    };
    fs.writeFileSync(path.join(dir, `${name}.json`), JSON.stringify(replacement));

    const result = spawnSync(
      nodeBin,
      [cliPath, "exec", "--", "/bin/sh", "-c", `touch ${JSON.stringify(marker)}`],
      { env: execEnv(dir, name, oldGeneration), encoding: "utf-8", timeout: 10000 },
    );
    expect(result.status).not.toBe(0);
    expect(result.stderr).toContain("replacement generation");
    expect(fs.existsSync(marker)).toBe(false);
    expect(readMeta(dir, name)).toEqual(replacement);
    expect(readEvents(dir, name).filter((event) => event.type === "session_exec")).toHaveLength(0);
  }, 15000);
});
