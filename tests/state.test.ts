// Tests for `pty state` and the underlying sessions.ts state helpers.

import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, spawnSync } from "node:child_process";
import {
  getState, getStateKey, setState, deleteState, listStateKeys,
} from "../src/sessions.ts";
import { EventFollower, type EventRecord } from "../src/events.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const serverModule = path.join(__dirname, "..", "dist", "server.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-state-"));
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
  return `st${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
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
    input: env.STDIN ?? undefined,
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

describe("state helpers (direct API)", () => {
  it("setState / getStateKey round-trip complex values", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;

    setState(name, "port", 3000);
    setState(name, "config", { host: "localhost", tls: { cert: "x.pem" } });
    setState(name, "peers", ["a", "b", "c"]);

    expect(getStateKey(name, "port")).toBe(3000);
    expect(getStateKey(name, "config")).toEqual({ host: "localhost", tls: { cert: "x.pem" } });
    expect(getStateKey(name, "peers")).toEqual(["a", "b", "c"]);
  }, 15000);

  it("getState returns the whole bag", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;

    setState(name, "a", 1);
    setState(name, "b", "two");
    const bag = getState(name);
    expect(bag).toEqual({ a: 1, b: "two" });
  }, 15000);

  it("getState returns a fresh copy — mutating doesn't affect stored state", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;

    setState(name, "a", 1);
    const bag = getState(name);
    bag.a = 999;
    expect(getStateKey(name, "a")).toBe(1);
  }, 15000);

  it("deleteState removes the key; deleting the last key drops the field", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;

    setState(name, "only", 1);
    deleteState(name, "only");
    const meta = JSON.parse(fs.readFileSync(path.join(dir, `${name}.json`), "utf-8"));
    expect(meta.state).toBeUndefined();
  }, 15000);

  it("listStateKeys returns the keys", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;

    setState(name, "a", 1);
    setState(name, "b", 2);
    expect(listStateKeys(name).sort()).toEqual(["a", "b"]);
  }, 15000);

  it("throws on unknown session", () => {
    process.env.PTY_SESSION_DIR = makeSessionDir();
    expect(() => getState("no-such-session")).toThrow(/not found/);
    expect(() => setState("no-such-session", "k", 1)).toThrow(/not found/);
  });
});

describe("pty state CLI", () => {
  it("set then get round-trips JSON", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const setR = runCli(dir, {}, "state", "set", name, "port", "3000");
    expect(setR.status).toBe(0);

    const getR = runCli(dir, {}, "state", "get", name, "port");
    expect(getR.status).toBe(0);
    expect(getR.stdout.trim()).toBe("3000");
  }, 15000);

  it("set emits a state.set event; delete emits state.delete", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    runCli(dir, {}, "state", "set", name, "foo", '"bar"');
    runCli(dir, {}, "state", "delete", name, "foo");

    const events = readEvents(dir, name);
    const set = events.find(e => e.type === "state.set" && e.key === "foo");
    const del = events.find(e => e.type === "state.delete" && e.key === "foo");
    expect(set).toBeTruthy();
    expect(set.value).toBe("bar");
    expect(del).toBeTruthy();
  }, 15000);

  it("get with no key prints the whole bag as pretty JSON", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    runCli(dir, {}, "state", "set", name, "a", "1");
    runCli(dir, {}, "state", "set", name, "b", "2");

    const r = runCli(dir, {}, "state", "get", name);
    expect(r.status).toBe(0);
    expect(JSON.parse(r.stdout)).toEqual({ a: 1, b: 2 });
  }, 15000);

  it("get on a missing key exits non-zero without printing undefined", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const r = runCli(dir, {}, "state", "get", name, "missing");
    expect(r.status).not.toBe(0);
    expect(r.stdout).toBe("");
  }, 15000);

  it("keys lists keys one per line", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    runCli(dir, {}, "state", "set", name, "alpha", "1");
    runCli(dir, {}, "state", "set", name, "beta",  "2");

    const r = runCli(dir, {}, "state", "keys", name);
    expect(r.status).toBe(0);
    expect(r.stdout.trim().split("\n").sort()).toEqual(["alpha", "beta"]);
  }, 15000);

  it("resolves to $PTY_SESSION when ref is omitted", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const setR = runCli(dir, { PTY_SESSION: name }, "state", "set", "inside-port", "4242");
    expect(setR.status).toBe(0);

    const getR = runCli(dir, { PTY_SESSION: name }, "state", "get", "inside-port");
    expect(getR.status).toBe(0);
    expect(getR.stdout.trim()).toBe("4242");
  }, 15000);

  it("set value from stdin when not given as an arg", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const r = runCli(dir, { STDIN: '{"big":{"payload":[1,2,3]}}' }, "state", "set", name, "config");
    expect(r.status).toBe(0);

    const getR = runCli(dir, {}, "state", "get", name, "config");
    expect(JSON.parse(getR.stdout)).toEqual({ big: { payload: [1, 2, 3] } });
  }, 15000);

  it("invalid JSON value rejected with a helpful message", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const r = runCli(dir, {}, "state", "set", name, "k", "not-json");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/not valid JSON/);
  }, 15000);

  it("prototype-chain names don't leak through get/delete/keys", async () => {
    // Regression: the original code used `metadata.state?.[key]` (inherits
    // through the prototype chain) and `key in metadata.state` (same).
    // Meant `getStateKey(name, "toString")` would return the Function
    // prototype method, and `pty state delete name toString` would emit a
    // ghost `state.delete` event because `in` said the key "existed."
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;

    // Prime the bag with one real key so metadata.state exists.
    setState(name, "real", 1);

    for (const name2 of ["toString", "hasOwnProperty", "constructor", "__proto__"]) {
      expect(getStateKey(name, name2)).toBeUndefined();
    }

    const before = readEvents(dir, name).filter(e => e.type === "state.delete").length;
    const r = runCli(dir, {}, "state", "delete", name, "toString");
    expect(r.status).toBe(0);
    const after = readEvents(dir, name).filter(e => e.type === "state.delete").length;
    expect(after).toBe(before); // no ghost event for inherited name

    // And `keys` still reports only real own-properties.
    expect(listStateKeys(name)).toEqual(["real"]);
  }, 15000);

  it("keys rejects extra positional args (consistent with get/set/delete)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const r = runCli(dir, {}, "state", "keys", name, "typo");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/unexpected/i);
  }, 15000);

  it("rejects too many positional args (forces the user to quote their JSON)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    // Unquoted JSON gets split by the shell and joined by the CLI into garbage.
    // Previous behavior silently joined `extra1 extra2` with a space and tried
    // JSON.parse on it. Now we reject the whole shape with a helpful message.
    const r = runCli(dir, {}, "state", "set", name, "key", "extra1", "extra2");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/too many positional|quote/i);
  }, 15000);

  it("delete on a missing key is a quiet no-op — no ghost state.delete event", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    const before = readEvents(dir, name).filter(e => e.type === "state.delete").length;

    const r = runCli(dir, {}, "state", "delete", name, "never-existed");
    expect(r.status).toBe(0);

    const after = readEvents(dir, name).filter(e => e.type === "state.delete").length;
    expect(after).toBe(before); // no new state.delete event fired
  }, 15000);

  it("heuristic: first positional that names an existing session wins as ref, not key", async () => {
    // Documents current resolveStateTarget() behavior. Two sessions in the
    // same dir: `inside` (the session we're pretending to be running under)
    // and `other`. Running `pty state set other 42` with PTY_SESSION=inside
    // targets session `other` with key=... wait, there's no key. That'd error.
    // Instead use: `pty state set other mykey 1`. The heuristic sees `other`
    // is a known session → treats it as ref, key=mykey, value=1.
    //
    // The footgun: if `mykey` collides with a session name, a future
    // refactor might re-shuffle positional resolution. This test locks the
    // current rule in place so the ambiguity can't silently drift.
    const dir = makeSessionDir();
    const inside = uniqueName();
    const other = uniqueName();
    await startDaemon(dir, inside);
    await startDaemon(dir, other);

    // With PTY_SESSION=inside, `state set <other's real name> mykey 1`
    // should target `other` (heuristic: first positional is a known session).
    const r = runCli(dir, { PTY_SESSION: inside }, "state", "set", other, "mykey", "1");
    expect(r.status).toBe(0);

    // Wrong interpretation (key="other" on `inside`) would put { other: "mykey" }
    // somewhere. Verify that didn't happen.
    process.env.PTY_SESSION_DIR = dir;
    expect(getStateKey(other, "mykey")).toBe(1);
    expect(getState(inside)).toEqual({});
  }, 20000);

  it("heuristic: an unknown-session-named first arg falls back to $PTY_SESSION and is treated as a key", async () => {
    // Mirror of the above: when the first positional is NOT a known session,
    // it's interpreted as the key instead. Same resolveStateTarget() path —
    // different branch.
    const dir = makeSessionDir();
    const inside = uniqueName();
    await startDaemon(dir, inside);

    // `notasession` isn't the name of any session in `dir`, so this becomes
    // `state set <inside> notasession "42"`.
    const r = runCli(dir, { PTY_SESSION: inside }, "state", "set", "notasession", "42");
    expect(r.status).toBe(0);

    process.env.PTY_SESSION_DIR = dir;
    expect(getStateKey(inside, "notasession")).toBe(42);
  }, 15000);

  it("setState under in-process Promise.all: every update lands", async () => {
    // Node's main loop is single-threaded and setState is synchronous —
    // Promise.all serializes these on the event loop with no interleaving
    // between a read and its matching write. Any lost update here means a
    // subtle bug (e.g., async RMW leaking in) we want to catch immediately.
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;

    const KEYS = 20;
    await Promise.all(
      Array.from({ length: KEYS }, (_, i) => Promise.resolve().then(() => setState(name, `k${i}`, i)))
    );

    const bag = getState(name);
    expect(Object.keys(bag).length).toBe(KEYS);
    for (let i = 0; i < KEYS; i++) {
      expect(bag[`k${i}`]).toBe(i);
    }
  }, 15000);
});

describe("state helpers emit events from the programmatic API", () => {
  // Regression: pty-layout et al. use setState/deleteState via the client
  // API, not via the CLI. Prior to this fix, events only fired from the
  // CLI wrapper — programmatic callers got silent writes.
  it("setState emits state.set even when called directly (no CLI involved)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;

    setState(name, "port", 9000);

    const events = readEvents(dir, name);
    const set = events.find(e => e.type === "state.set" && e.key === "port");
    expect(set).toBeTruthy();
    expect(set.value).toBe(9000);
  }, 15000);

  it("deleteState emits state.delete when something was removed", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;

    setState(name, "x", 1);
    const before = readEvents(dir, name).filter(e => e.type === "state.delete").length;
    const removed = deleteState(name, "x");
    expect(removed).toBe(true);
    const after = readEvents(dir, name).filter(e => e.type === "state.delete").length;
    expect(after).toBe(before + 1);
  }, 15000);

  it("deleteState on a missing key writes no event and returns false", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);
    process.env.PTY_SESSION_DIR = dir;

    const before = readEvents(dir, name).filter(e => e.type === "state.delete").length;
    const removed = deleteState(name, "never-existed");
    expect(removed).toBe(false);
    const after = readEvents(dir, name).filter(e => e.type === "state.delete").length;
    expect(after).toBe(before);
  }, 15000);
});

describe("EventFollower — state.* streaming", () => {
  it("delivers state.set and state.delete events live", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name);

    process.env.PTY_SESSION_DIR = dir;

    const received: EventRecord[] = [];
    const follower = new EventFollower({
      names: [name],
      onEvent: (e) => { if (e.type === "state.set" || e.type === "state.delete") received.push(e); },
    });
    follower.start();

    try {
      await new Promise((r) => setTimeout(r, 100));

      // Drive mutations through the CLI so we test the actual shipping path,
      // not an internal shortcut that bypasses event emission.
      const s = runCli(dir, {}, "state", "set", name, "port", "3000");
      expect(s.status).toBe(0);
      const d = runCli(dir, {}, "state", "delete", name, "port");
      expect(d.status).toBe(0);

      const deadline = Date.now() + 2000;
      while (received.length < 2 && Date.now() < deadline) {
        await new Promise((r) => setTimeout(r, 50));
      }

      expect(received.map(e => e.type)).toEqual(["state.set", "state.delete"]);
      expect((received[0] as any).key).toBe("port");
      expect((received[0] as any).value).toBe(3000);
      expect((received[1] as any).key).toBe("port");
    } finally {
      follower.stop();
    }
  }, 20000);
});
