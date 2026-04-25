// `pty tag-multi` — multi-session tag operations (read + write).
//
// Contract recap:
//   Selectors (mutually exclusive, pick one):
//     <name>...               explicit list (resolved before any write)
//     --filter-tag k=v ...    matching sessions (AND across multiple)
//     --all                   every session
//   Read mode = no ops; emit tags per session, JSON or text.
//   Write mode = any ops (k=v / --rm k); apply to each selected session.
//   --all + write requires --yes.
//   Empty match is exit 0 with "0 sessions matched."
//   Explicit-list with an unresolvable name: fail upfront, no writes.
//   Per-session writes are individually atomic (one tags_change event each).

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

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-tagmulti-"));
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
  return `tm${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
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

// =============================================================================
// READ MODE — explicit list
// =============================================================================

describe("pty tag-multi — read mode (explicit list)", () => {
  it("dumps tags for a single named session", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    await startDaemon(dir, a, { role: "web" });

    const r = runCli(dir, "tag-multi", a);
    expect(r.status).toBe(0);
    expect(r.stdout).toContain(a);
    expect(r.stdout).toContain("role=web");
  }, 15000);

  it("dumps tags for multiple named sessions in argv order", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    const b = uniqueName();
    await startDaemon(dir, a, { role: "web" });
    await startDaemon(dir, b, { role: "db" });

    const r = runCli(dir, "tag-multi", a, b);
    expect(r.status).toBe(0);
    expect(r.stdout).toContain(a);
    expect(r.stdout).toContain(b);
    expect(r.stdout).toContain("role=web");
    expect(r.stdout).toContain("role=db");
  }, 15000);

  it("--json emits an object keyed by session name", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    const b = uniqueName();
    await startDaemon(dir, a, { role: "web" });
    await startDaemon(dir, b, { env: "dev" });

    const r = runCli(dir, "tag-multi", a, b, "--json");
    expect(r.status).toBe(0);
    const parsed = JSON.parse(r.stdout);
    expect(parsed[a]).toEqual({ role: "web" });
    expect(parsed[b]).toEqual({ env: "dev" });
  }, 15000);

  it("a session without tags renders as an empty object in JSON", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    await startDaemon(dir, a);

    const r = runCli(dir, "tag-multi", a, "--json");
    expect(r.status).toBe(0);
    expect(JSON.parse(r.stdout)).toEqual({ [a]: {} });
  }, 15000);

  it("resolves displayNames in the explicit list", async () => {
    const dir = makeSessionDir();
    const stableId = uniqueName();
    const friendly = `friendly-${Math.random().toString(36).slice(2, 6)}`;
    await startDaemon(dir, stableId, { role: "web" }, friendly);

    const r = runCli(dir, "tag-multi", friendly, "--json");
    expect(r.status).toBe(0);
    // Resolved key in the output is the stable id, not the displayName.
    expect(JSON.parse(r.stdout)).toEqual({ [stableId]: { role: "web" } });
  }, 15000);

  it("errors out with no writes when one of the explicit names is unresolvable", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    await startDaemon(dir, a);

    const r = runCli(dir, "tag-multi", a, "no-such-session");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/not found|no-such-session/);
  }, 15000);
});

// =============================================================================
// READ MODE — selectors
// =============================================================================

describe("pty tag-multi — read mode (selectors)", () => {
  it("--filter-tag matches sessions with that tag", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    const b = uniqueName();
    const c = uniqueName();
    await startDaemon(dir, a, { role: "web" });
    await startDaemon(dir, b, { role: "db" });
    await startDaemon(dir, c, { role: "web" });

    const r = runCli(dir, "tag-multi", "--filter-tag", "role=web", "--json");
    expect(r.status).toBe(0);
    const parsed = JSON.parse(r.stdout);
    expect(Object.keys(parsed).sort()).toEqual([a, c].sort());
  }, 20000);

  it("multiple --filter-tag ANDs the conditions", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    const b = uniqueName();
    await startDaemon(dir, a, { role: "web", env: "prod" });
    await startDaemon(dir, b, { role: "web", env: "dev" });

    const r = runCli(dir, "tag-multi", "--filter-tag", "role=web", "--filter-tag", "env=prod", "--json");
    expect(r.status).toBe(0);
    expect(JSON.parse(r.stdout)).toEqual({ [a]: { role: "web", env: "prod" } });
  }, 15000);

  it("--filter-tag with zero matches: exit 0, empty JSON object", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    await startDaemon(dir, a, { role: "web" });

    const r = runCli(dir, "tag-multi", "--filter-tag", "role=ghost", "--json");
    expect(r.status).toBe(0);
    expect(JSON.parse(r.stdout)).toEqual({});
  }, 15000);

  it("--all emits every session (read is safe, no --yes needed)", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    const b = uniqueName();
    await startDaemon(dir, a, { role: "web" });
    await startDaemon(dir, b, { role: "db" });

    const r = runCli(dir, "tag-multi", "--all", "--json");
    expect(r.status).toBe(0);
    const parsed = JSON.parse(r.stdout);
    expect(Object.keys(parsed).sort()).toEqual([a, b].sort());
  }, 15000);

  it("--all on an empty session dir: exit 0, empty JSON", () => {
    const dir = makeSessionDir();
    const r = runCli(dir, "tag-multi", "--all", "--json");
    expect(r.status).toBe(0);
    expect(JSON.parse(r.stdout)).toEqual({});
  }, 15000);
});

// =============================================================================
// WRITE MODE — explicit list
// =============================================================================

describe("pty tag-multi — write mode (explicit list)", () => {
  it("sets a tag on each named session", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    const b = uniqueName();
    await startDaemon(dir, a);
    await startDaemon(dir, b);

    const r = runCli(dir, "tag-multi", a, b, "audit=2026-04-25");
    expect(r.status).toBe(0);

    expect(readMeta(dir, a).tags).toEqual({ audit: "2026-04-25" });
    expect(readMeta(dir, b).tags).toEqual({ audit: "2026-04-25" });
  }, 15000);

  it("removes a tag on each named session", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    const b = uniqueName();
    await startDaemon(dir, a, { role: "web", env: "prod" });
    await startDaemon(dir, b, { role: "web" });

    const r = runCli(dir, "tag-multi", a, b, "--rm", "role");
    expect(r.status).toBe(0);

    expect(readMeta(dir, a).tags).toEqual({ env: "prod" });
    expect(readMeta(dir, b).tags).toBeUndefined();
  }, 15000);

  it("combined set + rm in one call applies to each session", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    const b = uniqueName();
    await startDaemon(dir, a, { drop: "yes" });
    await startDaemon(dir, b, { drop: "yes" });

    runCli(dir, "tag-multi", a, b, "fresh=1", "--rm", "drop");
    expect(readMeta(dir, a).tags).toEqual({ fresh: "1" });
    expect(readMeta(dir, b).tags).toEqual({ fresh: "1" });
  }, 15000);

  it("each successful write fires its own tags_change event", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    const b = uniqueName();
    await startDaemon(dir, a);
    await startDaemon(dir, b);

    runCli(dir, "tag-multi", a, b, "role=web");
    const aEvts = readEvents(dir, a).filter((e) => e.type === "tags_change");
    const bEvts = readEvents(dir, b).filter((e) => e.type === "tags_change");
    expect(aEvts.length).toBe(1);
    expect(bEvts.length).toBe(1);
    expect(aEvts[0].value).toEqual({ role: "web" });
    expect(bEvts[0].value).toEqual({ role: "web" });
  }, 15000);

  it("no-op write on a session emits no event for that session, others fire normally", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    const b = uniqueName();
    await startDaemon(dir, a, { role: "web" });   // setting role=web is a no-op for a
    await startDaemon(dir, b);                     // real change for b

    const beforeA = readEvents(dir, a).filter((e) => e.type === "tags_change").length;
    const beforeB = readEvents(dir, b).filter((e) => e.type === "tags_change").length;

    runCli(dir, "tag-multi", a, b, "role=web");

    expect(readEvents(dir, a).filter((e) => e.type === "tags_change").length).toBe(beforeA);
    expect(readEvents(dir, b).filter((e) => e.type === "tags_change").length).toBe(beforeB + 1);
  }, 15000);

  it("upfront unresolvable name: no writes happen on any session", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    await startDaemon(dir, a);

    const r = runCli(dir, "tag-multi", a, "no-such", "role=web");
    expect(r.status).not.toBe(0);
    // Must not have written to `a` even though `a` was resolvable.
    expect(readMeta(dir, a).tags).toBeUndefined();
  }, 15000);

  it("resolves displayName for each session in the list", async () => {
    const dir = makeSessionDir();
    const aId = uniqueName();
    const bId = uniqueName();
    const aFriendly = `f1-${Math.random().toString(36).slice(2, 6)}`;
    const bFriendly = `f2-${Math.random().toString(36).slice(2, 6)}`;
    await startDaemon(dir, aId, undefined, aFriendly);
    await startDaemon(dir, bId, undefined, bFriendly);

    runCli(dir, "tag-multi", aFriendly, bFriendly, "role=web");
    expect(readMeta(dir, aId).tags).toEqual({ role: "web" });
    expect(readMeta(dir, bId).tags).toEqual({ role: "web" });
  }, 15000);
});

// =============================================================================
// WRITE MODE — selectors
// =============================================================================

describe("pty tag-multi — write mode (selectors)", () => {
  it("--filter-tag writes to each matching session", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    const b = uniqueName();
    const c = uniqueName();
    await startDaemon(dir, a, { role: "web" });
    await startDaemon(dir, b, { role: "db" });
    await startDaemon(dir, c, { role: "web" });

    runCli(dir, "tag-multi", "--filter-tag", "role=web", "audit=2026-04-25");

    expect(readMeta(dir, a).tags).toEqual({ role: "web", audit: "2026-04-25" });
    expect(readMeta(dir, b).tags).toEqual({ role: "db" }); // not matched
    expect(readMeta(dir, c).tags).toEqual({ role: "web", audit: "2026-04-25" });
  }, 20000);

  it("--filter-tag matching zero sessions: exit 0, no writes", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    await startDaemon(dir, a, { role: "web" });

    const r = runCli(dir, "tag-multi", "--filter-tag", "role=ghost", "x=1");
    expect(r.status).toBe(0);
    expect(readMeta(dir, a).tags).toEqual({ role: "web" }); // untouched
  }, 15000);

  it("--all without --yes is rejected (write mode is destructive across every session)", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    await startDaemon(dir, a);

    const r = runCli(dir, "tag-multi", "--all", "role=web");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/--yes/);
    // Untouched.
    expect(readMeta(dir, a).tags).toBeUndefined();
  }, 15000);

  it("--all --yes applies to every session", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    const b = uniqueName();
    await startDaemon(dir, a);
    await startDaemon(dir, b);

    const r = runCli(dir, "tag-multi", "--all", "--yes", "stamped=1");
    expect(r.status).toBe(0);
    expect(readMeta(dir, a).tags).toEqual({ stamped: "1" });
    expect(readMeta(dir, b).tags).toEqual({ stamped: "1" });
  }, 15000);

  it("-y short form works the same as --yes", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    await startDaemon(dir, a);

    const r = runCli(dir, "tag-multi", "--all", "-y", "role=web");
    expect(r.status).toBe(0);
    expect(readMeta(dir, a).tags).toEqual({ role: "web" });
  }, 15000);
});

// =============================================================================
// SELECTOR MUTEX
// =============================================================================

describe("pty tag-multi — selector mutex", () => {
  it("rejects --all combined with --filter-tag", () => {
    const dir = makeSessionDir();
    const r = runCli(dir, "tag-multi", "--all", "--filter-tag", "k=v");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/mutually exclusive|pick one/i);
  });

  it("rejects --all combined with explicit names", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    await startDaemon(dir, a);

    const r = runCli(dir, "tag-multi", "--all", a);
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/mutually exclusive|pick one/i);
  }, 15000);

  it("rejects --filter-tag combined with explicit names", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    await startDaemon(dir, a);

    const r = runCli(dir, "tag-multi", "--filter-tag", "k=v", a);
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/mutually exclusive|pick one/i);
  }, 15000);

  it("rejects no selector at all", () => {
    const dir = makeSessionDir();
    const r = runCli(dir, "tag-multi");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/selector/i);
  });

  it("rejects ops with no selector (e.g. just `pty tag-multi role=web`)", () => {
    const dir = makeSessionDir();
    const r = runCli(dir, "tag-multi", "role=web");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/selector/i);
  });
});

// =============================================================================
// OPS PARSING — error surface (mirrors pty tag)
// =============================================================================

describe("pty tag-multi — ops parsing errors", () => {
  it("rejects empty key in =value", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    await startDaemon(dir, a);

    const r = runCli(dir, "tag-multi", a, "=value");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/key/i);
    expect(readMeta(dir, a).tags).toBeUndefined();
  }, 15000);

  it("rejects --rm at end without a key", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    await startDaemon(dir, a, { keep: "yes" });

    const r = runCli(dir, "tag-multi", a, "--rm");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/--rm/);
    expect(readMeta(dir, a).tags).toEqual({ keep: "yes" });
  }, 15000);

  it("rejects --rm with empty-string key", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    await startDaemon(dir, a);

    const r = runCli(dir, "tag-multi", a, "--rm", "");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/key/i);
  }, 15000);

  it("rejects --filter-tag without a value", () => {
    const dir = makeSessionDir();
    const r = runCli(dir, "tag-multi", "--filter-tag");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/--filter-tag|k=v/i);
  });

  it("rejects --filter-tag with no equals sign", () => {
    const dir = makeSessionDir();
    const r = runCli(dir, "tag-multi", "--filter-tag", "no-equals");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/filter|k=v/i);
  });

  it("multi-= splits on first equals (key='foo', value='bar=baz')", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    await startDaemon(dir, a);

    runCli(dir, "tag-multi", a, "foo=bar=baz");
    expect(readMeta(dir, a).tags).toEqual({ foo: "bar=baz" });
  }, 15000);

  it("set+rm same key in one call: rm wins (matches pty tag contract)", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    await startDaemon(dir, a);

    runCli(dir, "tag-multi", a, "k=v", "--rm", "k");
    expect(readMeta(dir, a).tags).toBeUndefined();
  }, 15000);
});

// =============================================================================
// MISC
// =============================================================================

describe("pty tag-multi — misc", () => {
  it("read of an explicit list with mixed tagged/untagged sessions is consistent", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    const b = uniqueName();
    const c = uniqueName();
    await startDaemon(dir, a, { role: "web" });
    await startDaemon(dir, b);
    await startDaemon(dir, c, { env: "prod" });

    const r = runCli(dir, "tag-multi", a, b, c, "--json");
    expect(r.status).toBe(0);
    expect(JSON.parse(r.stdout)).toEqual({
      [a]: { role: "web" },
      [b]: {},
      [c]: { env: "prod" },
    });
  }, 20000);

  it("write to an empty list (--filter-tag matches nothing) emits no events", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    await startDaemon(dir, a, { role: "web" });

    const before = readEvents(dir, a).length;
    runCli(dir, "tag-multi", "--filter-tag", "role=ghost", "x=1");
    const after = readEvents(dir, a).length;
    expect(after).toBe(before);
  }, 15000);

  it("--all read is consistent with summing per-name reads", async () => {
    const dir = makeSessionDir();
    const a = uniqueName();
    const b = uniqueName();
    await startDaemon(dir, a, { role: "web" });
    await startDaemon(dir, b, { env: "prod" });

    const all = JSON.parse(runCli(dir, "tag-multi", "--all", "--json").stdout);
    const explicit = JSON.parse(runCli(dir, "tag-multi", a, b, "--json").stdout);
    expect(all).toEqual(explicit);
  }, 15000);
});
