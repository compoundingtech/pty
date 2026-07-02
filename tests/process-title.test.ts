import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

// The daemon sets `process.title = "pty-daemon"` so it is identifiable in
// ps/top/htop/btm instead of showing V8's default main-thread name
// ("MainThread" under Node 24+). The only OS-visible proof is
// /proc/<pid>/comm, which exists on Linux only — so the comm assertion is
// gated on platform. Linux caps comm at 15 chars (TASK_COMM_LEN); the title
// values used here are all well under that.

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const isLinux = process.platform === "linux";

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-title-"));
afterAll(() => {
  fs.rmSync(testRoot, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

let bgPids: number[] = [];

function readComm(pid: number): string {
  return fs.readFileSync(`/proc/${pid}/comm`, "utf-8").trim();
}

afterEach(() => {
  for (const pid of bgPids) {
    try { process.kill(pid, "SIGTERM"); } catch {}
  }
  bgPids = [];
});

describe("daemon process title", () => {
  it.skipIf(!isLinux)("daemon process comm is 'pty-daemon'", () => {
    const sessionDir = fs.mkdtempSync(path.join(testRoot, "sd-"));
    const name = "title-test";
    const r = spawnSync(
      nodeBin,
      [cliPath, "run", "-d", "--id", name, "--no-display-name", "--", "sleep", "30"],
      {
        cwd: os.tmpdir(),
        env: { ...process.env, PTY_SESSION_DIR: sessionDir },
        encoding: "utf-8",
        timeout: 15000,
      },
    );
    expect(r.status, r.stderr).toBe(0);

    const pid = parseInt(
      fs.readFileSync(path.join(sessionDir, `${name}.pid`), "utf-8").trim(),
      10,
    );
    expect(Number.isInteger(pid)).toBe(true);
    bgPids.push(pid);

    // comm is capped at 15 chars; "pty-daemon" (10) is not truncated.
    expect(readComm(pid)).toBe("pty-daemon");
  });
});
