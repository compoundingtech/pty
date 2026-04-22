// Tests for the `pty emit` CLI subcommand and the underlying
// emitUserEvent helper. Round-trips events through a real events.jsonl
// file so the parse + format paths also get covered.

import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, spawnSync } from "node:child_process";
import {
  emitUserEvent, validateUserEventType, readRecentEvents, isUserEvent,
  EventFollower, type EventRecord,
} from "../src/events.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const serverModule = path.join(__dirname, "..", "dist", "server.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-emit-"));
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
  return `em${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
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

function runCli(sessionDir: string, env: Record<string, string>, ...args: string[]) {
  return spawnSync(nodeBin, [cliPath, ...args], {
    env: { ...process.env, PTY_SESSION_DIR: sessionDir, ...env },
    encoding: "utf-8",
    timeout: 10_000,
  });
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

describe("validateUserEventType", () => {
  it("accepts user.something", () => {
    expect(validateUserEventType("user.build-done")).toBeNull();
    expect(validateUserEventType("user.a")).toBeNull();
  });
  it("rejects types that don't start with user.", () => {
    expect(validateUserEventType("build-done")).toMatch(/must start with/);
    expect(validateUserEventType("session_start")).toMatch(/must start with/);
    expect(validateUserEventType("state.set")).toMatch(/must start with/);
  });
  it("rejects bare 'user.'", () => {
    expect(validateUserEventType("user.")).toMatch(/suffix/);
  });
  it("rejects empty / whitespace / control chars", () => {
    expect(validateUserEventType("")).toMatch(/non-empty/);
    expect(validateUserEventType("user.has space")).toMatch(/whitespace/);
    expect(validateUserEventType("user.tab\tfoo")).toMatch(/whitespace/);
  });
});

describe("emitUserEvent direct API", () => {
  it("writes a user event and readRecentEvents round-trips it", async () => {
    process.env.PTY_SESSION_DIR = makeSessionDir();
    const name = uniqueName();
    // Don't need a daemon — appendEvent just writes the .events.jsonl file.
    await emitUserEvent(name, "user.build-done", { data: { pct: 100 } });
    await emitUserEvent(name, "user.note", { text: "hello" });

    const events = readRecentEvents(name);
    expect(events).toHaveLength(2);
    expect(events[0].type).toBe("user.build-done");
    expect((events[0] as any).data).toEqual({ pct: 100 });
    expect(events[1].type).toBe("user.note");
    expect((events[1] as any).text).toBe("hello");
    expect(isUserEvent(events[0])).toBe(true);
  });

  it("throws on invalid types instead of silently writing them", async () => {
    process.env.PTY_SESSION_DIR = makeSessionDir();
    const name = uniqueName();
    await expect(emitUserEvent(name, "system_thing")).rejects.toThrow(/must start with/);
    await expect(emitUserEvent(name, "user.")).rejects.toThrow(/suffix/);
  });
});

describe("pty emit CLI", () => {
  it("publishes a user event on a running session", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const r = runCli(dir, {}, "emit", name, "user.tests-passed",
      "--json", '{"count": 42}');
    expect(r.status).toBe(0);

    const events = readRecentEventsInDir(dir, name);
    const latest = events[events.length - 1];
    expect(latest.type).toBe("user.tests-passed");
    expect((latest as any).data).toEqual({ count: 42 });
  }, 15000);

  it("resolves to $PTY_SESSION when no ref is given", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const r = runCli(dir, { PTY_SESSION: name }, "emit", "user.from-inside");
    expect(r.status).toBe(0);

    const events = readRecentEventsInDir(dir, name);
    expect(events.some(e => e.type === "user.from-inside")).toBe(true);
  }, 15000);

  it("rejects non-user.* types with a clear error", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const r = runCli(dir, {}, "emit", name, "bogus-type");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/must start with/);
  }, 15000);

  it("errors when no ref and no $PTY_SESSION", async () => {
    const dir = makeSessionDir();
    // Run without PTY_SESSION in the env (parent process may have it set).
    const env: Record<string, string | undefined> = { ...process.env, PTY_SESSION_DIR: dir };
    delete env.PTY_SESSION;
    const r = spawnSync(nodeBin, [cliPath, "emit", "user.whatever"], {
      env: env as Record<string, string>,
      encoding: "utf-8",
      timeout: 10_000,
    });
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/not running inside a pty session|no session ref/);
  });

  it("--text puts the payload on the event's text field", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const r = runCli(dir, {}, "emit", name, "user.note", "--text", "checkpoint reached");
    expect(r.status).toBe(0);

    const events = readRecentEventsInDir(dir, name);
    const latest = events[events.length - 1];
    expect(latest.type).toBe("user.note");
    expect((latest as any).text).toBe("checkpoint reached");
    expect((latest as any).data).toBeUndefined();
  }, 15000);

  it("--json and --text together land both fields", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const r = runCli(dir, {}, "emit", name, "user.mixed",
      "--json", '{"ok":true}', "--text", "done");
    expect(r.status).toBe(0);

    const events = readRecentEventsInDir(dir, name);
    const latest = events[events.length - 1];
    expect((latest as any).data).toEqual({ ok: true });
    expect((latest as any).text).toBe("done");
  }, 15000);

  it("rejects invalid --json with a clear error and no event written", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const before = readRecentEventsInDir(dir, name).length;

    const r = runCli(dir, {}, "emit", name, "user.bad",
      "--json", "{not-valid-json");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/not valid JSON|--json/);

    const after = readRecentEventsInDir(dir, name).length;
    expect(after).toBe(before);
  }, 15000);
});

describe("appendEvent retention", () => {
  it("caps the events log when scripts write in a loop", async () => {
    // Regression: previously `appendEvent` (used by `pty emit` and the
    // `pty state set/delete` event emissions) did a raw fsp.appendFile
    // with no truncation, while the daemon's EventWriter enforces a
    // 1000-line cap. A script that ran `pty state set` in a tight loop
    // would grow events.jsonl without bound. Now appendEvent calls the
    // same retention path.
    process.env.PTY_SESSION_DIR = makeSessionDir();
    const name = uniqueName();

    // Write well past MAX_LINES (1000) to force at least one truncation.
    // Emit sequentially to keep the test deterministic.
    for (let i = 0; i < 1200; i++) {
      await emitUserEvent(name, "user.loop", { data: { i } });
    }

    const content = fs.readFileSync(
      path.join(process.env.PTY_SESSION_DIR!, `${name}.events.jsonl`),
      "utf-8"
    );
    const lineCount = content.trimEnd().split("\n").length;
    // After truncation kicks in the file should be at or below MAX_LINES.
    expect(lineCount).toBeLessThanOrEqual(1000);
    // And the truncation is tail-preserving: the most recent event (i=1199)
    // should still be in the file.
    expect(content).toContain('"i":1199');
  }, 30000);
});

describe("EventFollower — user.* streaming", () => {
  it("delivers user.* events to a live follower", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    // The follower reads PTY_SESSION_DIR from process.env when resolving
    // event file paths — point it at the test dir before construction.
    process.env.PTY_SESSION_DIR = dir;

    const received: EventRecord[] = [];
    const follower = new EventFollower({
      names: [name],
      onEvent: (e) => { if (e.type.startsWith("user.")) received.push(e); },
    });
    follower.start();

    try {
      // Give the watcher a moment to attach.
      await new Promise((r) => setTimeout(r, 100));

      await emitUserEvent(name, "user.first", { text: "one" });
      await emitUserEvent(name, "user.second", { data: { n: 2 } });

      // Poll for both — fs.watch can coalesce notifications.
      const deadline = Date.now() + 2000;
      while (received.length < 2 && Date.now() < deadline) {
        await new Promise((r) => setTimeout(r, 50));
      }

      expect(received.map(e => e.type)).toEqual(["user.first", "user.second"]);
      expect(isUserEvent(received[0])).toBe(true);
      expect((received[0] as any).text).toBe("one");
      expect((received[1] as any).data).toEqual({ n: 2 });
    } finally {
      follower.stop();
    }
  }, 15000);
});

// Helper: re-read events.jsonl from a specific session dir (can't rely on
// process.env.PTY_SESSION_DIR since readRecentEvents uses it directly).
function readRecentEventsInDir(dir: string, name: string): any[] {
  const content = fs.readFileSync(path.join(dir, `${name}.events.jsonl`), "utf-8");
  return content.trimEnd().split("\n").filter(Boolean).map(l => JSON.parse(l));
}
