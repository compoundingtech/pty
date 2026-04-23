// Tests for the new display_name_change / tags_change events that fire
// from setDisplayName / updateTags. Requested by pty-layout-claude so
// downstream consumers (pty-layout) can react to rename and tag
// mutations without polling the metadata file.

import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, spawnSync } from "node:child_process";
import {
  setDisplayName, updateTags, readMetadata,
} from "../src/sessions.ts";
import { EventFollower, formatEvent, type EventRecord } from "../src/events.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const serverModule = path.join(__dirname, "..", "dist", "server.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-metaev-"));
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
  return `mev${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
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

function readEvents(dir: string, name: string): any[] {
  try {
    const content = fs.readFileSync(path.join(dir, `${name}.events.jsonl`), "utf-8");
    return content.trimEnd().split("\n").filter(Boolean).map(l => JSON.parse(l));
  } catch { return []; }
}

describe("setDisplayName — display_name_change event", () => {
  it("emits on a real change, with previous + value populated", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;

    setDisplayName(name, "my-label");

    const ev = readEvents(dir, name).find(e => e.type === "display_name_change");
    expect(ev).toBeTruthy();
    expect(ev.previous).toBeNull();
    expect(ev.value).toBe("my-label");
  }, 15000);

  it("emits on clear (value becomes null)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;

    setDisplayName(name, "initial");
    setDisplayName(name, null);

    const changes = readEvents(dir, name).filter(e => e.type === "display_name_change");
    expect(changes).toHaveLength(2);
    expect(changes[0].value).toBe("initial");
    expect(changes[1].previous).toBe("initial");
    expect(changes[1].value).toBeNull();
  }, 15000);

  it("does NOT emit on a no-op write (same value twice)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;

    setDisplayName(name, "stable");
    const afterFirst = readEvents(dir, name).filter(e => e.type === "display_name_change").length;
    setDisplayName(name, "stable"); // no-op
    const afterSecond = readEvents(dir, name).filter(e => e.type === "display_name_change").length;
    expect(afterSecond).toBe(afterFirst);
  }, 15000);

  it("does NOT emit on a no-op clear (was already null)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;

    const before = readEvents(dir, name).filter(e => e.type === "display_name_change").length;
    setDisplayName(name, null); // was never set
    const after = readEvents(dir, name).filter(e => e.type === "display_name_change").length;
    expect(after).toBe(before);
  }, 15000);

  it("fires from `pty rename` CLI too (end-to-end)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const r = runCli(dir, {}, "rename", name, "friendly");
    expect(r.status).toBe(0);

    const ev = readEvents(dir, name).find(e => e.type === "display_name_change");
    expect(ev).toBeTruthy();
    expect(ev.value).toBe("friendly");
  }, 15000);

  it("delivers live via EventFollower", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;

    const received: EventRecord[] = [];
    const follower = new EventFollower({
      names: [name],
      onEvent: (e) => { if (e.type === "display_name_change") received.push(e); },
    });
    follower.start();

    try {
      await new Promise((r) => setTimeout(r, 100));
      setDisplayName(name, "live-label");

      const deadline = Date.now() + 2000;
      while (received.length < 1 && Date.now() < deadline) {
        await new Promise((r) => setTimeout(r, 50));
      }
      expect(received.length).toBe(1);
      expect((received[0] as any).value).toBe("live-label");
    } finally {
      follower.stop();
    }
  }, 15000);
});

describe("updateTags — tags_change event", () => {
  it("emits with full previous + value snapshots when a tag is added", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;

    updateTags(name, { role: "web" });

    const ev = readEvents(dir, name).find(e => e.type === "tags_change");
    expect(ev).toBeTruthy();
    expect(ev.previous).toEqual({});
    expect(ev.value).toEqual({ role: "web" });
  }, 15000);

  it("emits with previous carrying existing tags + value reflecting the merge", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;

    updateTags(name, { role: "web" });
    updateTags(name, { owner: "forge" });

    const changes = readEvents(dir, name).filter(e => e.type === "tags_change");
    expect(changes).toHaveLength(2);
    expect(changes[1].previous).toEqual({ role: "web" });
    expect(changes[1].value).toEqual({ role: "web", owner: "forge" });
  }, 15000);

  it("emits when a tag is removed", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;

    updateTags(name, { a: "1", b: "2" });
    updateTags(name, {}, ["a"]);

    const changes = readEvents(dir, name).filter(e => e.type === "tags_change");
    expect(changes).toHaveLength(2);
    expect(changes[1].previous).toEqual({ a: "1", b: "2" });
    expect(changes[1].value).toEqual({ b: "2" });
  }, 15000);

  it("does NOT emit on a no-op (same value for an existing key)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;

    updateTags(name, { role: "web" });
    const before = readEvents(dir, name).filter(e => e.type === "tags_change").length;
    updateTags(name, { role: "web" }); // no-op — same key, same value
    const after = readEvents(dir, name).filter(e => e.type === "tags_change").length;
    expect(after).toBe(before);
  }, 15000);

  it("does NOT emit when the `removals` list doesn't intersect current keys", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;

    updateTags(name, { role: "web" });
    const before = readEvents(dir, name).filter(e => e.type === "tags_change").length;
    updateTags(name, {}, ["never-was-set"]);
    const after = readEvents(dir, name).filter(e => e.type === "tags_change").length;
    expect(after).toBe(before);
  }, 15000);

  it("fires from `pty tag` CLI too (end-to-end)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const r = runCli(dir, {}, "tag", name, "role=web");
    expect(r.status).toBe(0);

    const ev = readEvents(dir, name).find(e => e.type === "tags_change");
    expect(ev).toBeTruthy();
    expect(ev.value.role).toBe("web");
  }, 15000);

  it("delivers live via EventFollower", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;

    const received: EventRecord[] = [];
    const follower = new EventFollower({
      names: [name],
      onEvent: (e) => { if (e.type === "tags_change") received.push(e); },
    });
    follower.start();

    try {
      await new Promise((r) => setTimeout(r, 100));
      updateTags(name, { live: "yes" });

      const deadline = Date.now() + 2000;
      while (received.length < 1 && Date.now() < deadline) {
        await new Promise((r) => setTimeout(r, 50));
      }
      expect(received.length).toBe(1);
      expect((received[0] as any).value).toEqual({ live: "yes" });
    } finally {
      follower.stop();
    }
  }, 15000);
});

describe("formatEvent for new metadata events", () => {
  it("formats display_name_change showing both previous and new", () => {
    const line = formatEvent({
      session: "test",
      type: "display_name_change",
      ts: "2026-04-23T10:15:03.000Z",
      previous: "old",
      value: "new",
    });
    expect(line).toContain("display_name ->");
    expect(line).toContain('"new"');
    expect(line).toContain('"old"');
  });

  it("formats display_name_change cleanly when clearing (value=null)", () => {
    const line = formatEvent({
      session: "test",
      type: "display_name_change",
      ts: "2026-04-23T10:15:03.000Z",
      previous: "old",
      value: null,
    });
    expect(line).toContain("null");
    expect(line).toContain('"old"');
  });

  it("formats tags_change as a space-separated k=v listing", () => {
    const line = formatEvent({
      session: "test",
      type: "tags_change",
      ts: "2026-04-23T10:15:03.000Z",
      previous: { role: "web" },
      value: { role: "web", owner: "forge" },
    });
    expect(line).toContain("tags ->");
    expect(line).toContain("role=web");
    expect(line).toContain("owner=forge");
  });

  it("formats tags_change with empty maps as {}", () => {
    const line = formatEvent({
      session: "test",
      type: "tags_change",
      ts: "2026-04-23T10:15:03.000Z",
      previous: { role: "web" },
      value: {},
    });
    expect(line).toContain("{}");
  });
});
