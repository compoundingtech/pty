import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const serverModule = path.join(__dirname, "..", "dist", "server.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-exit-"));
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

/** Spawn the daemon as a real subprocess and return the child + an exit promise. */
function startDaemonSubprocess(sessionDir: string, name: string, command: string, args: string[] = []) {
  const config = JSON.stringify({
    name, command, args, displayCommand: command,
    cwd: os.tmpdir(), rows: 24, cols: 80,
  });
  const child = spawn(nodeBin, [serverModule], {
    detached: true,
    stdio: ["ignore", "ignore", "pipe"],
    env: { ...process.env, PTY_SERVER_CONFIG: config, PTY_SESSION_DIR: sessionDir },
  });
  bgPids.push(child.pid!);
  let stderr = "";
  child.stderr?.on("data", (d: Buffer) => { stderr += d.toString(); });
  child.unref();
  (child.stderr as any)?.unref?.();

  const exitPromise = new Promise<number | null>((resolve) => {
    child.on("exit", (code) => resolve(code));
  });
  return { child, exitPromise, getStderr: () => stderr };
}

async function waitForSocket(sessionDir: string, name: string, timeoutMs = 5000): Promise<void> {
  const socketPath = path.join(sessionDir, `${name}.sock`);
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    try { fs.statSync(socketPath); return; } catch {}
    await new Promise((r) => setTimeout(r, 50));
  }
  throw new Error(`Timeout waiting for socket ${socketPath}`);
}

function readEvents(sessionDir: string, name: string): any[] {
  const eventsPath = path.join(sessionDir, `${name}.events.jsonl`);
  try {
    const content = fs.readFileSync(eventsPath, "utf-8");
    return content.trim().split("\n").filter(Boolean).map((l) => JSON.parse(l));
  } catch {
    return [];
  }
}

describe("session_exit event is flushed to disk before daemon exits", () => {
  it("captures session_exit when child exits naturally (short-lived process)", async () => {
    const dir = makeSessionDir();
    const name = `x${Math.random().toString(36).slice(2, 6)}`;

    // `true` exits immediately, triggering the daemon's onExit path.
    const { exitPromise } = startDaemonSubprocess(dir, name, "true");

    await waitForSocket(dir, name);
    // Wait for the daemon to fully exit (this is what pty kill races against).
    await exitPromise;

    const events = readEvents(dir, name);
    const exits = events.filter((e) => e.type === "session_exit");
    expect(exits.length).toBe(1);
    expect(typeof exits[0].exitCode).toBe("number");
  }, 15000);

  it("captures session_exit when daemon is killed via SIGTERM", async () => {
    const dir = makeSessionDir();
    const name = `k${Math.random().toString(36).slice(2, 6)}`;

    // `sh -c 'sleep 30'` so the daemon stays alive until we SIGTERM it — mimics `pty kill`.
    const { child, exitPromise, getStderr } = startDaemonSubprocess(dir, name, "/bin/sh", ["-c", "sleep 30"]);

    try { await waitForSocket(dir, name); }
    catch (e) {
      throw new Error(`${(e as Error).message}\nDaemon stderr:\n${getStderr()}`);
    }
    // Give the daemon a moment to be fully ready
    await new Promise((r) => setTimeout(r, 200));

    // Kill the daemon (like `pty kill` does)
    try { process.kill(child.pid!, "SIGTERM"); } catch {}
    await exitPromise;

    const events = readEvents(dir, name);
    const exits = events.filter((e) => e.type === "session_exit");
    expect(exits.length).toBe(1);
  }, 15000);

  it("session_start is always present (watchFile offset 0 for new files)", async () => {
    const dir = makeSessionDir();
    const name = `s${Math.random().toString(36).slice(2, 6)}`;

    const { exitPromise } = startDaemonSubprocess(dir, name, "true");
    await waitForSocket(dir, name);
    await exitPromise;

    const events = readEvents(dir, name);
    const starts = events.filter((e) => e.type === "session_start");
    expect(starts.length).toBe(1);
  }, 15000);
});
