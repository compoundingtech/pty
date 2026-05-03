// Verifies that bin/pty forwards SIGTERM/SIGINT to the inner cli.js child.
// Without forwarding, systemd's KillMode=process leaves the inner supervisor
// orphaned and still holding supervisor.lock — the new unit invocation then
// fails with "another supervisor is already running".

import { describe, it, expect, afterEach } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const wrapperPath = path.join(__dirname, "..", "bin", "pty");
const nodeBin = process.execPath;

const sessionDirs: string[] = [];
const trackedPids: number[] = [];
afterEach(() => {
  // Belt-and-braces: if a test failed mid-way, kill any inner cli.js it left
  // behind so the next run isn't poisoned by an orphan holding the lock.
  for (const pid of trackedPids) {
    try { process.kill(pid, "SIGKILL"); } catch {}
  }
  trackedPids.length = 0;
  for (const d of sessionDirs) {
    try { fs.rmSync(d, { recursive: true, force: true }); } catch {}
  }
  sessionDirs.length = 0;
});

function makeSessionDir(): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "pty-wrap-"));
  sessionDirs.push(dir);
  return dir;
}

/** Wait until predicate returns true or `timeoutMs` elapses. */
async function waitFor(predicate: () => boolean, timeoutMs: number): Promise<boolean> {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    if (predicate()) return true;
    await new Promise((r) => setTimeout(r, 25));
  }
  return predicate();
}

function isAlive(pid: number): boolean {
  try { process.kill(pid, 0); return true; } catch { return false; }
}

describe("bin/pty signal forwarding", () => {
  it("propagates SIGTERM to the inner cli.js so the supervisor releases its lock", async () => {
    const sessionDir = makeSessionDir();

    // `supervisor start` is the canonical long-lived command and the one the
    // dev3 regression hit. Its SIGTERM handler calls Supervisor.stop() which
    // releases supervisor.lock — so a clean shutdown leaves no lock file.
    const wrapper = spawn(nodeBin, [wrapperPath, "supervisor", "start"], {
      env: { ...process.env, PTY_SESSION_DIR: sessionDir },
      stdio: ["ignore", "pipe", "pipe"],
    });

    let stdout = "";
    let stderr = "";
    wrapper.stdout?.on("data", (d: Buffer) => { stdout += d.toString(); });
    wrapper.stderr?.on("data", (d: Buffer) => { stderr += d.toString(); });

    let exitCode: number | null = null;
    let exitSignal: NodeJS.Signals | null = null;
    wrapper.on("exit", (c, s) => { exitCode = c; exitSignal = s; });

    // Wait for the supervisor to fully start (it writes supervisor.pid).
    const pidPath = path.join(sessionDir, "supervisor", "supervisor.pid");
    const lockPath = path.join(sessionDir, "supervisor.lock");
    const ready = await waitFor(() => fs.existsSync(pidPath) && fs.existsSync(lockPath), 5000);
    expect(ready, `supervisor never started; stdout=${stdout} stderr=${stderr}`).toBe(true);

    const innerPid = parseInt(fs.readFileSync(pidPath, "utf-8").trim(), 10);
    trackedPids.push(innerPid);
    expect(innerPid).toBeGreaterThan(0);
    expect(innerPid).not.toBe(wrapper.pid);
    expect(isAlive(innerPid)).toBe(true);

    // The actual test: SIGTERM the wrapper, expect the inner cli.js to also die.
    wrapper.kill("SIGTERM");

    const wrapperExited = await waitFor(() => exitCode !== null || exitSignal !== null, 5000);
    expect(wrapperExited, "wrapper did not exit after SIGTERM").toBe(true);

    const innerDied = await waitFor(() => !isAlive(innerPid), 5000);
    expect(innerDied, `inner cli.js (pid ${innerPid}) survived wrapper SIGTERM — signal not forwarded`).toBe(true);

    // Clean shutdown should release the lock; a SIGKILLed supervisor would leave it.
    expect(fs.existsSync(lockPath), "supervisor.lock not released — child shutdown was not graceful").toBe(false);
  }, 20000);
});
