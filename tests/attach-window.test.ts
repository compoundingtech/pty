// Regression tests for issue #180 (attach window).
//
// `pty run` (attached) starts the child while it still holds the per-session
// creation lock, so a `pty metadata patch --id $PTY_SESSION` executed as the
// first thing in the child deterministically failed with `metadata is busy`.
// The owner sidecar (`<id>.pid`) was likewise published only after the child
// spawn, so ancestry checks at child start could race it.
//
// The daemon-side fix under test:
//   1. `patchMetadataById` waits boundedly (METADATA_PATCH_WAIT_MS) for the
//      creation/attach window instead of failing `busy` at once —
//      fail-closed: stuck locks still fail, never indefinite.
//   2. The daemon publishes the owner sidecar BEFORE spawning the child.
//
// Style follows tests/atomic-writes.test.ts and tests/metadata-events.test.ts:
// real daemon subprocesses, real CLI processes, isolated tmp session dirs.

import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn } from "node:child_process";
import { terminateAndWait } from "./setup/processes.ts";
import {
  acquireLock, isCreationLockHeld, patchMetadataById, readMetadata, releaseLock,
} from "../src/sessions.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const serverModule = path.join(__dirname, "..", "dist", "server.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-attachwin-"));
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
  return `aw${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
}

afterEach(async () => {
  await terminateAndWait(bgPids);
  bgPids = [];
  for (const dir of sessionDirs) {
    try {
      for (const e of fs.readdirSync(dir)) { try { fs.unlinkSync(path.join(dir, e)); } catch {} }
    } catch {}
  }
  sessionDirs = [];
});

/** Minimal valid record: what the daemon has published while `pty run`
 *  still holds the creation lock (the deterministic state from repro 1). */
function publishRecord(dir: string, name: string): void {
  fs.writeFileSync(path.join(dir, `${name}.json`), JSON.stringify({
    command: "sleep",
    args: ["30"],
    displayCommand: "sleep 30",
    cwd: os.tmpdir(),
    createdAt: new Date().toISOString(),
  }));
}

interface PatchRun {
  code: number | null;
  stdout: string;
  stderr: string;
}

/** Run `pty metadata patch` as a separate process, like the attached child does. */
function runPatchCli(dir: string, name: string, patch: unknown): Promise<PatchRun> {
  return new Promise((resolve) => {
    const child = spawn(nodeBin, [cliPath, "metadata", "patch", "--id", name], {
      env: { ...process.env, PTY_SESSION_DIR: dir, PTY_ROOT_LEGACY_SILENT: "1" },
      stdio: ["pipe", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    child.stdout?.on("data", (d: Buffer) => { stdout += d.toString(); });
    child.stderr?.on("data", (d: Buffer) => { stderr += d.toString(); });
    child.on("error", (e) => { stderr += String(e); });
    child.stdin!.end(JSON.stringify(patch));
    child.once("exit", (code) => resolve({ code, stdout, stderr }));
  });
}

describe("issue #180: metadata patch during the attach window", () => {
  it("waits out a held creation lock instead of failing busy", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    publishRecord(dir, name);
    process.env.PTY_SESSION_DIR = dir;

    // Simulate `pty run` holding the creation lock across creation/attach.
    expect(acquireLock(name)).toBe(true);
    expect(isCreationLockHeld(name)).toBe(true);
    const startedAt = Date.now();
    try {
      const patchPromise = runPatchCli(dir, name, { tags: { "issue.180": "1" } });
      // Let the patch arrive while the lock is held (pre-fix it failed here
      // immediately with `metadata is busy`), then release like a spawner
      // whose daemon finished publishing.
      await new Promise((r) => setTimeout(r, 700));
      expect(isCreationLockHeld(name)).toBe(true);
      releaseLock(name);
      const run = await patchPromise;
      expect(Date.now() - startedAt).toBeGreaterThanOrEqual(600);
      expect(run.stderr).not.toMatch(/busy/i);
      expect(run.code).toBe(0);
      expect(JSON.parse(run.stdout)).toMatchObject({ changed: true });
      expect(readMetadata(name)?.tags?.["issue.180"]).toBe("1");
    } finally {
      releaseLock(name);
    }
  }, 30_000);

  it("waits for a late-published record while creation is in flight", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    process.env.PTY_SESSION_DIR = dir;

    // No record yet — the child started before the daemon published it.
    // The live creation lock is the evidence a creation is in flight.
    expect(acquireLock(name)).toBe(true);
    try {
      const patchPromise = runPatchCli(dir, name, { tags: { late: "yes" } });
      await new Promise((r) => setTimeout(r, 500));
      publishRecord(dir, name);
      await new Promise((r) => setTimeout(r, 400));
      releaseLock(name);
      const run = await patchPromise;
      expect(run.code).toBe(0);
      expect(JSON.parse(run.stdout)).toMatchObject({ changed: true });
      expect(readMetadata(name)?.tags?.late).toBe("yes");
    } finally {
      releaseLock(name);
    }
  }, 30_000);

  it("still fails closed (busy) when the lock never releases — bounded, never indefinite", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    publishRecord(dir, name);
    process.env.PTY_SESSION_DIR = dir;
    expect(acquireLock(name)).toBe(true);
    try {
      const startedAt = Date.now();
      await expect(patchMetadataById(name, { tags: { stuck: "1" } }, 300))
        .rejects.toThrow(/metadata is busy/i);
      const elapsed = Date.now() - startedAt;
      expect(elapsed).toBeGreaterThanOrEqual(200); // it waited, not fail-fast
      expect(elapsed).toBeLessThan(5000); // bounded, never indefinite
      expect(readMetadata(name)?.tags?.stuck).toBeUndefined();
    } finally {
      releaseLock(name);
    }
  }, 15_000);

  it("unknown ids still fail fast with not found (no live creation lock)", async () => {
    const dir = makeSessionDir();
    process.env.PTY_SESSION_DIR = dir;
    const startedAt = Date.now();
    await expect(patchMetadataById(`nosuch-${uniqueName()}`, { tags: { x: "y" } }, 8000))
      .rejects.toThrow(/not found/);
    expect(Date.now() - startedAt).toBeLessThan(2000);
  }, 15_000);
});

describe("issue #180: owner sidecar present at child start", () => {
  async function verdictAtChildStart(dir: string, name: string): Promise<string> {
    const verdictPath = path.join(dir, `${name}.verdict`);
    const pidPath = path.join(dir, `${name}.pid`);
    // The child records, as its very first action, whether the owner
    // sidecar was already published — then parks so the daemon stays up.
    const probe =
      `if test -f "${pidPath}"; then echo present > "${verdictPath}"; ` +
      `else echo absent > "${verdictPath}"; fi; exec sleep 30`;
    const config = JSON.stringify({
      name, command: "sh", args: ["-c", probe], displayCommand: "sh",
      cwd: os.tmpdir(), rows: 24, cols: 80,
    });
    const child = spawn(nodeBin, [serverModule], {
      detached: true,
      stdio: ["ignore", "ignore", "pipe"],
      env: { ...process.env, PTY_SERVER_CONFIG: config, PTY_SESSION_DIR: dir },
    });
    bgPids.push(child.pid!);
    let stderr = "";
    child.stderr?.on("data", (d: Buffer) => { stderr += d.toString(); });
    child.on("exit", (code) => {
      if (code !== null && !fs.existsSync(verdictPath)) {
        throw new Error(`Daemon exited before child ran: ${stderr}`);
      }
    });
    (child.stderr as any)?.unref?.();
    child.unref();
    const deadline = Date.now() + 10_000;
    while (Date.now() < deadline) {
      try {
        const verdict = fs.readFileSync(verdictPath, "utf-8").trim();
        if (verdict === "present" || verdict === "absent") return verdict;
      } catch {}
      await new Promise((r) => setTimeout(r, 25));
    }
    throw new Error(`Timed out waiting for child verdict (daemon stderr: ${stderr})`);
  }

  it("publishes the sidecar before the child runs (3 consecutive starts)", async () => {
    for (let i = 0; i < 3; i++) {
      const dir = makeSessionDir();
      const name = uniqueName();
      // Must read the verdict before afterEach wipes the dir.
      expect(await verdictAtChildStart(dir, name)).toBe("present");
      await terminateAndWait(bgPids);
      bgPids = [];
    }
  }, 60_000);
});
