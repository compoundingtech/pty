import { describe, it, expect, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-killw-"));
const bgPids: number[] = [];
afterAll(() => {
  for (const pid of bgPids) { try { process.kill(pid, "SIGTERM"); } catch {} }
  fs.rmSync(testRoot, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

function isAlive(pid: number): boolean {
  try { process.kill(pid, 0); return true; } catch { return false; }
}

function runCli(dir: string, args: string[]) {
  return spawnSync(nodeBin, [cliPath, ...args], {
    env: { ...process.env, PTY_SESSION_DIR: dir, PTY_ROOT_LEGACY_SILENT: "1" },
    encoding: "utf8", timeout: 15000,
  });
}

/** Create a real detached session (the production path) and return its pid. */
function createSession(dir: string, name: string): number {
  const r = runCli(dir, ["run", "-d", "--id", name, "--", "cat"]);
  expect(r.status).toBe(0);
  const pid = Number(fs.readFileSync(path.join(dir, `${name}.pid`), "utf8").trim());
  bgPids.push(pid);
  return pid;
}

describe("pty kill waits for the daemon to fully exit", () => {
  it("the daemon process is gone by the time `pty kill` returns", () => {
    const dir = fs.mkdtempSync(path.join(testRoot, "d-"));
    const pid = createSession(dir, "kw");
    expect(isAlive(pid)).toBe(true);

    const r = runCli(dir, ["kill", "kw"]);
    expect(r.status).toBe(0);
    expect(r.stdout).toContain("killed");
    // The point of the fix: kill blocked until the daemon (and its shutdown
    // metadata re-flush) finished, so the pid is already dead here — not racing
    // a caller that immediately `pty rm`s the session.
    expect(isAlive(pid)).toBe(false);
  }, 20000);

  it("a follow-up `pty rm` after kill leaves no stray files (no late-flush race)", () => {
    const dir = fs.mkdtempSync(path.join(testRoot, "d-"));
    createSession(dir, "kw2");

    expect(runCli(dir, ["kill", "kw2"]).status).toBe(0);
    expect(runCli(dir, ["rm", "kw2"]).status).toBe(0);

    // Daemon fully exited before kill returned, so rm removed the final state
    // and nothing (metadata or a `.tmp.*`) can reappear.
    const leftovers = fs.readdirSync(dir).filter((f) => f.startsWith("kw2"));
    expect(leftovers).toEqual([]);
  }, 20000);

  it("daemon shutdown does NOT resurrect metadata that was already removed", async () => {
    // The (ii) fix for the watchdog residue: if the session's metadata was
    // removed (e.g. `pty rm`) while the daemon is still shutting down, the
    // daemon's late exit-metadata re-flush must not re-create the `.json`.
    const dir = fs.mkdtempSync(path.join(testRoot, "d-"));
    const pid = createSession(dir, "kw3");
    const metaPath = path.join(dir, "kw3.json");
    expect(fs.existsSync(metaPath)).toBe(true);

    // Remove the metadata out from under the running daemon, then trigger its
    // shutdown (its exit-metadata write fires during shutdown).
    fs.unlinkSync(metaPath);
    process.kill(pid, "SIGTERM");
    const start = Date.now();
    while (Date.now() - start < 6000 && isAlive(pid)) {
      await new Promise((r) => setTimeout(r, 50));
    }
    await new Promise((r) => setTimeout(r, 400)); // grace for any late write

    expect(isAlive(pid)).toBe(false);
    // Must stay gone — no resurrected .json, no stray .json.tmp.*.
    expect(fs.existsSync(metaPath)).toBe(false);
    expect(fs.readdirSync(dir).filter((f) => f.startsWith("kw3.json"))).toEqual([]);
  }, 20000);
});
