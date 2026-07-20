// Exit-time cleanup of dead non-permanent sessions.
//
// A non-permanent session that finishes is garbage the moment it finishes.
// Rather than leaving its registry entry for a later `pty gc` sweep to
// notice, the daemon removes it as part of its own shutdown — so removal is
// CAUSED by the exit instead of discovered afterwards. That deletes both the
// sweep's polling interval and the window in which a dead session is still
// listed in `pty ls`.
//
// This file pins the whole policy, including its deliberate exemptions. The
// exemptions matter as much as the reap: each one is a case where the dead
// session's metadata/scrollback is still load-bearing for somebody.

import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, execFileSync } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const serverModule = path.join(__dirname, "..", "dist", "server.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-exitreap-"));
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
  return `xr${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
}

async function startDaemon(
  sessionDir: string,
  name: string,
  command: string,
  args: string[] = [],
  opts: { tags?: Record<string, string>; ephemeral?: boolean } = {},
): Promise<number> {
  const config = JSON.stringify({
    name, command, args, displayCommand: command,
    cwd: os.tmpdir(), rows: 24, cols: 80,
    tags: opts.tags,
    ephemeral: opts.ephemeral ?? false,
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

function runCli(sessionDir: string, ...args: string[]): { stdout: string; status: number } {
  try {
    const stdout = execFileSync(nodeBin, [cliPath, ...args], {
      env: { ...process.env, PTY_SESSION_DIR: sessionDir },
      encoding: "utf-8",
      timeout: 10000,
    });
    return { stdout, status: 0 };
  } catch (err: any) {
    return { stdout: (err.stdout ?? "") + (err.stderr ?? ""), status: err.status ?? 1 };
  }
}

function sessionFiles(dir: string, name: string): string[] {
  try {
    return fs.readdirSync(dir).filter((f) => f.startsWith(name));
  } catch {
    return [];
  }
}

function isAlive(pid: number): boolean {
  try { process.kill(pid, 0); return true; } catch { return false; }
}

/** Wait until `name` has no files left in `dir`, or the budget runs out.
 *  Returns the final file list so the assertion reports what survived. */
async function waitForGone(dir: string, name: string, budgetMs = 6000): Promise<string[]> {
  const deadline = Date.now() + budgetMs;
  let files = sessionFiles(dir, name);
  while (Date.now() < deadline && files.length > 0) {
    await new Promise((r) => setTimeout(r, 100));
    files = sessionFiles(dir, name);
  }
  return files;
}

/** Wait for a daemon pid to leave the process table. The reap happens during
 *  shutdown, so "the daemon is gone" is the earliest point at which the
 *  on-disk verdict is final. */
async function waitForDaemonExit(pid: number, budgetMs = 6000): Promise<void> {
  const deadline = Date.now() + budgetMs;
  while (Date.now() < deadline && isAlive(pid)) {
    await new Promise((r) => setTimeout(r, 50));
  }
}

afterEach(() => {
  // Only pids this file spawned are ever signalled.
  for (const pid of bgPids) {
    try { process.kill(pid, "SIGTERM"); } catch {}
  }
  bgPids = [];
  for (const dir of sessionDirs) {
    try {
      for (const e of fs.readdirSync(dir)) {
        try { fs.unlinkSync(path.join(dir, e)); } catch {}
      }
    } catch {}
  }
  sessionDirs = [];
});

describe("exit-time reap: sessions that clean themselves up", () => {
  it("removes a non-permanent session that exits cleanly", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const pid = await startDaemon(dir, name, "true");

    await waitForDaemonExit(pid);
    expect(await waitForGone(dir, name)).toEqual([]);
  }, 20000);

  it("removes a non-permanent session that exits nonzero (a crash counts as an exit)", async () => {
    // The distinction the reap does NOT draw: a crashed session is just as
    // dead as a clean one, and its metadata is just as much garbage. Anyone
    // who wants to inspect a crash uses `keep`.
    const dir = makeSessionDir();
    const name = uniqueName();
    const pid = await startDaemon(dir, name, "sh", ["-c", "exit 3"]);

    await waitForDaemonExit(pid);
    expect(await waitForGone(dir, name)).toEqual([]);
  }, 20000);

  it("removes the events file along with the metadata", async () => {
    // The reap is `cleanupAll`, not just the registry entry — a session that
    // is gone leaves no `.events.jsonl` behind to accumulate either.
    const dir = makeSessionDir();
    const name = uniqueName();
    const pid = await startDaemon(dir, name, "true");

    await waitForDaemonExit(pid);
    const files = await waitForGone(dir, name);
    expect(files.filter((f) => f.endsWith(".events.jsonl"))).toEqual([]);
  }, 20000);

  it("leaves nothing for `pty gc` to sweep afterwards", async () => {
    // The end-to-end point of the change: gc becomes a no-op for this class
    // of session because there is no longer a window in which it is listed.
    const dir = makeSessionDir();
    const name = uniqueName();
    const pid = await startDaemon(dir, name, "true");
    await waitForDaemonExit(pid);
    await waitForGone(dir, name);

    const gc = runCli(dir, "gc");
    expect(gc.stdout).not.toContain(name);
    expect(gc.stdout).toContain("Nothing to clean up.");
  }, 20000);
});

describe("exit-time reap: exemptions", () => {
  it("retains a session tagged keep=true", async () => {
    // `keep` exists precisely to hold a dead session's logs and scrollback
    // for debugging. If exit-time cleanup ignored it, the flag would be
    // unusable — the session would be gone before anyone could look.
    const dir = makeSessionDir();
    const name = uniqueName();
    const pid = await startDaemon(dir, name, "true", [], { tags: { keep: "true" } });

    await waitForDaemonExit(pid);
    await new Promise((r) => setTimeout(r, 1000));
    expect(sessionFiles(dir, name).some((f) => f.endsWith(".json"))).toBe(true);
  }, 20000);

  it("honours a keep tag applied while the session was still running", async () => {
    // The realistic flow: an operator watching a session decides to pin it
    // BEFORE it dies. The exit path must therefore read tags from current
    // on-disk metadata, not from the spawn-time config snapshot.
    const dir = makeSessionDir();
    const name = uniqueName();
    const pid = await startDaemon(dir, name, "sh", ["-c", "sleep 3"]);

    const tagged = runCli(dir, "tag", name, "keep=true");
    expect(tagged.status).toBe(0);

    await waitForDaemonExit(pid, 15000);
    await new Promise((r) => setTimeout(r, 1000));
    expect(sessionFiles(dir, name).some((f) => f.endsWith(".json"))).toBe(true);
  }, 30000);

  it("treats keep=false as no exemption", async () => {
    // So the exemption can be turned off in place, rather than only by
    // removing the key.
    const dir = makeSessionDir();
    const name = uniqueName();
    const pid = await startDaemon(dir, name, "true", [], { tags: { keep: "false" } });

    await waitForDaemonExit(pid);
    expect(await waitForGone(dir, name)).toEqual([]);
  }, 20000);

  it("retains a strategy=permanent session for its supervisor", async () => {
    // A permanent session's exit metadata is the record its supervisor
    // reconciles against to respawn it. Self-reaping would destroy exactly
    // the thing the supervisor needs.
    const dir = makeSessionDir();
    const name = uniqueName();
    const pid = await startDaemon(dir, name, "true", [], { tags: { strategy: "permanent" } });

    await waitForDaemonExit(pid);
    await new Promise((r) => setTimeout(r, 1000));
    expect(sessionFiles(dir, name).some((f) => f.endsWith(".json"))).toBe(true);
  }, 20000);

  it("retains metadata when the daemon is stopped from outside (`pty kill`)", async () => {
    // `pty kill` is stop-and-keep, deliberately distinct from `pty rm`.
    // The child had not finished — someone interrupted it, nearly always to
    // go look at what it was doing.
    const dir = makeSessionDir();
    const name = uniqueName();
    const pid = await startDaemon(dir, name, "cat");

    expect(runCli(dir, "kill", name).stdout).toContain("killed");
    await waitForDaemonExit(pid);
    await new Promise((r) => setTimeout(r, 500));

    const files = sessionFiles(dir, name);
    expect(files.some((f) => f.endsWith(".json"))).toBe(true);
    expect(files.some((f) => f.endsWith(".sock"))).toBe(false);
  }, 20000);

  it("still reaps a permanent session when --ephemeral is set", async () => {
    // `--ephemeral` predates this policy and was already the aggressive
    // opt-in. It keeps overriding everything except `keep`, so no existing
    // caller of it regresses.
    const dir = makeSessionDir();
    const name = uniqueName();
    const pid = await startDaemon(dir, name, "true", [], {
      tags: { strategy: "permanent" },
      ephemeral: true,
    });

    await waitForDaemonExit(pid);
    expect(await waitForGone(dir, name)).toEqual([]);
  }, 20000);

  it("lets `keep` win over --ephemeral", async () => {
    // `keep` is the explicit "I am going to look at this" flag. Nothing
    // should be able to delete the session out from under that intent
    // except an explicit `pty rm`.
    const dir = makeSessionDir();
    const name = uniqueName();
    const pid = await startDaemon(dir, name, "true", [], {
      tags: { keep: "true" },
      ephemeral: true,
    });

    await waitForDaemonExit(pid);
    await new Promise((r) => setTimeout(r, 1000));
    expect(sessionFiles(dir, name).some((f) => f.endsWith(".json"))).toBe(true);
  }, 20000);

  it("removes a kept session on an explicit `pty rm`", async () => {
    // `keep` exempts a session from REAPING, not from deletion. An operator
    // who asks for removal by name gets it — otherwise a kept session would
    // be un-removable except by editing its tags first.
    const dir = makeSessionDir();
    const name = uniqueName();
    const pid = await startDaemon(dir, name, "true", [], { tags: { keep: "true" } });
    await waitForDaemonExit(pid);
    await new Promise((r) => setTimeout(r, 500));

    expect(runCli(dir, "rm", name).stdout).toContain("removed");
    expect(sessionFiles(dir, name)).toEqual([]);
  }, 20000);
});

describe("exit-time reap: what it structurally cannot cover", () => {
  it("leaves a vanished session for `pty gc`, and gc still sweeps it", async () => {
    // The boundary of the whole feature. Self-cleanup runs in the daemon, so
    // a daemon that is SIGKILLed cannot perform it — the process that would
    // do the cleaning is the one that died. Vanished sessions are therefore
    // the residual duty that keeps gc's sweep alive.
    const dir = makeSessionDir();
    const name = uniqueName();
    const pid = await startDaemon(dir, name, "sh", ["-c", "sleep 60"]);

    // Only ever the pid this test spawned.
    process.kill(pid, "SIGKILL");
    await waitForDaemonExit(pid);
    await new Promise((r) => setTimeout(r, 500));

    // Survived its own death: no exit record was ever written.
    expect(sessionFiles(dir, name).some((f) => f.endsWith(".json"))).toBe(true);

    const gc = runCli(dir, "gc");
    expect(gc.stdout).toContain(`Removed: ${name}`);
    expect(sessionFiles(dir, name)).toEqual([]);
  }, 25000);

  it("reports kept sessions from `pty gc` instead of silently skipping them", async () => {
    // gc must agree with the exit path about `keep`: if it did not, a kept
    // session would survive its own exit only to be swept by the next tick.
    // Reporting makes "why is this dead session still listed?" answerable.
    const dir = makeSessionDir();
    const name = uniqueName();
    const pid = await startDaemon(dir, name, "true", [], { tags: { keep: "true" } });
    await waitForDaemonExit(pid);
    await new Promise((r) => setTimeout(r, 500));

    const gc = runCli(dir, "gc");
    expect(gc.stdout).toContain(`Kept (keep tag): ${name}`);
    expect(sessionFiles(dir, name).some((f) => f.endsWith(".json"))).toBe(true);
  }, 25000);
});
