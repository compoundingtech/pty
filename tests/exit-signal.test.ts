import { describe, it, expect, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync, execFileSync } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-sig-"));
const bgPids: number[] = [];
afterAll(() => {
  for (const pid of bgPids) { try { process.kill(pid, "SIGKILL"); } catch {} }
  fs.rmSync(testRoot, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

function runCli(dir: string, args: string[]) {
  return spawnSync(nodeBin, [cliPath, ...args], {
    env: { ...process.env, PTY_SESSION_DIR: dir, PTY_ROOT_LEGACY_SILENT: "1" },
    encoding: "utf8", timeout: 15000,
  });
}

function createSession(dir: string, name: string, cmd: string[]): number {
  expect(runCli(dir, ["run", "-d", "--id", name, "--", ...cmd]).status).toBe(0);
  const pid = Number(fs.readFileSync(path.join(dir, `${name}.pid`), "utf8").trim());
  bgPids.push(pid);
  return pid;
}

function readMeta(dir: string, name: string): any {
  return JSON.parse(fs.readFileSync(path.join(dir, `${name}.json`), "utf8"));
}

async function waitForExit(dir: string, name: string): Promise<any> {
  const start = Date.now();
  while (Date.now() - start < 8000) {
    try {
      const m = readMeta(dir, name);
      if (m.exitedAt) return m;
    } catch {}
    await new Promise((r) => setTimeout(r, 50));
  }
  throw new Error(`session "${name}" never recorded an exit`);
}

describe("pty surfaces a signal death (OOM SIGKILL) instead of losing it", () => {
  it("a SIGKILL'd child is recorded as 128+signal (137), not exit 0", async () => {
    const dir = fs.mkdtempSync(path.join(testRoot, "d-"));
    const daemonPid = createSession(dir, "sk", ["sh", "-c", "exec sleep 300"]);

    // The session's leaf is the daemon's direct child (exec replaces the sh).
    const leaf = Number(
      execFileSync("pgrep", ["-P", String(daemonPid)], { encoding: "utf8" }).trim().split("\n")[0],
    );
    expect(Number.isInteger(leaf)).toBe(true);

    process.kill(leaf, "SIGKILL"); // simulate an OS OOM kill

    const meta = await waitForExit(dir, "sk");
    // Was exit 0 (clean finish) before the fix — a real crash that convoy's
    // nonzero gate would have missed. Now surfaced as 128 + SIGKILL(9) = 137.
    expect(meta.exitCode).toBe(137);

    // The raw signal is also surfaced on the session_exit event.
    const events = fs.readFileSync(path.join(dir, "sk.events.jsonl"), "utf8")
      .trim().split("\n").map((l) => JSON.parse(l));
    const exit = events.find((e) => e.type === "session_exit");
    expect(exit.exitCode).toBe(137);
    expect(exit.signal).toBe(9);
  }, 20000);

  it("a clean exit is unchanged (raw code, no signal)", async () => {
    const dir = fs.mkdtempSync(path.join(testRoot, "d-"));
    createSession(dir, "ce", ["sh", "-c", "exit 5"]);

    const meta = await waitForExit(dir, "ce");
    expect(meta.exitCode).toBe(5);
    expect(meta.signal).toBeUndefined();

    const events = fs.readFileSync(path.join(dir, "ce.events.jsonl"), "utf8")
      .trim().split("\n").map((l) => JSON.parse(l));
    const exit = events.find((e) => e.type === "session_exit");
    expect(exit.exitCode).toBe(5);
    expect(exit.signal).toBeUndefined();
  }, 20000);
});
