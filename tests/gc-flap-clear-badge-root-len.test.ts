// Follow-ups to #56:
//   1. `pty restart` clears strategy.status=flapping + bookkeeping.
//   2. `pty up` clears the same on the "already running, tag-sync" path.
//   3. `pty list` renders `[flapping]` badge (mutually exclusive with [permanent]).
//   4. Fail-loud startup backstop when PTY_ROOT is too long for the socket-path
//      kernel limit — errors before any subcommand runs.

import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-follow-"));
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
  return `fc${++nameCounter}${Math.random().toString(36).slice(2, 5)}`;
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

/** Write metadata simulating an exited session that gc previously marked
 *  flapping. Includes all four bookkeeping tags. */
function writeFlappingExited(
  sessionDir: string,
  name: string,
  extraTags: Record<string, string> = {},
): void {
  fs.writeFileSync(path.join(sessionDir, `${name}.json`), JSON.stringify({
    command: "sh", args: ["-c", "exit 1"], displayCommand: "sh -c 'exit 1'",
    cwd: os.tmpdir(),
    createdAt: new Date(Date.now() - 10 * 60_000).toISOString(),
    exitedAt: new Date().toISOString(),
    exitCode: 1,
    tags: {
      strategy: "permanent",
      "strategy.status": "flapping",
      "strategy.consecutive-fast-fails": "3",
      "strategy.last-respawn-at": new Date(Date.now() - 5000).toISOString(),
      "strategy.command-hash": "0123456789abcdef",
      ...extraTags,
    },
  }));
  const evPath = path.join(sessionDir, `${name}.events.jsonl`);
  if (!fs.existsSync(evPath)) fs.writeFileSync(evPath, "");
}

afterEach(() => {
  for (const dir of sessionDirs) {
    try {
      for (const e of fs.readdirSync(dir)) { try { fs.unlinkSync(path.join(dir, e)); } catch {} }
    } catch {}
  }
  sessionDirs = [];
});

describe("pty restart clears strategy.status=flapping + bookkeeping", () => {
  it("restart of a flapping-exited session drops all four gc bookkeeping tags", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    writeFlappingExited(dir, name, { role: "test" });

    // Restart it. -y skips the prompt; command is "sh -c 'exit 1'" which
    // will exit almost immediately — that's fine, we care about the tag
    // snapshot right after spawn. PTY_SESSION is set so restart takes its
    // "already inside a session, don't attach" branch and returns 0 instead of
    // attaching to the just-exited session (a non-TTY attach here would inherit
    // that session's exit code 1). Setting it explicitly rather than relying on
    // an ambient PTY_SESSION leaking from the harness.
    const r = spawnSync(nodeBin, [cliPath, "restart", "-y", name], {
      env: { ...process.env, PTY_SESSION_DIR: dir, PTY_SESSION: "outer" },
      encoding: "utf-8",
      timeout: 15000,
    });
    expect(r.status).toBe(0);

    // Give the daemon a moment to write its metadata.
    await new Promise((res) => setTimeout(res, 300));

    const meta = readMeta(dir, name);
    expect(meta.tags.strategy).toBe("permanent");
    expect(meta.tags.role).toBe("test");
    // Bookkeeping tags are gone.
    expect(meta.tags["strategy.status"]).toBeUndefined();
    expect(meta.tags["strategy.consecutive-fast-fails"]).toBeUndefined();
    expect(meta.tags["strategy.last-respawn-at"]).toBeUndefined();
    expect(meta.tags["strategy.command-hash"]).toBeUndefined();
  });
});

describe("pty list renders [flapping] badge", () => {
  it("running session with strategy.status=flapping shows [flapping] in text output", () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    // Simulate a running session (synthetic pid + no exitedAt) with a
    // flapping mark. `pty list` classifies status by (pid alive check +
    // exited fields); we cheat by using our own pid so isProcessAlive
    // returns true — the row renders as running.
    fs.writeFileSync(path.join(dir, `${name}.json`), JSON.stringify({
      command: "sh", args: [], displayCommand: "sh",
      cwd: os.tmpdir(),
      createdAt: new Date().toISOString(),
      tags: {
        strategy: "permanent",
        "strategy.status": "flapping",
      },
    }));
    fs.writeFileSync(path.join(dir, `${name}.pid`), String(process.pid));

    const r = runCli(dir, "list");
    expect(r.status).toBe(0);
    // ANSI color-code stripped for the assertion; check the literal marker.
    // `[flapping]` supersedes `[permanent]` when both would apply.
    expect(r.stdout).toContain("[flapping]");
    expect(r.stdout).not.toContain("[permanent]");
  });

  it("session without strategy.status=flapping still shows [permanent]", () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    fs.writeFileSync(path.join(dir, `${name}.json`), JSON.stringify({
      command: "sh", args: [], displayCommand: "sh",
      cwd: os.tmpdir(),
      createdAt: new Date().toISOString(),
      tags: { strategy: "permanent" },
    }));
    fs.writeFileSync(path.join(dir, `${name}.pid`), String(process.pid));

    const r = runCli(dir, "list");
    expect(r.status).toBe(0);
    expect(r.stdout).toContain("[permanent]");
    expect(r.stdout).not.toContain("[flapping]");
  });
});

describe("PTY_ROOT length backstop", () => {
  it("errors at startup when PTY_ROOT is too deep to fit the sockaddr_un limit", () => {
    // Build a 95-byte path — well past the 90-byte usable threshold
    // (104 − 14 for `/xxxxxxxx.sock`). Doesn't need to actually exist;
    // the check is on byte length, not existence.
    const tooLong = "/tmp/" + "a".repeat(95);
    expect(Buffer.byteLength(tooLong, "utf-8")).toBeGreaterThan(90);

    const r = spawnSync(nodeBin, [cliPath, "list"], {
      env: { ...process.env, PTY_ROOT: tooLong, PTY_ROOT_LEGACY_SILENT: "1" },
      encoding: "utf-8",
      timeout: 5000,
    });
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/PTY_ROOT is too long/);
    expect(r.stderr).toMatch(/104-byte kernel limit/);
    // Points the finger at the root, not the name.
    expect(r.stderr).toMatch(/Shorten the root/);
  });

  it("errors before any subcommand-specific parsing runs", () => {
    // Backstop should fire even on a bogus subcommand — the too-long
    // root is caught before dispatch.
    const tooLong = "/tmp/" + "b".repeat(100);
    const r = spawnSync(nodeBin, [cliPath, "definitely-not-a-real-subcommand"], {
      env: { ...process.env, PTY_ROOT: tooLong, PTY_ROOT_LEGACY_SILENT: "1" },
      encoding: "utf-8",
      timeout: 5000,
    });
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/PTY_ROOT is too long/);
    // The "unknown command" path is NOT hit; the root check errors first.
    expect(r.stderr).not.toMatch(/Unknown command/);
  });

  it("allows a root right at the usable threshold", () => {
    // Build a root at exactly 90 bytes — the maximum that leaves room
    // for `/xxxxxxxx.sock` in 104. This should succeed (empty list).
    const usable = 104 - ("/".length + 8 + ".sock".length);
    const okRoot = "/tmp/" + "c".repeat(usable - "/tmp/".length);
    expect(Buffer.byteLength(okRoot, "utf-8")).toBe(usable);
    fs.mkdirSync(okRoot, { recursive: true });
    try {
      const r = spawnSync(nodeBin, [cliPath, "list", "--json"], {
        env: { ...process.env, PTY_ROOT: okRoot, PTY_ROOT_LEGACY_SILENT: "1" },
        encoding: "utf-8",
        timeout: 5000,
      });
      expect(r.status).toBe(0);
      expect(JSON.parse(r.stdout)).toEqual([]);
    } finally {
      try { fs.rmSync(okRoot, { recursive: true, force: true }); } catch {}
    }
  });

  it("--root <shorter> overrides an env that would otherwise fail", () => {
    // Env is too long; --root override is fine. The startup check reads
    // process.env.PTY_ROOT *after* --root parsing has set it, so the
    // override wins.
    const tooLongEnv = "/tmp/" + "d".repeat(95);
    const shortFlag = fs.mkdtempSync(path.join(testRoot, "shorter-"));
    const r = spawnSync(nodeBin, [cliPath, "--root", shortFlag, "list", "--json"], {
      env: { ...process.env, PTY_ROOT: tooLongEnv, PTY_ROOT_LEGACY_SILENT: "1" },
      encoding: "utf-8",
      timeout: 5000,
    });
    expect(r.status).toBe(0);
    expect(JSON.parse(r.stdout)).toEqual([]);
  });
});
