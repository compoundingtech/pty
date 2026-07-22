// Concurrency robustness: `pty list` must not destroy a LIVE session when its
// pidfile is transiently unreadable.
//
// The daemon creates its .sock (listen) BEFORE it writes its .pid, and the
// plain pidfile write can be caught mid-flight, so under concurrent multi-agent
// load a `pty list` can momentarily read a null pid for a perfectly healthy
// session. The old listSessions treated "can't read pid" as "process dead" and
// ran cleanupSocket — deleting the live daemon's socket/pid out from under it,
// making it invisible and getting it GC'd + re-launched by consumers that
// reconcile on not-running. Destruction must require POSITIVE proof of death;
// a reachable control socket alone proves the daemon is alive.

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

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-listrace-"));
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

async function startDaemon(sessionDir: string, name: string): Promise<number> {
  const config = JSON.stringify({
    name, command: "cat", args: [], displayCommand: "cat",
    cwd: os.tmpdir(), rows: 24, cols: 80,
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
  throw new Error(`Timeout waiting for daemon socket: ${socketPath}`);
}

function runCli(sessionDir: string, args: string[]) {
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

describe("pty list: concurrency robustness (do not reap a live session on a transient pid read)", () => {
  it("keeps a live session running when its pidfile is transiently missing", async () => {
    const dir = makeSessionDir();
    const name = "live-norace";
    await startDaemon(dir, name);

    // Simulate the startup / concurrent-load window: the pidfile is momentarily
    // absent while the daemon (and its listening socket) is fully alive.
    fs.unlinkSync(path.join(dir, `${name}.pid`));

    const list = JSON.parse(runCli(dir, ["list", "--json"]).stdout);
    const found = list.find((s: any) => s.name === name);
    // The live session must still be reported — and as running, not dropped.
    expect(found).toBeDefined();
    expect(found.status).toBe("running");
    // Critically: its control socket must NOT have been reaped. Deleting it
    // would make the still-alive daemon invisible + get it re-launched.
    expect(fs.existsSync(path.join(dir, `${name}.sock`))).toBe(true);
  }, 20000);

  it("still reaps a genuinely dead session's stale socket (positive proof of death)", async () => {
    const dir = makeSessionDir();
    const name = "dead-reap";
    const pid = await startDaemon(dir, name);

    // Kill the daemon hard so it leaves a stale socket + a readable (but dead)
    // pidfile — the positive-proof-of-death case that SHOULD still be reaped.
    process.kill(pid, "SIGKILL");
    // Wait for the process to actually leave the table.
    const start = Date.now();
    while (Date.now() - start < 5000) {
      try { process.kill(pid, 0); } catch { break; }
      await new Promise((r) => setTimeout(r, 50));
    }

    // First list has positive proof of death (readable dead pid + unreachable
    // socket) → it reaps the stale socket/pid.
    runCli(dir, ["list", "--json"]);
    expect(fs.existsSync(path.join(dir, `${name}.sock`))).toBe(false);

    // The session stays addressable as vanished (a SIGKILLed daemon wrote no
    // exit record) once only its metadata remains.
    const list2 = JSON.parse(runCli(dir, ["list", "--json"]).stdout);
    const found = list2.find((s: any) => s.name === name);
    expect(found).toBeDefined();
    expect(found.status).toBe("vanished");
  }, 20000);
});
