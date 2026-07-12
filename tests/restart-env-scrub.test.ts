import { describe, it, expect, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-envscrub-"));
const bgPids: number[] = [];
afterAll(() => {
  for (const pid of bgPids) { try { process.kill(pid, "SIGKILL"); } catch {} }
  fs.rmSync(testRoot, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

interface RunOpts { env?: Record<string, string>; unset?: string[]; timeout?: number }

// PTY_SESSION set => restart takes its "already inside a session, not attaching"
// branch and returns instead of hanging on a non-TTY attach.
function runCli(dir: string, args: string[], opts: RunOpts = {}) {
  const env: Record<string, string> = {
    ...(process.env as Record<string, string>),
    PTY_SESSION_DIR: dir, PTY_ROOT_LEGACY_SILENT: "1", PTY_SESSION: "outer",
    ...(opts.env ?? {}),
  };
  for (const k of opts.unset ?? []) delete env[k];
  return spawnSync(nodeBin, [cliPath, ...args], { env, encoding: "utf8", timeout: opts.timeout ?? 15000 });
}

/** A command that records the ST_AGENT/ST_ROOT it was actually launched with,
 *  then stays alive. `-` default => "UNSET" when the var is absent. Re-runs on
 *  every (re)start, so the file always reflects the current child's env. */
function recorderCmd(outFile: string): string[] {
  return ["sh", "-c", `printf '%s|%s' "\${ST_AGENT-UNSET}" "\${ST_ROOT-UNSET}" > "${outFile}"; exec sleep 300`];
}

function createSession(dir: string, name: string, outFile: string, opts: RunOpts): void {
  const r = runCli(dir, ["run", "-d", "--id", name, "--", ...recorderCmd(outFile)], opts);
  expect(r.status).toBe(0);
  try {
    bgPids.push(Number(fs.readFileSync(path.join(dir, `${name}.pid`), "utf8").trim()));
  } catch {}
}

const sleepSync = (ms: number) => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);

function waitForContent(p: string, timeoutMs = 4000): string {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    try {
      const s = fs.readFileSync(p, "utf8");
      if (s.length > 0) return s;
    } catch {}
    sleepSync(50);
  }
  try { return fs.readFileSync(p, "utf8"); } catch { return ""; }
}

describe("restart scrubs the restarter's bus-identity env", () => {
  it("does NOT leak the restarter's ST_AGENT/ST_ROOT into the re-exec'd session", () => {
    const dir = fs.mkdtempSync(path.join(testRoot, "d-"));
    const outFile = path.join(dir, "child.env");
    // Create with no ambient identity so the first launch records UNSET|UNSET.
    createSession(dir, "s", outFile, { unset: ["ST_AGENT", "ST_ROOT"] });
    expect(waitForContent(outFile)).toBe("UNSET|UNSET");

    // Restart from a DIFFERENT agent's shell: its identity must not leak in.
    fs.rmSync(outFile, { force: true });
    const r = runCli(dir, ["restart", "-y", "s"], {
      env: { ST_AGENT: "smalltalk-claude", ST_ROOT: "/leaked/convoy" },
    });
    expect(r.status).toBe(0);
    expect(r.stdout).toContain("restarted");

    const recorded = waitForContent(outFile);
    expect(recorded).toBe("UNSET|UNSET");
    expect(recorded).not.toContain("smalltalk-claude");
    expect(recorded).not.toContain("/leaked/convoy");
  }, 25000);

  it("still inherits the creator's ST_AGENT on a fresh `pty run` (create path unaffected)", () => {
    const dir = fs.mkdtempSync(path.join(testRoot, "d-"));
    const outFile = path.join(dir, "child.env");
    createSession(dir, "fresh", outFile, {
      env: { ST_AGENT: "creator-abc", ST_ROOT: "/creator/convoy" },
    });
    expect(waitForContent(outFile)).toBe("creator-abc|/creator/convoy");
  }, 20000);
});
