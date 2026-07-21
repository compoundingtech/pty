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
  tags?: Record<string, string>,
): Promise<number> {
  const config = JSON.stringify({
    name, command, args, displayCommand: command,
    cwd: os.tmpdir(), rows: 24, cols: 80, tags,
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
    // `keep=true` exempts the session from the daemon's exit-time self-reap,
    // so there is still a record on disk to inspect after it exits cleanly.
    await startDaemon(dir, name, "true", [], { keep: "true" }); // exits cleanly
    await new Promise((r) => setTimeout(r, 1000));

    const r = runCli(dir, "list", "--json");
    const sessions = JSON.parse(r.stdout);
    const found = sessions.find((s: any) => s.name === name);
    expect(found.status).toBe("exited");
    expect(found.exitCode).toBe(0);
    expect(found.exitedAt).not.toBeNull();
  }, 15000);
});

describe("listSessions guards against deleting state for live daemons", () => {
  // Refs https://github.com/compoundingtech/pty/issues/34. listSessions used to
  // unconditionally `cleanupSocket` whenever the socket-reachable probe
  // failed and `cleanupAll` whenever metadata was older than 24h. Both
  // ran even if the recorded pid was still alive — once the .sock or
  // .json was gone, the still-running daemon became invisible to every
  // future scan. These tests pin the new behaviour: live pid wins.
  it("keeps a session whose socket file is missing but recorded pid is still alive", () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    // Use the test runner's own pid as a stand-in for an alive daemon.
    fs.writeFileSync(path.join(dir, `${name}.pid`), String(process.pid));
    fs.writeFileSync(path.join(dir, `${name}.json`), JSON.stringify({
      command: "cat", args: [], displayCommand: "cat", cwd: os.tmpdir(),
      createdAt: new Date().toISOString(),
    }));
    // Note: no .sock file written. Without the guard, scan-and-cleanup paths
    // would fall into the .json branch and delete metadata-on-age.

    // Force the .json into the >24h bucket so the second guard is exercised.
    const old = new Date(Date.now() - 48 * 3600_000).toISOString();
    fs.writeFileSync(path.join(dir, `${name}.json`), JSON.stringify({
      command: "cat", args: [], displayCommand: "cat", cwd: os.tmpdir(),
      createdAt: old,
    }));

    const r = runCli(dir, "list", "--json");
    expect(r.status).toBe(0);
    const found = JSON.parse(r.stdout).find((s: any) => s.name === name);
    expect(found, "session should still be listed because its pid is alive").toBeDefined();
    // Metadata file must survive the call so the next scan also sees it.
    expect(fs.existsSync(path.join(dir, `${name}.json`))).toBe(true);
  }, 10000);

  it("does delete metadata older than 24h when the pid is dead", () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    // Pid 0x7fffffff is "guaranteed dead" on Linux/macOS in practice.
    fs.writeFileSync(path.join(dir, `${name}.pid`), "2147483647");
    const old = new Date(Date.now() - 48 * 3600_000).toISOString();
    fs.writeFileSync(path.join(dir, `${name}.json`), JSON.stringify({
      command: "cat", args: [], displayCommand: "cat", cwd: os.tmpdir(),
      createdAt: old,
    }));

    const r = runCli(dir, "list", "--json");
    expect(r.status).toBe(0);
    expect(JSON.parse(r.stdout).find((s: any) => s.name === name)).toBeUndefined();
    expect(fs.existsSync(path.join(dir, `${name}.json`))).toBe(false);
  }, 10000);
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

// Adding metadata files in a specific order and checking that `pty list`
// sorts its output regardless of on-disk insertion order. Without an
// explicit sort the output reflects readdir order (APFS insertion-ish),
// which is near-meaningless when sessions come and go.
function writeFakeMetadataWithDn(
  dir: string,
  name: string,
  opts: { createdAt: string; displayName?: string },
) {
  const meta = {
    command: "cat",
    args: [],
    displayCommand: "cat",
    cwd: os.tmpdir(),
    createdAt: opts.createdAt,
    ...(opts.displayName ? { displayName: opts.displayName } : {}),
  };
  fs.writeFileSync(path.join(dir, `${name}.json`), JSON.stringify(meta));
}

describe("pty list — sort order", () => {
  it("JSON output is sorted ASCII by displayName, falling back to name", () => {
    const dir = makeSessionDir();
    const now = new Date().toISOString();

    // Intentionally create in non-alphabetical order to confirm the sort
    // isn't just insertion-order by accident.
    writeFakeMetadataWithDn(dir, "zzz-raw", { createdAt: now });
    writeFakeMetadataWithDn(dir, "aaa-raw", { createdAt: now, displayName: "mmm-friendly" });
    writeFakeMetadataWithDn(dir, "mmm-raw", { createdAt: now, displayName: "bbb-friendly" });
    writeFakeMetadataWithDn(dir, "bbb-raw", { createdAt: now });

    const r = runCli(dir, "list", "--json");
    expect(r.status).toBe(0);
    const sessions = JSON.parse(r.stdout);
    // Expected sort keys: bbb-friendly, bbb-raw, mmm-friendly, zzz-raw
    const keys = sessions.map((s: any) => s.displayName ?? s.name);
    expect(keys).toEqual(["bbb-friendly", "bbb-raw", "mmm-friendly", "zzz-raw"]);
  }, 10000);

  it("text output renders grouped buckets in sorted order", () => {
    const dir = makeSessionDir();
    const now = new Date().toISOString();

    // All vanished (missing exitedAt/exitCode), so they land in a single
    // bucket and we can just scan for line order within it.
    writeFakeMetadataWithDn(dir, "z1", { createdAt: now });
    writeFakeMetadataWithDn(dir, "a1", { createdAt: now });
    writeFakeMetadataWithDn(dir, "m1", { createdAt: now });

    const r = runCli(dir, "list");
    expect(r.status).toBe(0);
    const ia = r.stdout.indexOf("a1");
    const im = r.stdout.indexOf("m1");
    const iz = r.stdout.indexOf("z1");
    expect(ia).toBeGreaterThan(-1);
    expect(im).toBeGreaterThan(ia);
    expect(iz).toBeGreaterThan(im);
  }, 10000);

  it("displayName beats the stable id when sorting", () => {
    const dir = makeSessionDir();
    const now = new Date().toISOString();
    // id "aaa" but displayName "zebra" should sort AFTER id "mmm" with no
    // displayName — the displayName wins.
    writeFakeMetadataWithDn(dir, "aaa", { createdAt: now, displayName: "zebra" });
    writeFakeMetadataWithDn(dir, "mmm", { createdAt: now });

    const r = runCli(dir, "list", "--json");
    const sessions = JSON.parse(r.stdout);
    const keys = sessions.map((s: any) => s.displayName ?? s.name);
    expect(keys).toEqual(["mmm", "zebra"]);
  }, 10000);
});
