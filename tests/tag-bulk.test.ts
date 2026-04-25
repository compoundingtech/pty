// Tests for the bulk-operation contract on `pty tag`. The single-set and
// single-rm forms have been there since v0.x; this file pins down the
// multi-key matrix (multiple sets, multiple --rm, set+rm in one call,
// interleaved order, set/rm of the same key) and the error-surface
// (missing =, empty key, --rm without arg).
//
// Contract (kept in sync with src/sessions.ts updateTags):
//   1. Updates are applied before removals. So `pty tag X k=v --rm k`
//      sets k then removes it → final state is removed.
//   2. Same-key duplicates within a single call: last positional wins
//      for sets; --rm is idempotent.
//   3. Bulk operations fan into ONE writeMetadata call → ONE
//      tags_change event with full before/after snapshots.
//   4. No-op bulk operations (effective tags unchanged) fire no event.
//   5. Empty key (`=value`) and `--rm` at end-without-arg are rejected
//      with a clear error.

import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, spawnSync } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const serverModule = path.join(__dirname, "..", "dist", "server.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-tagbulk-"));
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
  return `tb${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
}

async function startDaemon(
  sessionDir: string,
  name: string,
  initialTags?: Record<string, string>,
  displayName?: string,
): Promise<number> {
  const config = JSON.stringify({
    name, command: "cat", args: [], displayCommand: "cat",
    cwd: os.tmpdir(), rows: 24, cols: 80,
    ...(initialTags ? { tags: initialTags } : {}),
    ...(displayName ? { displayName } : {}),
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

function runCli(sessionDir: string, ...args: string[]) {
  return spawnSync(nodeBin, [cliPath, ...args], {
    env: { ...process.env, PTY_SESSION_DIR: sessionDir },
    encoding: "utf-8",
    timeout: 10_000,
  });
}

function readMeta(sessionDir: string, name: string) {
  return JSON.parse(fs.readFileSync(path.join(sessionDir, `${name}.json`), "utf-8"));
}

function readEvents(dir: string, name: string): any[] {
  try {
    const content = fs.readFileSync(path.join(dir, `${name}.events.jsonl`), "utf-8");
    return content.trimEnd().split("\n").filter(Boolean).map((l) => JSON.parse(l));
  } catch { return []; }
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

describe("pty tag — bulk set", () => {
  it("sets a single key=value (back-compat)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const r = runCli(dir, "tag", name, "role=web");
    expect(r.status).toBe(0);
    expect(readMeta(dir, name).tags).toEqual({ role: "web" });
  }, 15000);

  it("sets multiple keys in one call", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const r = runCli(dir, "tag", name, "role=web", "owner=forge", "env=dev");
    expect(r.status).toBe(0);
    expect(readMeta(dir, name).tags).toEqual({
      role: "web", owner: "forge", env: "dev",
    });
  }, 15000);

  it("same key set twice in one call: last value wins", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const r = runCli(dir, "tag", name, "color=red", "color=blue");
    expect(r.status).toBe(0);
    expect(readMeta(dir, name).tags.color).toBe("blue");
  }, 15000);

  it("merges with existing tags rather than replacing", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, { existing: "yes" });

    runCli(dir, "tag", name, "fresh=1", "another=2");
    expect(readMeta(dir, name).tags).toEqual({
      existing: "yes", fresh: "1", another: "2",
    });
  }, 15000);

  it("allows empty value (key=)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const r = runCli(dir, "tag", name, "key=");
    expect(r.status).toBe(0);
    expect(readMeta(dir, name).tags).toEqual({ key: "" });
  }, 15000);

  it("multi-= splits on the first equals (key='foo', value='bar=baz')", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    runCli(dir, "tag", name, "foo=bar=baz");
    expect(readMeta(dir, name).tags).toEqual({ foo: "bar=baz" });
  }, 15000);

  it("supports many tags in a single call (atomicity sanity)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const args = Array.from({ length: 30 }, (_, i) => `k${i}=${i}`);
    const r = runCli(dir, "tag", name, ...args);
    expect(r.status).toBe(0);
    const tags = readMeta(dir, name).tags;
    expect(Object.keys(tags).length).toBe(30);
    for (let i = 0; i < 30; i++) expect(tags[`k${i}`]).toBe(String(i));
  }, 20000);
});

describe("pty tag — bulk remove", () => {
  it("removes a single key (back-compat)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, { a: "1", b: "2" });

    runCli(dir, "tag", name, "--rm", "a");
    expect(readMeta(dir, name).tags).toEqual({ b: "2" });
  }, 15000);

  it("removes multiple keys in one call", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, { a: "1", b: "2", c: "3" });

    runCli(dir, "tag", name, "--rm", "a", "--rm", "c");
    expect(readMeta(dir, name).tags).toEqual({ b: "2" });
  }, 15000);

  it("rm of nonexistent key: silent no-op (no error, exit 0)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, { a: "1" });

    const r = runCli(dir, "tag", name, "--rm", "never-was-set");
    expect(r.status).toBe(0);
    expect(readMeta(dir, name).tags).toEqual({ a: "1" });
  }, 15000);

  it("rm of same key twice in one call: idempotent (final: removed)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, { dup: "1" });

    runCli(dir, "tag", name, "--rm", "dup", "--rm", "dup");
    expect(readMeta(dir, name).tags).toBeUndefined();
  }, 15000);

  it("removing every tag drops the `tags` field entirely", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, { only: "x" });

    runCli(dir, "tag", name, "--rm", "only");
    expect(readMeta(dir, name).tags).toBeUndefined();
  }, 15000);
});

describe("pty tag — combined set + remove", () => {
  it("combines set and rm in one call", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, { keep: "yes", drop: "yes" });

    runCli(dir, "tag", name, "added=new", "--rm", "drop");
    expect(readMeta(dir, name).tags).toEqual({ keep: "yes", added: "new" });
  }, 15000);

  it("set+rm same key in one call: rm wins (updates apply first, then removals)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    // Document the contract: `--rm` runs after sets, so this is equivalent
    // to "set k=v then immediately remove k" — final state is removed.
    runCli(dir, "tag", name, "k=v", "--rm", "k");
    expect(readMeta(dir, name).tags).toBeUndefined();
  }, 15000);

  it("position-independence: set + rm in any order produce the same result", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    const b = uniqueName();
    await startDaemon(dir, a, { drop: "yes" });
    await startDaemon(dir, b, { drop: "yes" });

    runCli(dir, "tag", a, "fresh=1", "--rm", "drop", "another=2");
    runCli(dir, "tag", b, "--rm", "drop", "another=2", "fresh=1");

    expect(readMeta(dir, a).tags).toEqual(readMeta(dir, b).tags);
    expect(readMeta(dir, a).tags).toEqual({ fresh: "1", another: "2" });
  }, 15000);

  it("interleaved set / rm / set / rm preserves the apply-updates-first contract", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, { x: "old", y: "keep" });

    // Sequence: set x=new, --rm y, set z=new, --rm x.
    // Apply order: updates first (x=new, z=new), then removals (rm y, rm x).
    // Final: { z: "new" }. y was removed. x was set then removed.
    runCli(dir, "tag", name, "x=new", "--rm", "y", "z=new", "--rm", "x");
    expect(readMeta(dir, name).tags).toEqual({ z: "new" });
  }, 15000);
});

describe("pty tag — error surface", () => {
  it("rejects a positional missing the equals sign", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const r = runCli(dir, "tag", name, "no-equals-here");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/key=value|--rm/);
    // No write should have happened.
    expect(readMeta(dir, name).tags).toBeUndefined();
  }, 15000);

  it("rejects empty-key positional like `=value`", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const r = runCli(dir, "tag", name, "=value");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/key/i);
    expect(readMeta(dir, name).tags).toBeUndefined();
  }, 15000);

  it("rejects --rm at the end of argv with no key", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, { keep: "yes" });

    const r = runCli(dir, "tag", name, "--rm");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/--rm/);
    // Original tags untouched.
    expect(readMeta(dir, name).tags).toEqual({ keep: "yes" });
  }, 15000);

  it("rejects --rm with an empty key (`--rm \"\"`)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const r = runCli(dir, "tag", name, "--rm", "");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/key/i);
  }, 15000);

  it("errors on a nonexistent session ref", () => {
    const dir = makeSessionDir();
    const r = runCli(dir, "tag", "no-such-session", "k=v");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/not found/);
  }, 15000);

  it("rejects bad shape early — no partial application across multiple positionals", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    // A valid set followed by an invalid positional. We want either-all-or-
    // nothing semantics: the valid one should NOT land if the call as a
    // whole fails. (Atomic at the file level — there's only one writeMetadata
    // call, so this is automatically true; this test pins it down.)
    const r = runCli(dir, "tag", name, "good=yes", "no-equals");
    expect(r.status).not.toBe(0);
    expect(readMeta(dir, name).tags).toBeUndefined();
  }, 15000);
});

describe("pty tag — events", () => {
  it("bulk write fires exactly one tags_change event", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const before = readEvents(dir, name).filter((e) => e.type === "tags_change").length;
    runCli(dir, "tag", name, "a=1", "b=2", "c=3", "--rm", "z");
    const after = readEvents(dir, name).filter((e) => e.type === "tags_change");
    expect(after.length).toBe(before + 1);
    // The single event carries the full snapshot.
    const last = after[after.length - 1];
    expect(last.previous).toEqual({});
    expect(last.value).toEqual({ a: "1", b: "2", c: "3" });
  }, 15000);

  it("no-op bulk write fires no event (setting current values + rm of nonexistent)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, { stable: "x" });

    const before = readEvents(dir, name).filter((e) => e.type === "tags_change").length;
    // Setting `stable=x` (current value) and removing `nope` (not present).
    // Effective tags are unchanged → no event.
    runCli(dir, "tag", name, "stable=x", "--rm", "nope");
    const after = readEvents(dir, name).filter((e) => e.type === "tags_change").length;
    expect(after).toBe(before);
  }, 15000);

  it("partial no-op + real change still fires one event", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, { same: "x" });

    const before = readEvents(dir, name).filter((e) => e.type === "tags_change").length;
    runCli(dir, "tag", name, "same=x", "new=y"); // same is no-op, new is a real add.
    const after = readEvents(dir, name).filter((e) => e.type === "tags_change");
    expect(after.length).toBe(before + 1);
    expect(after[after.length - 1].value).toEqual({ same: "x", new: "y" });
  }, 15000);

  it("set+rm same key (rm wins) is still a no-op when the key was never present", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const before = readEvents(dir, name).filter((e) => e.type === "tags_change").length;
    // Set k=v, then --rm k, on a session with no `k` → effective change is
    // zero, so no event.
    runCli(dir, "tag", name, "k=v", "--rm", "k");
    const after = readEvents(dir, name).filter((e) => e.type === "tags_change").length;
    expect(after).toBe(before);
  }, 15000);
});

describe("pty tag — reads + resolution", () => {
  it("no positional args: dumps current tags (back-compat)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, { role: "web" });

    const r = runCli(dir, "tag", name);
    expect(r.status).toBe(0);
    expect(r.stdout).toContain("role=web");
  }, 15000);

  it("dump on empty: 'No tags' message", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const r = runCli(dir, "tag", name);
    expect(r.stdout).toMatch(/No tags/);
  }, 15000);

  it("resolves displayName as the session ref for bulk ops", async () => {
    const dir = makeSessionDir();
    const stableId = uniqueName();
    const friendly = `friendly-${Math.random().toString(36).slice(2, 6)}`;
    await startDaemon(dir, stableId, undefined, friendly);

    const r = runCli(dir, "tag", friendly, "via=displayname", "another=ok");
    expect(r.status).toBe(0);
    expect(readMeta(dir, stableId).tags).toEqual({
      via: "displayname", another: "ok",
    });
  }, 15000);
});
