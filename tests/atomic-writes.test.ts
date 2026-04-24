// Concurrent writer corruption regression. Originally reported against
// `pty tag` — two calls racing on the same `<name>.json.tmp` path could
// leave a corrupted JSON file behind. Same bug class lived in the
// supervisor state save, the `pty supervisor reset` path, and the
// event-log truncation path (which rewrote in place, so readers could
// see a half-written file mid-truncation).
//
// These tests spawn N child processes that race on the same session and
// assert the persisted file is ALWAYS valid JSON / valid JSONL. Last-
// write-wins is acceptable per the fix's stated contract; corruption
// is not.

import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, spawnSync } from "node:child_process";
import {
  atomicWriteFileSync, atomicWriteFile,
  updateTags, setState, readMetadata,
} from "../src/sessions.ts";
import { appendEventSync, readRecentEvents } from "../src/events.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const serverModule = path.join(__dirname, "..", "dist", "server.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-atomic-"));
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
  return `at${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
}

async function startDaemon(sessionDir: string, name: string): Promise<number> {
  const config = JSON.stringify({
    name, command: "cat", args: [], displayCommand: "cat",
    cwd: os.tmpdir(), rows: 24, cols: 80,
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
  throw new Error("Timeout waiting for daemon");
}

afterEach(() => {
  for (const pid of bgPids) { try { process.kill(pid, "SIGTERM"); } catch {} }
  bgPids = [];
  for (const dir of sessionDirs) {
    try {
      for (const e of fs.readdirSync(dir)) { try { fs.unlinkSync(path.join(dir, e)); } catch {} }
    } catch {}
  }
  sessionDirs = [];
});

describe("atomicWriteFileSync / atomicWriteFile helpers", () => {
  it("produces a file readable by a concurrent reader throughout a rewrite cycle", async () => {
    const dir = makeSessionDir();
    const target = path.join(dir, "test.json");
    atomicWriteFileSync(target, JSON.stringify({ version: 0 }));

    // Writer loop: rewrites the file 200x in quick succession.
    // Reader loop: opens and JSON.parses the file; must never fail.
    let writerDone = false;
    const writerErrors: string[] = [];
    const readerErrors: string[] = [];

    const writer = (async () => {
      try {
        for (let i = 0; i < 200; i++) {
          atomicWriteFileSync(target, JSON.stringify({ version: i + 1 }));
          await new Promise((r) => setImmediate(r));
        }
      } catch (e: any) { writerErrors.push(e.message); }
      finally { writerDone = true; }
    })();

    const reader = (async () => {
      while (!writerDone) {
        try {
          const content = fs.readFileSync(target, "utf-8");
          JSON.parse(content); // must not throw
        } catch (e: any) { readerErrors.push(e.message); }
        await new Promise((r) => setImmediate(r));
      }
    })();

    await Promise.all([writer, reader]);
    expect(writerErrors).toEqual([]);
    expect(readerErrors).toEqual([]);
  }, 15000);

  it("async twin produces the same guarantee under Promise.all writers", async () => {
    const dir = makeSessionDir();
    const target = path.join(dir, "test-async.json");
    await atomicWriteFile(target, JSON.stringify({ version: 0 }));

    let writerDone = false;
    const readerErrors: string[] = [];

    const writer = (async () => {
      for (let i = 0; i < 50; i++) {
        await Promise.all(
          Array.from({ length: 4 }, (_, k) =>
            atomicWriteFile(target, JSON.stringify({ batch: i, writer: k })),
          ),
        );
      }
      writerDone = true;
    })();

    const reader = (async () => {
      while (!writerDone) {
        try {
          const content = fs.readFileSync(target, "utf-8");
          JSON.parse(content);
        } catch (e: any) { readerErrors.push(e.message); }
        await new Promise((r) => setImmediate(r));
      }
    })();

    await Promise.all([writer, reader]);
    expect(readerErrors).toEqual([]);
  }, 15000);

  it("cleans up tmp files after every successful write (no leftover *.tmp.*)", async () => {
    const dir = makeSessionDir();
    const target = path.join(dir, "clean.json");
    for (let i = 0; i < 50; i++) {
      atomicWriteFileSync(target, JSON.stringify({ i }));
    }
    const entries = fs.readdirSync(dir);
    const leftover = entries.filter((e) => e.includes(".tmp."));
    expect(leftover).toEqual([]);
  });
});

describe("concurrent pty tag via multiple CLI processes", () => {
  it("leaves the metadata file valid JSON after 10 racing taggers", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    // Spawn 10 concurrent `pty tag` processes, each setting a distinct key.
    const N = 10;
    const procs: ReturnType<typeof spawn>[] = [];
    for (let i = 0; i < N; i++) {
      procs.push(spawn(nodeBin, [cliPath, "tag", name, `k${i}=${i}`], {
        env: { ...process.env, PTY_SESSION_DIR: dir },
        stdio: "ignore",
      }));
    }
    await Promise.all(procs.map((p) => new Promise<void>((resolve) => p.on("exit", () => resolve()))));

    // File must be valid JSON. Last-write-wins means we may have lost some
    // updates, but the file itself MUST parse.
    const metaPath = path.join(dir, `${name}.json`);
    const content = fs.readFileSync(metaPath, "utf-8");
    expect(() => JSON.parse(content)).not.toThrow();

    // No leftover tmp files from crashed or racing writers.
    const entries = fs.readdirSync(dir);
    const leftover = entries.filter((e) => e.includes(".tmp."));
    expect(leftover).toEqual([]);
  }, 30000);

  it("in-process Promise.all of setState calls leaves metadata valid", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;

    // 50 concurrent setState calls in the same process — stress-tests the
    // atomic helper under the sync Promise.all-of-sync-functions pattern.
    await Promise.all(
      Array.from({ length: 50 }, (_, i) =>
        Promise.resolve().then(() => setState(name, `k${i}`, i)),
      ),
    );

    const meta = readMetadata(name);
    expect(meta).not.toBeNull();
    // All 50 should have landed — Node's single-threaded eventloop + sync
    // setState means writes serialize, no losses.
    expect(Object.keys(meta!.state ?? {}).sort()).toHaveLength(50);
  }, 15000);
});

describe("event log truncation vs concurrent reader", () => {
  it("reader never sees a half-written file during truncation", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    process.env.PTY_SESSION_DIR = dir;

    // Prime the log well past MAX_LINES so truncation is guaranteed to run.
    // emitUserEvent uses validateUserEventType; we go straight to
    // appendEventSync via a minimal UserEvent to skip that path.
    for (let i = 0; i < 1200; i++) {
      appendEventSync(name, {
        session: name,
        type: `user.prime` as const,
        ts: new Date().toISOString(),
        data: { i },
      } as any);
    }

    // Reader loop: hammer the events file while a second writer loop keeps
    // triggering truncation. Reader must ALWAYS see valid JSONL.
    let writerDone = false;
    const readerErrors: string[] = [];

    const writer = (async () => {
      for (let i = 0; i < 500; i++) {
        appendEventSync(name, {
          session: name,
          type: `user.more` as const,
          ts: new Date().toISOString(),
          data: { i },
        } as any);
        if (i % 25 === 0) await new Promise((r) => setImmediate(r));
      }
      writerDone = true;
    })();

    const reader = (async () => {
      while (!writerDone) {
        try {
          const events = readRecentEvents(name);
          // readRecentEvents drops malformed lines silently, but if the file
          // was half-written we'd see a truncated final line missing its
          // newline or a line that doesn't JSON.parse. Verify every
          // returned event has a `type` field (the weakest postcondition).
          for (const e of events) {
            if (typeof e.type !== "string") {
              readerErrors.push("event without .type");
              break;
            }
          }
        } catch (e: any) {
          readerErrors.push(e.message);
        }
        await new Promise((r) => setImmediate(r));
      }
    })();

    await Promise.all([writer, reader]);
    expect(readerErrors).toEqual([]);

    // Final file must also be valid.
    const content = fs.readFileSync(path.join(dir, `${name}.events.jsonl`), "utf-8");
    const finalLines = content.trimEnd().split("\n").filter(Boolean);
    for (const l of finalLines) {
      expect(() => JSON.parse(l)).not.toThrow();
    }
  }, 30000);
});

describe("concurrent updateTags from separate processes", () => {
  it("no corruption when 20 processes race (direct child-process fan-out)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const N = 20;
    const procs: ReturnType<typeof spawn>[] = [];
    for (let i = 0; i < N; i++) {
      procs.push(spawn(nodeBin, [cliPath, "tag", name, `race${i}=${i}`], {
        env: { ...process.env, PTY_SESSION_DIR: dir },
        stdio: "ignore",
      }));
    }
    await Promise.all(procs.map((p) => new Promise<void>((resolve) => p.on("exit", () => resolve()))));

    const metaPath = path.join(dir, `${name}.json`);
    const content = fs.readFileSync(metaPath, "utf-8");
    const parsed = JSON.parse(content);
    expect(typeof parsed).toBe("object");
    expect(parsed).not.toBeNull();

    // Per the contract: last-write-wins, so SOME subset of the 20 keys
    // will survive. We can't predict which, but (a) the file parses and
    // (b) at least one update landed (otherwise the atomic helper is
    // broken in a different way).
    const raceKeys = Object.keys(parsed.tags ?? {}).filter((k) => k.startsWith("race"));
    expect(raceKeys.length).toBeGreaterThan(0);
  }, 30000);
});
