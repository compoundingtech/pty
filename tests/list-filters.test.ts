// End-to-end tests for the new `pty list` flags shipped alongside the
// vanished-status change (--status, --older-than/--newer-than, --summary)
// and for the status inference itself. Writes metadata files directly to
// a scratch session dir for the age-filter and vanished cases so tests
// don't have to wait for wall-clock time.

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

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-lf-"));
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
  return `lf${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
}

async function startDaemon(
  sessionDir: string,
  name: string,
  command: string,
  args: string[] = [],
): Promise<number> {
  const config = JSON.stringify({
    name, command, args, displayCommand: command,
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

/** Fabricate a session-directory-only record with no daemon. Lets us test
 *  age filters and the vanished inference without waiting for real wall time
 *  or SIGKILL-ing a real daemon. */
function writeFakeMetadata(dir: string, name: string, opts: {
  createdAt: string;
  exitedAt?: string;
  exitCode?: number;
  tags?: Record<string, string>;
}) {
  const meta = {
    command: "cat",
    args: [],
    displayCommand: "cat",
    cwd: os.tmpdir(),
    createdAt: opts.createdAt,
    ...(opts.exitedAt ? { exitedAt: opts.exitedAt } : {}),
    ...(opts.exitCode != null ? { exitCode: opts.exitCode } : {}),
    ...(opts.tags ? { tags: opts.tags } : {}),
  };
  fs.writeFileSync(path.join(dir, `${name}.json`), JSON.stringify(meta));
}

function runCli(sessionDir: string, ...args: string[]) {
  return spawnSync(nodeBin, [cliPath, ...args], {
    env: { ...process.env, PTY_SESSION_DIR: sessionDir },
    encoding: "utf-8",
    timeout: 10000,
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

describe("vanished status", () => {
  it("listSessions infers vanished when metadata has no exitedAt/exitCode and no live daemon", () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    writeFakeMetadata(dir, name, { createdAt: new Date().toISOString() });

    const r = runCli(dir, "list", "--json");
    expect(r.status).toBe(0);
    const sessions = JSON.parse(r.stdout);
    const found = sessions.find((s: any) => s.name === name);
    expect(found).toBeDefined();
    expect(found.status).toBe("vanished");
    expect(found.exitCode).toBeNull();
    expect(found.exitedAt).toBeNull();
  }, 10000);

  it("text output separates vanished sessions into their own bucket", () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    writeFakeMetadata(dir, name, { createdAt: new Date().toISOString() });

    const r = runCli(dir, "list");
    expect(r.status).toBe(0);
    expect(r.stdout).toContain("Vanished sessions");
    expect(r.stdout).toContain(name);
  }, 10000);

  it("cleanly-exited sessions keep status=exited, not vanished", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "true"); // exits cleanly
    await new Promise((r) => setTimeout(r, 1000));

    const r = runCli(dir, "list", "--json");
    const sessions = JSON.parse(r.stdout);
    const found = sessions.find((s: any) => s.name === name);
    expect(found.status).toBe("exited");
    expect(found.exitCode).toBe(0);
    expect(found.exitedAt).not.toBeNull();
  }, 15000);
});

describe("pty list --status", () => {
  it("filters to a single status", async () => {
    const dir = makeSessionDir();
    const live = uniqueName();
    await startDaemon(dir, live, "cat");

    const gone = uniqueName();
    writeFakeMetadata(dir, gone, {
      createdAt: new Date().toISOString(),
      exitedAt: new Date().toISOString(),
      exitCode: 0,
    });

    const lost = uniqueName();
    writeFakeMetadata(dir, lost, { createdAt: new Date().toISOString() });

    const onlyRunning = runCli(dir, "list", "--json", "--status", "running");
    expect(JSON.parse(onlyRunning.stdout).map((s: any) => s.name)).toEqual([live]);

    const onlyExited = runCli(dir, "list", "--json", "--status", "exited");
    expect(JSON.parse(onlyExited.stdout).map((s: any) => s.name)).toEqual([gone]);

    const onlyVanished = runCli(dir, "list", "--json", "--status", "vanished");
    expect(JSON.parse(onlyVanished.stdout).map((s: any) => s.name)).toEqual([lost]);
  }, 15000);

  it("rejects an invalid --status value", () => {
    const dir = makeSessionDir();
    const r = runCli(dir, "list", "--status", "bogus");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toContain("--status expects");
  }, 10000);
});

describe("pty list --older-than / --newer-than", () => {
  it("--older-than filters out recent sessions", () => {
    const dir = makeSessionDir();
    const old = uniqueName();
    const recent = uniqueName();
    const TWO_HOURS_AGO = new Date(Date.now() - 2 * 3600_000).toISOString();
    writeFakeMetadata(dir, old, { createdAt: TWO_HOURS_AGO });
    writeFakeMetadata(dir, recent, { createdAt: new Date().toISOString() });

    const r = runCli(dir, "list", "--json", "--older-than", "1h");
    const names = JSON.parse(r.stdout).map((s: any) => s.name);
    expect(names).toEqual([old]);
  }, 10000);

  it("--newer-than filters out old sessions", () => {
    const dir = makeSessionDir();
    const old = uniqueName();
    const recent = uniqueName();
    writeFakeMetadata(dir, old, {
      createdAt: new Date(Date.now() - 2 * 3600_000).toISOString(),
    });
    writeFakeMetadata(dir, recent, { createdAt: new Date().toISOString() });

    const r = runCli(dir, "list", "--json", "--newer-than", "1h");
    const names = JSON.parse(r.stdout).map((s: any) => s.name);
    expect(names).toEqual([recent]);
  }, 10000);

  it("rejects malformed duration", () => {
    const dir = makeSessionDir();
    const r = runCli(dir, "list", "--older-than", "1week");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toContain("duration");
  }, 10000);

  it("composes with --filter-tag", () => {
    const dir = makeSessionDir();
    const oldMatch = uniqueName();
    const oldSkip = uniqueName();
    const TWO_HOURS_AGO = new Date(Date.now() - 2 * 3600_000).toISOString();
    writeFakeMetadata(dir, oldMatch, { createdAt: TWO_HOURS_AGO, tags: { env: "prod" } });
    writeFakeMetadata(dir, oldSkip,  { createdAt: TWO_HOURS_AGO, tags: { env: "dev" } });

    const r = runCli(
      dir, "list", "--json", "--older-than", "1h", "--filter-tag", "env=prod",
    );
    expect(JSON.parse(r.stdout).map((s: any) => s.name)).toEqual([oldMatch]);
  }, 10000);
});

describe("pty list --summary", () => {
  it("emits counts + oldest/newest in text mode", () => {
    const dir = makeSessionDir();
    const oldName = uniqueName();
    const recentName = uniqueName();
    writeFakeMetadata(dir, oldName, {
      createdAt: new Date(Date.now() - 2 * 3600_000).toISOString(),
      exitedAt: new Date(Date.now() - 60_000).toISOString(),
      exitCode: 0,
    });
    writeFakeMetadata(dir, recentName, { createdAt: new Date().toISOString() });

    const r = runCli(dir, "list", "--summary");
    expect(r.status).toBe(0);
    expect(r.stdout).toContain("2 sessions");
    expect(r.stdout).toContain("1 exited");
    expect(r.stdout).toContain("1 vanished");
    expect(r.stdout).toContain(`oldest: ${oldName}`);
    expect(r.stdout).toContain(`newest: ${recentName}`);
  }, 10000);

  it("emits structured JSON when --summary --json", () => {
    const dir = makeSessionDir();
    const only = uniqueName();
    writeFakeMetadata(dir, only, {
      createdAt: new Date(Date.now() - 5 * 60_000).toISOString(),
    });

    const r = runCli(dir, "list", "--json", "--summary");
    const payload = JSON.parse(r.stdout);
    expect(payload.total).toBe(1);
    expect(payload.byStatus.vanished).toBe(1);
    expect(payload.byStatus.exited).toBe(0);
    expect(payload.byStatus.running).toBe(0);
    expect(payload.oldest.name).toBe(only);
    expect(payload.oldest.status).toBe("vanished");
    expect(payload.oldest.ageSeconds).toBeGreaterThanOrEqual(295);
    expect(payload.newest.name).toBe(only);
  }, 10000);

  it("summary respects --status filter", () => {
    const dir = makeSessionDir();
    const exited = uniqueName();
    const lost = uniqueName();
    writeFakeMetadata(dir, exited, {
      createdAt: new Date(Date.now() - 60_000).toISOString(),
      exitedAt: new Date().toISOString(),
      exitCode: 0,
    });
    writeFakeMetadata(dir, lost, { createdAt: new Date().toISOString() });

    const r = runCli(dir, "list", "--json", "--summary", "--status", "vanished");
    const payload = JSON.parse(r.stdout);
    expect(payload.total).toBe(1);
    expect(payload.byStatus.vanished).toBe(1);
    expect(payload.byStatus.exited).toBe(0);
    expect(payload.oldest.name).toBe(lost);
  }, 10000);

  it("summary reports 'No matching sessions.' when the filter set is empty", () => {
    const dir = makeSessionDir();
    writeFakeMetadata(dir, uniqueName(), { createdAt: new Date().toISOString() });

    const r = runCli(dir, "list", "--summary", "--status", "running");
    expect(r.stdout).toContain("No matching sessions.");
  }, 10000);
});
