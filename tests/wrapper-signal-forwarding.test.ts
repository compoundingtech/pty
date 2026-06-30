// Verifies that bin/pty forwards SIGTERM/SIGINT to the inner cli.js child.
// systemd's `KillMode=process` only signals the leader (the bin/pty shell
// shim) and lets children become orphans unless the leader propagates the
// signal. Without forwarding, the inner cli.js survives a unit `stop` and
// the next start fails because the orphan still holds whatever resource
// it owned (file watchers, sockets, etc.).

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
  it("propagates SIGTERM to the inner cli.js (events --all is long-lived)", async () => {
    const sessionDir = makeSessionDir();

    // `events --all` is a long-lived command that registers a SIGINT
    // handler and runs an EventFollower until the process is signalled.
    // Same shape as the supervisor used to be: long-running, owns file
    // watchers, must exit cleanly when the wrapper relays a signal.
    const wrapper = spawn(nodeBin, [wrapperPath, "events", "--all"], {
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

    // Wait long enough for the wrapper to fork the inner node cli.js.
    // No specific marker; sleep briefly then look at the process tree.
    await new Promise((r) => setTimeout(r, 800));

    // Find the inner cli.js by walking the wrapper's child processes via
    // /proc on Linux, or via `pgrep -P` everywhere. We use ps -o pid -p
    // <wrapper-pid> first then list children with a tree walk fallback.
    const psResult = (() => {
      try {
        const { execFileSync } = require("node:child_process") as typeof import("node:child_process");
        return execFileSync("pgrep", ["-P", String(wrapper.pid)], { encoding: "utf-8" }).trim();
      } catch {
        return "";
      }
    })();
    expect(psResult, `pgrep failed to find a child of pid ${wrapper.pid}; stdout=${stdout} stderr=${stderr}`).not.toBe("");

    const innerPid = parseInt(psResult.split("\n")[0]!.trim(), 10);
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
  }, 20000);
});
