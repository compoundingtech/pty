import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn } from "node:child_process";
import {
  SessionConnection,
  sendData,
  peekScreen,
} from "../src/connection.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const serverModule = path.join(__dirname, "..", "dist", "server.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-conn-"));
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
  return `conn${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
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

describe("SessionConnection", () => {
  it("connects and receives initial screen", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    process.env.PTY_SESSION_DIR = dir;
    await startDaemon(dir, name, "sh", ["-c", "echo hello-screen; exec cat"]);
    await new Promise((r) => setTimeout(r, 300));

    const conn = new SessionConnection({ name, rows: 24, cols: 80 });
    const screen = await conn.connect();

    expect(screen).toContain("hello-screen");
    expect(conn.connected).toBe(true);
    conn.disconnect();
    expect(conn.connected).toBe(false);
  }, 15000);

  it("receives data events after connect", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    process.env.PTY_SESSION_DIR = dir;
    await startDaemon(dir, name, "cat");

    const conn = new SessionConnection({ name, rows: 24, cols: 80 });
    await conn.connect();

    const received: string[] = [];
    conn.on("data", (data: string) => { received.push(data); });

    conn.write("test-input");
    await new Promise((r) => setTimeout(r, 300));

    expect(received.join("")).toContain("test-input");
    conn.disconnect();
  }, 15000);

  it("press() sends named keys", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    process.env.PTY_SESSION_DIR = dir;
    await startDaemon(dir, name, "cat");

    const conn = new SessionConnection({ name, rows: 24, cols: 80 });
    await conn.connect();

    const received: string[] = [];
    conn.on("data", (data: string) => { received.push(data); });

    // cat echoes everything, including the return key as a newline
    conn.write("hello");
    conn.press("return");
    await new Promise((r) => setTimeout(r, 300));

    const output = received.join("");
    expect(output).toContain("hello");
    expect(output).toContain("\r");
    conn.disconnect();
  }, 15000);

  it("emits exit when process exits", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    process.env.PTY_SESSION_DIR = dir;
    // Process that exits after a short delay
    await startDaemon(dir, name, "sh", ["-c", "sleep 0.2; exit 7"]);

    const conn = new SessionConnection({ name, rows: 24, cols: 80 });
    await conn.connect();

    const exitCode = await new Promise<number>((resolve) => {
      conn.on("exit", resolve);
    });

    expect(exitCode).toBe(7);
    conn.disconnect();
  }, 15000);

  it("rejects on nonexistent session", async () => {
    const dir = makeSessionDir();
    process.env.PTY_SESSION_DIR = dir;

    const conn = new SessionConnection({ name: "nonexistent", rows: 24, cols: 80 });
    // Suppress the 'error' event that fires after the promise rejects
    conn.on("error", () => {});
    await expect(conn.connect()).rejects.toThrow("not found or not running");
  }, 15000);

  it("resize sends new dimensions", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    process.env.PTY_SESSION_DIR = dir;
    await startDaemon(dir, name, "cat");

    const conn = new SessionConnection({ name, rows: 24, cols: 80 });
    await conn.connect();

    // Should not throw
    conn.resize(30, 100);
    await new Promise((r) => setTimeout(r, 100));

    conn.disconnect();
  }, 15000);
});

describe("sendData", () => {
  it("sends text to a session and resolves", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    process.env.PTY_SESSION_DIR = dir;
    await startDaemon(dir, name, "cat");

    await sendData({ name, data: ["hello-send"] });

    // Verify data was received via peek
    await new Promise((r) => setTimeout(r, 200));
    const screen = await peekScreen({ name, plain: true });
    expect(screen).toContain("hello-send");
  }, 15000);

  it("rejects for nonexistent session", async () => {
    const dir = makeSessionDir();
    process.env.PTY_SESSION_DIR = dir;

    await expect(
      sendData({ name: "nonexistent", data: ["test"] })
    ).rejects.toThrow("not found or not running");
  }, 15000);
});

describe("peekScreen", () => {
  it("returns screen content", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    process.env.PTY_SESSION_DIR = dir;
    await startDaemon(dir, name, "sh", ["-c", "echo peek-test; exec cat"]);
    await new Promise((r) => setTimeout(r, 300));

    const screen = await peekScreen({ name });
    expect(screen).toContain("peek-test");
  }, 15000);

  it("returns plain text when plain=true", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    process.env.PTY_SESSION_DIR = dir;
    await startDaemon(dir, name, "sh", ["-c", "echo plain-test; exec cat"]);
    await new Promise((r) => setTimeout(r, 300));

    const screen = await peekScreen({ name, plain: true });
    expect(screen).toContain("plain-test");
    // Plain text should not contain ANSI escape sequences
    expect(screen).not.toMatch(/\x1b\[/);
  }, 15000);

  it("rejects for nonexistent session", async () => {
    const dir = makeSessionDir();
    process.env.PTY_SESSION_DIR = dir;

    await expect(
      peekScreen({ name: "nonexistent" })
    ).rejects.toThrow("not found or not running");
  }, 15000);
});
