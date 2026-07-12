import { describe, it, expect, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-rg-"));
const bgPids: number[] = [];
afterAll(() => {
  for (const pid of bgPids) { try { process.kill(pid, "SIGKILL"); } catch {} }
  fs.rmSync(testRoot, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

// PTY_SESSION set => restart takes its "already inside a session, not attaching"
// branch and returns instead of hanging on a non-TTY attach.
function runCli(dir: string, args: string[], timeout = 15000) {
  return spawnSync(nodeBin, [cliPath, ...args], {
    env: { ...process.env, PTY_SESSION_DIR: dir, PTY_ROOT_LEGACY_SILENT: "1", PTY_SESSION: "outer" },
    encoding: "utf8", timeout,
  });
}

function createSession(dir: string, name: string, extra: string[], cmd: string[]): void {
  const r = runCli(dir, ["run", "-d", "--id", name, ...extra, "--", ...cmd]);
  expect(r.status).toBe(0);
  try {
    bgPids.push(Number(fs.readFileSync(path.join(dir, `${name}.pid`), "utf8").trim()));
  } catch {}
}

describe("pty restart guardrail for stateful agent sessions", () => {
  it("refuses to restart a role=agent session (exit nonzero, points at convoy)", () => {
    const dir = fs.mkdtempSync(path.join(testRoot, "d-"));
    createSession(dir, "ag", ["--tag", "role=agent"], ["sleep", "300"]);

    const r = runCli(dir, ["restart", "-y", "ag"]);
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/stateful agent/);
    expect(r.stderr).toMatch(/role=agent/);
    expect(r.stderr).toMatch(/--force/);
    expect(r.stderr).toMatch(/convoy/);
  }, 20000);

  it("refuses to restart a `claude --resume` command session", () => {
    const dir = fs.mkdtempSync(path.join(testRoot, "d-"));
    // No role tag — detection is purely on the stored argv.
    createSession(dir, "cr", ["--no-display-name"], ["claude", "--resume", "ABC-123"]);

    const r = runCli(dir, ["restart", "-y", "cr"]);
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/stateful agent/);
    expect(r.stderr).toMatch(/claude --resume/);
  }, 20000);

  it("does NOT block a normal session (no agent tag, no claude --resume)", () => {
    const dir = fs.mkdtempSync(path.join(testRoot, "d-"));
    createSession(dir, "plain", [], ["sleep", "300"]);

    const r = runCli(dir, ["restart", "-y", "plain"]);
    expect(r.status).toBe(0);
    expect(r.stdout).toContain("restarted");
    expect(r.stderr).not.toMatch(/stateful agent/);
  }, 20000);

  it("--force overrides the guardrail (restarts anyway)", () => {
    const dir = fs.mkdtempSync(path.join(testRoot, "d-"));
    createSession(dir, "ag2", ["--tag", "role=agent"], ["sleep", "300"]);

    // --force bypasses the guard AND the nesting guard, so it proceeds to
    // attach and would hang in this non-TTY test — a short timeout is fine; we
    // only assert it got PAST the guard ("restarted" printed, no refusal).
    const r = runCli(dir, ["restart", "-y", "--force", "ag2"], 4000);
    expect(r.stderr).not.toMatch(/stateful agent/);
    expect(r.stdout).toContain("restarted");
  }, 20000);
});
