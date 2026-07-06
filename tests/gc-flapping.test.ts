// #54: fast-fail respawn cap. A crash-looping permanent session gets
// flagged flapping and stopped after `strategy.fast-fail-limit`
// consecutive fast failures within `strategy.fast-fail-window` seconds.
// Auto-reset on command change; manual reset via `pty tag --rm`.

import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-flap-"));
afterAll(() => {
  fs.rmSync(testRoot, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

let sessionDirs: string[] = [];

function makeSessionDir(): string {
  const dir = fs.mkdtempSync(path.join(testRoot, "d-"));
  sessionDirs.push(dir);
  return dir;
}

let nameCounter = 0;
function uniqueName(): string {
  return `fl${++nameCounter}${Math.random().toString(36).slice(2, 5)}`;
}

function runCli(sessionDir: string, ...args: string[]) {
  return spawnSync(nodeBin, [cliPath, ...args], {
    env: { ...process.env, PTY_SESSION_DIR: sessionDir },
    encoding: "utf-8",
    timeout: 15000,
  });
}

function readMeta(sessionDir: string, name: string): any {
  return JSON.parse(fs.readFileSync(path.join(sessionDir, `${name}.json`), "utf-8"));
}

function readEvents(sessionDir: string, name: string): any[] {
  const filePath = path.join(sessionDir, `${name}.events.jsonl`);
  try {
    return fs.readFileSync(filePath, "utf-8")
      .trimEnd().split("\n").filter((l) => l.length > 0).map((l) => JSON.parse(l));
  } catch {
    return [];
  }
}

/** Write a synthetic exited-permanent metadata file. `respawnAt` seeds
 *  `strategy.last-respawn-at`, `exitedAt` seeds `metadata.exitedAt` —
 *  their delta drives the fast-fail classifier. */
function writeExitedPermanent(
  sessionDir: string,
  name: string,
  opts: {
    command?: string;
    args?: string[];
    tags?: Record<string, string>;
    lastRespawnAt?: string;
    exitedAt?: string;
    counter?: number;
    commandHash?: string;
    status?: string;
  },
): void {
  const command = opts.command ?? "sh";
  const args = opts.args ?? ["-c", "exit 1"];
  const tags: Record<string, string> = {
    strategy: "permanent",
    ...(opts.tags ?? {}),
  };
  if (opts.lastRespawnAt !== undefined) tags["strategy.last-respawn-at"] = opts.lastRespawnAt;
  if (opts.counter !== undefined) tags["strategy.consecutive-fast-fails"] = String(opts.counter);
  if (opts.commandHash !== undefined) tags["strategy.command-hash"] = opts.commandHash;
  if (opts.status !== undefined) tags["strategy.status"] = opts.status;

  fs.writeFileSync(path.join(sessionDir, `${name}.json`), JSON.stringify({
    command, args, displayCommand: command,
    cwd: os.tmpdir(),
    createdAt: new Date(Date.now() - 10 * 60_000).toISOString(),
    exitedAt: opts.exitedAt ?? new Date().toISOString(),
    exitCode: 1,
    tags,
  }));
  // Also write a stub events file so appendEventSync doesn't create
  // orphaned JSONL. Some listSessions paths key off it.
  const evPath = path.join(sessionDir, `${name}.events.jsonl`);
  if (!fs.existsSync(evPath)) fs.writeFileSync(evPath, "");
}

function commandHash(command: string, args: string[]): string {
  const h = createHash("sha256");
  h.update(command);
  h.update("\0");
  h.update(args.join("\0"));
  return h.digest("hex").slice(0, 16);
}

afterEach(() => {
  for (const dir of sessionDirs) {
    try {
      for (const e of fs.readdirSync(dir)) { try { fs.unlinkSync(path.join(dir, e)); } catch {} }
    } catch {}
  }
  sessionDirs = [];
});

describe("gc fast-fail respawn cap", () => {
  it("dry-run: below-limit fast fails preview as respawn (no state mutation)", () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    // Session was respawned 5s ago, exited 1s later → fast fail (1s < 60s).
    // Prior counter 1 → this tick would make it 2, still below limit 3.
    const last = new Date(Date.now() - 5000).toISOString();
    const exit = new Date(Date.now() - 4000).toISOString();
    writeExitedPermanent(dir, name, {
      lastRespawnAt: last, exitedAt: exit, counter: 1,
      commandHash: commandHash("sh", ["-c", "exit 1"]),
    });

    const r = runCli(dir, "gc", "--dry-run");
    expect(r.status).toBe(0);
    expect(r.stdout).toMatch(new RegExp(`Would respawn: ${name}`));
    expect(r.stdout).not.toContain("Would flap");
    // Dry-run must not mutate tags on disk.
    const meta = readMeta(dir, name);
    expect(meta.tags["strategy.consecutive-fast-fails"]).toBe("1");
    expect(meta.tags["strategy.status"]).toBeUndefined();
  });

  it("dry-run: at-limit tick previews Would flap and no respawn", () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const last = new Date(Date.now() - 5000).toISOString();
    const exit = new Date(Date.now() - 4000).toISOString();
    // Prior counter 2 → this tick makes it 3, hits default limit → flap.
    writeExitedPermanent(dir, name, {
      lastRespawnAt: last, exitedAt: exit, counter: 2,
      commandHash: commandHash("sh", ["-c", "exit 1"]),
    });

    const r = runCli(dir, "gc", "--dry-run");
    expect(r.status).toBe(0);
    expect(r.stdout).toMatch(new RegExp(`Would flap: ${name} \\(3 fast-fails in 60s, limit 3\\)`));
    expect(r.stdout).not.toMatch(new RegExp(`Would respawn: ${name}`));

    // No mutation on dry-run.
    const meta = readMeta(dir, name);
    expect(meta.tags["strategy.status"]).toBeUndefined();
    expect(meta.tags["strategy.consecutive-fast-fails"]).toBe("2");
  });

  it("at-limit tick persists strategy.status=flapping and emits session_flapping", () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const last = new Date(Date.now() - 5000).toISOString();
    const exit = new Date(Date.now() - 4000).toISOString();
    writeExitedPermanent(dir, name, {
      lastRespawnAt: last, exitedAt: exit, counter: 2,
      commandHash: commandHash("sh", ["-c", "exit 1"]),
    });

    const r = runCli(dir, "gc");
    expect(r.status).toBe(0);
    expect(r.stdout).toContain(`Flapping: ${name} (3 fast-fails in 60s, limit 3)`);

    const meta = readMeta(dir, name);
    expect(meta.tags["strategy.status"]).toBe("flapping");
    expect(meta.tags["strategy.consecutive-fast-fails"]).toBe("3");

    const events = readEvents(dir, name);
    const flap = events.find((e) => e.type === "session_flapping");
    expect(flap).toBeDefined();
    expect(flap.counter).toBe(3);
    expect(flap.limit).toBe(3);
    expect(flap.window).toBe(60);
  });

  it("already-flapping session is silently skipped on subsequent tick", () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    writeExitedPermanent(dir, name, {
      status: "flapping",
      counter: 3,
      lastRespawnAt: new Date(Date.now() - 60_000).toISOString(),
      exitedAt: new Date(Date.now() - 55_000).toISOString(),
      commandHash: commandHash("sh", ["-c", "exit 1"]),
    });

    const r = runCli(dir, "gc");
    expect(r.status).toBe(0);
    expect(r.stdout).toContain(`Skipped (flapping): ${name}`);
    expect(r.stdout).not.toContain(`Respawned: ${name}`);

    // No new events beyond what was already there.
    const events = readEvents(dir, name);
    expect(events.filter((e) => e.type === "session_flapping").length).toBe(0);
  });

  it("slow-fail (past window) resets the counter to 0", () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    // Last respawn 10 minutes ago, exited 9 minutes ago → 60s live, past
    // the default 60s window (using an exit ~60m after respawn).
    const last = new Date(Date.now() - 10 * 60_000).toISOString();
    const exit = new Date(Date.now() - 5 * 60_000).toISOString();
    writeExitedPermanent(dir, name, {
      lastRespawnAt: last, exitedAt: exit, counter: 2,
      commandHash: commandHash("sh", ["-c", "exit 1"]),
    });

    const r = runCli(dir, "gc", "--dry-run");
    expect(r.status).toBe(0);
    expect(r.stdout).toMatch(new RegExp(`Would respawn: ${name}`));
    expect(r.stdout).not.toContain("Would flap");
    // (Counter reset is verified via the "no flap at same prior counter"
    // outcome — a fast fail at prior=2 would have flapped; slow-fail
    // continues to respawn.)
  });

  it("command-hash change auto-resets counter and clears flapping mark", () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    // Session is currently flagged flapping under an old command hash.
    // Metadata now reports a DIFFERENT command (operator edited pty.toml).
    // The classifier should notice the divergence and both reset the
    // counter and clear the flapping mark, letting gc respawn.
    const oldHash = commandHash("sh", ["-c", "old-command"]);
    writeExitedPermanent(dir, name, {
      command: "sh", args: ["-c", "exit 1"],  // new command
      status: "flapping", counter: 3,
      lastRespawnAt: new Date(Date.now() - 5000).toISOString(),
      exitedAt: new Date(Date.now() - 4000).toISOString(),
      commandHash: oldHash,  // stale hash
    });

    const r = runCli(dir, "gc", "--dry-run");
    expect(r.status).toBe(0);
    expect(r.stdout).toMatch(new RegExp(`Would respawn: ${name}`));
    expect(r.stdout).not.toContain("Skipped (flapping)");
    expect(r.stdout).not.toContain("Would flap");
  });

  it("per-session strategy.fast-fail-limit overrides the default", () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    // Prior counter 1 + fast fail → this tick makes it 2. Default limit
    // 3 would let it respawn; per-session limit 2 flaps instead.
    writeExitedPermanent(dir, name, {
      lastRespawnAt: new Date(Date.now() - 5000).toISOString(),
      exitedAt: new Date(Date.now() - 4000).toISOString(),
      counter: 1,
      commandHash: commandHash("sh", ["-c", "exit 1"]),
      tags: { "strategy.fast-fail-limit": "2" },
    });

    const r = runCli(dir, "gc", "--dry-run");
    expect(r.status).toBe(0);
    expect(r.stdout).toMatch(new RegExp(`Would flap: ${name} \\(2 fast-fails in 60s, limit 2\\)`));
  });

  it("--fast-fail-limit CLI flag applies to sessions without a per-session tag", () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    // Prior counter 4 + fast fail → 5. CLI flag lifts limit to 10 → still respawn.
    writeExitedPermanent(dir, name, {
      lastRespawnAt: new Date(Date.now() - 5000).toISOString(),
      exitedAt: new Date(Date.now() - 4000).toISOString(),
      counter: 4,
      commandHash: commandHash("sh", ["-c", "exit 1"]),
    });

    const r = runCli(dir, "gc", "--dry-run", "--fast-fail-limit=10");
    expect(r.status).toBe(0);
    expect(r.stdout).toMatch(new RegExp(`Would respawn: ${name}`));
    expect(r.stdout).not.toContain("Would flap");
  });

  it("per-session strategy.fast-fail-window overrides the default", () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    // Live time = 30s. Default window 60s → fast fail. Per-session window
    // 10s → slow fail (30 > 10) → counter resets → respawn, no flap.
    writeExitedPermanent(dir, name, {
      lastRespawnAt: new Date(Date.now() - 30_000).toISOString(),
      exitedAt: new Date(Date.now() - 0).toISOString(),
      counter: 2,
      commandHash: commandHash("sh", ["-c", "exit 1"]),
      tags: { "strategy.fast-fail-window": "10" },
    });

    const r = runCli(dir, "gc", "--dry-run");
    expect(r.status).toBe(0);
    expect(r.stdout).toMatch(new RegExp(`Would respawn: ${name}`));
    expect(r.stdout).not.toContain("Would flap");
  });

  it("no prior last-respawn-at (first respawn ever) doesn't count as fast fail", () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    // Session exited fresh — no last-respawn-at tag yet. Counter absent.
    writeExitedPermanent(dir, name, {
      exitedAt: new Date().toISOString(),
    });

    const r = runCli(dir, "gc", "--dry-run");
    expect(r.status).toBe(0);
    expect(r.stdout).toMatch(new RegExp(`Would respawn: ${name}`));
    expect(r.stdout).not.toContain("Would flap");
  });
});
