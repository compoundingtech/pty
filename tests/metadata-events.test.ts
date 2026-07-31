// Tests for the new display_name_change / tags_change events that fire
// from setDisplayName / updateTags. Requested by pty-layout-claude so
// downstream consumers (pty-layout) can react to rename and tag
// mutations without polling the metadata file.

import { describe, it, expect, afterEach, afterAll } from "vitest";
import { terminateAndWait } from "./setup/processes.ts";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import * as net from "node:net";
import { fileURLToPath } from "node:url";
import { spawn, spawnSync } from "node:child_process";
import {
  acquireLock, patchMetadataById, readMetadata, releaseLock, setDisplayName,
  updateTags, writeMetadata,
} from "../src/sessions.ts";
import {
  acquireEventLock, EventFollower, formatEvent, releaseEventLock,
  type EventRecord,
} from "../src/events.ts";
import { encodeAttach } from "../src/protocol.ts";

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

async function startDaemon(
  sessionDir: string,
  name: string,
  overrides: Record<string, unknown> = {},
): Promise<number> {
  const config = JSON.stringify({
    name, command: "cat", args: [], displayCommand: "cat",
    cwd: os.tmpdir(), rows: 24, cols: 80,
    ...overrides,
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

async function attach(sessionDir: string, name: string): Promise<void> {
  await new Promise<void>((resolve, reject) => {
    const socket = net.createConnection(path.join(sessionDir, `${name}.sock`), () => {
      socket.write(encodeAttach(24, 80));
    });
    socket.once("data", () => {
      socket.destroy();
      resolve();
    });
    socket.once("error", reject);
  });
}

async function waitFor(
  predicate: () => boolean,
  description: string,
  timeoutMs = 3_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  throw new Error(`Timed out waiting for ${description}`);
}

function runCli(sessionDir: string, env: Record<string, string>, ...args: string[]) {
  return spawnSync(nodeBin, [cliPath, ...args], {
    env: { ...process.env, PTY_SESSION_DIR: sessionDir, ...env },
    encoding: "utf-8",
    timeout: 10_000,
  });
}

function runCliWithInput(sessionDir: string, input: string, ...args: string[]) {
  return spawnSync(nodeBin, [cliPath, ...args], {
    env: { ...process.env, PTY_SESSION_DIR: sessionDir },
    input,
    encoding: "utf-8",
    timeout: 10_000,
  });
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

function readEvents(dir: string, name: string): any[] {
  try {
    const content = fs.readFileSync(path.join(dir, `${name}.events.jsonl`), "utf-8");
    return content.trimEnd().split("\n").filter(Boolean).map(l => JSON.parse(l));
  } catch { return []; }
}

describe("patchMetadataById", () => {
  it("fails before changing metadata or events when event publication is locked", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;
    const metadataPath = path.join(dir, `${name}.json`);
    const eventsPath = path.join(dir, `${name}.events.jsonl`);
    const metadataBefore = fs.readFileSync(metadataPath);
    const eventsBefore = fs.readFileSync(eventsPath);
    expect(acquireEventLock(name)).toBe(true);

    try {
      await expect(patchMetadataById(name, {
        displayName: "Blocked",
        tags: { description: "x".repeat(1000) },
      })).rejects.toThrow(/event log is busy/i);
      expect(fs.readFileSync(metadataPath)).toEqual(metadataBefore);
      expect(fs.readFileSync(eventsPath)).toEqual(eventsBefore);
    } finally {
      releaseEventLock(name);
    }
  }, 15_000);

  it("preserves the complete recovery record and unknown future fields", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;
    const current = readMetadata(name)!;
    writeMetadata(name, {
      ...current,
      rows: 41,
      cols: 121,
      ephemeral: true,
      isolateEnv: true,
      extraEnv: { ASSIGNED: "yes" },
      unsetEnv: ["NO_COLOR"],
      futureRecoveryCapability: { version: 2 },
    } as typeof current);

    const result = await patchMetadataById(name, {
      displayName: "Recovery-safe",
      tags: { owner: "agent" },
    });

    expect(result.metadata).toMatchObject({
      rows: 41,
      cols: 121,
      ephemeral: true,
      isolateEnv: true,
      extraEnv: { ASSIGNED: "yes" },
      unsetEnv: ["NO_COLOR"],
      futureRecoveryCapability: { version: 2 },
      displayName: "Recovery-safe",
      tags: { owner: "agent" },
    });
  }, 15_000);

  it("daemon attach cannot write through a held metadata lock", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;
    await patchMetadataById(name, { displayName: "Locked", tags: { owner: "agent" } });
    const before = fs.readFileSync(path.join(dir, `${name}.json`), "utf8");
    expect(acquireLock(name)).toBe(true);

    try {
      await attach(dir, name);
      await new Promise((resolve) => setTimeout(resolve, 100));
      expect(fs.readFileSync(path.join(dir, `${name}.json`), "utf8")).toBe(before);
    } finally {
      releaseLock(name);
    }

    await attach(dir, name);
    await waitFor(() => readMetadata(name)?.lastAttachAt !== undefined, "post-lock attach metadata");
    expect(readMetadata(name)).toMatchObject({
      displayName: "Locked",
      tags: { owner: "agent" },
    });
  }, 15_000);

  it("exit metadata retries after a short external lock winner", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, {
      command: "sh",
      args: ["-c", "sleep 0.4; exit 7"],
      displayCommand: "exit 7",
      tags: { keep: "true" },
      unsetEnv: ["NO_COLOR"],
    });
    process.env.PTY_SESSION_DIR = dir;
    await patchMetadataById(name, { displayName: "Exiting", tags: { owner: "agent" } });
    expect(acquireLock(name)).toBe(true);

    try {
      await waitFor(
        () => readEvents(dir, name).some((event) => event.type === "session_exit"),
        "session_exit while metadata is locked",
      );
      await new Promise((resolve) => setTimeout(resolve, 100));
      expect(readMetadata(name)?.exitedAt).toBeUndefined();
    } finally {
      releaseLock(name);
    }

    await waitFor(() => readMetadata(name)?.exitedAt !== undefined, "close-time exit metadata flush");
    expect(readMetadata(name)).toMatchObject({
      exitCode: 7,
      displayName: "Exiting",
      tags: { keep: "true", owner: "agent" },
      unsetEnv: ["NO_COLOR"],
    });
  }, 15_000);

  it("atomically changes displayName and tags while preserving unrelated tags", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;
    updateTags(name, { keep: "yes", replace: "old", remove: "old" });

    const result = await patchMetadataById(name, {
      displayName: "Worker",
      tags: { replace: "new", remove: null, added: "yes" },
    });

    expect(result.changed).toBe(true);
    expect(result.metadata.displayName).toBe("Worker");
    expect(result.metadata.tags).toEqual({ keep: "yes", replace: "new", added: "yes" });
    const changes = readEvents(dir, name).filter((event) => event.type === "metadata_change");
    expect(changes).toHaveLength(1);
    expect(changes[0].previous).toEqual({
      displayName: null,
      tags: { replace: "old", remove: "old", added: null },
    });
    expect(changes[0].value).toEqual({
      displayName: "Worker",
      tags: { replace: "new", remove: null, added: "yes" },
    });
  }, 15000);

  it("supports clear operations and emits one coherent event", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;
    await patchMetadataById(name, { displayName: "Before", tags: { remove: "yes", keep: "yes" } });

    const result = await patchMetadataById(name, {
      displayName: null,
      tags: { remove: null },
    });

    expect(result.metadata.displayName).toBeUndefined();
    expect(result.metadata.tags).toEqual({ keep: "yes" });
    const changes = readEvents(dir, name).filter((event) => event.type === "metadata_change");
    expect(changes).toHaveLength(2);
    expect(changes[1].previous).toEqual({ displayName: "Before", tags: { remove: "yes" } });
    expect(changes[1].value).toEqual({ displayName: null, tags: { remove: null } });
  }, 15000);

  it("suppresses both the write result and event for a no-op", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;
    await patchMetadataById(name, { displayName: "Stable", tags: { role: "worker" } });
    const before = readEvents(dir, name).filter((event) => event.type === "metadata_change").length;

    const result = await patchMetadataById(name, {
      displayName: "Stable",
      tags: { role: "worker", absent: null },
    });

    expect(result.changed).toBe(false);
    expect(readEvents(dir, name).filter((event) => event.type === "metadata_change")).toHaveLength(before);
  }, 15000);

  it("never falls back from a missing stable id to a matching displayName", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;
    setDisplayName(name, "missing-id");

    await expect(patchMetadataById("missing-id", { tags: { wrong: "target" } }))
      .rejects.toThrow('Session id "missing-id" not found');
    expect(readMetadata(name)?.tags?.wrong).toBeUndefined();
  }, 15000);

  it.each([
    [{ displayName: " Worker" }, /Invalid displayName/],
    [{ displayName: "Worker\u2028Next" }, /Invalid displayName/],
    [{ displayName: "Worker\u2029Next" }, /Invalid displayName/],
    [{ displayName: "😀".repeat(161) }, /Invalid displayName/],
    [{ tags: { "": "value" } }, /tag keys must be non-empty/],
    [{ tags: { role: 1 } }, /tag values must be strings or null/],
    [{ unknown: true }, /unknown field "unknown"/],
  ])("rejects an invalid patch without writing metadata", async (patch, message) => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;
    const before = readMetadata(name);

    await expect(patchMetadataById(name, patch as any)).rejects.toThrow(message);
    expect(readMetadata(name)).toEqual(before);
    expect(readEvents(dir, name).filter((event) => event.type === "metadata_change")).toHaveLength(0);
  }, 15000);

  it("accepts the 160-scalar boundary with slash and backslash", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;
    const displayName = `${"😀".repeat(156)}/a\\b`;

    const result = await patchMetadataById(name, { displayName });

    expect(result.metadata.displayName).toBe(displayName);
  }, 15000);

  it("exposes the exact-id atomic operation through JSON stdin/stdout", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const result = runCliWithInput(
      dir,
      JSON.stringify({ displayName: "CLI Worker", tags: { role: "worker" } }),
      "metadata", "patch", "--id", name,
    );

    expect(result.status).toBe(0);
    expect(JSON.parse(result.stdout)).toMatchObject({
      changed: true,
      metadata: { displayName: "CLI Worker", tags: { role: "worker" } },
    });
    expect(readEvents(dir, name).filter((event) => event.type === "metadata_change")).toHaveLength(1);
  }, 15000);

  it("CLI exact-id lookup refuses a same-string displayName alias", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;
    setDisplayName(name, "missing-id");

    const result = runCliWithInput(
      dir,
      JSON.stringify({ tags: { wrong: "target" } }),
      "metadata", "patch", "--id", "missing-id",
    );

    expect(result.status).not.toBe(0);
    expect(result.stderr).toContain('Session id "missing-id" not found');
    expect(readMetadata(name)?.tags?.wrong).toBeUndefined();
  }, 15000);

  it.each([
    [[], "{}", /missing required --id/],
    [["--id", "target"], "not-json", /invalid JSON on stdin/],
    [["--id", "target"], "[]", /Metadata patch must be a JSON object/],
  ])("CLI reports actionable input errors", (args, input, message) => {
    const dir = makeSessionDir();
    const result = runCliWithInput(dir, input, "metadata", "patch", ...args);
    expect(result.status).not.toBe(0);
    expect(result.stderr).toMatch(message);
  });
});

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

  it("formats metadata_change with coherent previous/value snapshots", () => {
    const line = formatEvent({
      session: "test",
      type: "metadata_change",
      ts: "2026-04-23T10:15:03.000Z",
      previous: { displayName: null, tags: { role: null } },
      value: { displayName: "Worker", tags: { role: "worker" } },
    });
    expect(line).toContain("metadata ->");
    expect(line).toContain('"Worker"');
    expect(line).toContain('"worker"');
  });
});
