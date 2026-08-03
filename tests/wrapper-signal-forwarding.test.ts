// Verifies that bin/pty runs the CLI in the launcher's process. Keeping a
// single process makes inherited descriptors and signal ownership identical
// to a direct dist/cli.js invocation.

import { describe, it, expect, afterEach } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { execFileSync, spawn } from "node:child_process";

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

describe("bin/pty process lifecycle", () => {
  it("runs a long-lived command as one process that exits on SIGTERM", async () => {
    const sessionDir = makeSessionDir();
    const socketPath = path.join(sessionDir, "remote.sock");

    const wrapper = spawn(nodeBin, [wrapperPath, "remote-serve", "--socket", socketPath], {
      env: { ...process.env, PTY_SESSION_DIR: sessionDir },
      stdio: ["ignore", "pipe", "pipe"],
    });
    trackedPids.push(wrapper.pid!);

    let stdout = "";
    let stderr = "";
    wrapper.stdout?.on("data", (d: Buffer) => { stdout += d.toString(); });
    wrapper.stderr?.on("data", (d: Buffer) => { stderr += d.toString(); });

    let exitCode: number | null = null;
    let exitSignal: NodeJS.Signals | null = null;
    wrapper.on("exit", (c, s) => { exitCode = c; exitSignal = s; });

    const started = await waitFor(
      () => stdout.includes(`pty remote-serve listening on ${socketPath}`),
      5000,
    );
    expect(started, `bin/pty did not become ready; stdout=${stdout} stderr=${stderr}`).toBe(true);

    const childPids = (() => {
      try {
        return execFileSync("pgrep", ["-P", String(wrapper.pid)], { encoding: "utf-8" }).trim();
      } catch {
        return "";
      }
    })();
    expect(childPids, `bin/pty unexpectedly spawned a child; stdout=${stdout} stderr=${stderr}`).toBe("");

    wrapper.kill("SIGTERM");

    const wrapperExited = await waitFor(() => exitCode !== null || exitSignal !== null, 5000);
    expect(wrapperExited, "wrapper did not exit after SIGTERM").toBe(true);

    expect(exitCode).toBe(0);
    expect(exitSignal).toBeNull();
  }, 20000);
});
