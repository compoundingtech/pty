// PARITY DRIVE — Round 2: shared, language-neutral fixtures.
//
// tests/fixtures/parity/screens.json is the SINGLE source of truth both this
// node suite and the Rust port (pty-rust) assert against, so the two
// implementations cannot silently drift. Node OWNS the file; pty-rust vendors a
// byte-identical mirror and writes the equivalent Rust assertions.
//
// This harness loads that JSON and, for each fixture, spawns the same daemon
// module the CLI daemonizes, waits settleMs, runs `peek --plain`, and asserts
// node reproduces the fixture's EXACT plain-screen bytes (+ exit status for the
// after-exit fixtures). Assertion rules (see the fixtures README): plain bytes
// EXACT; ANSI would be asserted as a mode-set, never raw bytes.

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

const fixturesPath = path.join(__dirname, "fixtures", "parity", "screens.json");
const fixtures = JSON.parse(fs.readFileSync(fixturesPath, "utf-8")) as {
  version: number;
  fixtures: Array<{
    id: string;
    kind: "plain-screen" | "plain-screen-after-exit" | "reaped-after-exit";
    description: string;
    spawn: { command: string; args: string[]; rows: number; cols: number };
    // Per-fixture env overlay for the spawned daemon. Exit-time behavior is
    // configurable (PTY_REAP_ON_EXIT); a fixture pins the mode it needs here.
    env?: Record<string, string>;
    settleMs: number;
    expect: {
      plainScreen?: string;
      plainScreenLength?: number;
      status?: string;
      exitCode?: number;
      reaped?: boolean;
    };
  }>;
};

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-parityfx-"));
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
  return `fx${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
}

async function startDaemon(
  sessionDir: string,
  name: string,
  command: string,
  args: string[],
  rows: number,
  cols: number,
  env: Record<string, string> = {},
): Promise<number> {
  const config = JSON.stringify({
    name, command, args, displayCommand: command,
    cwd: os.tmpdir(), rows, cols,
  });
  const child = spawn(nodeBin, [serverModule], {
    detached: true,
    stdio: ["ignore", "ignore", "pipe"],
    env: { ...process.env, PTY_SERVER_CONFIG: config, PTY_SESSION_DIR: sessionDir, ...env },
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
  throw new Error(`Timeout waiting for daemon socket: ${socketPath}`);
}

function runCli(sessionDir: string, args: string[]) {
  return spawnSync(nodeBin, [cliPath, ...args], {
    env: { ...process.env, PTY_SESSION_DIR: sessionDir },
    encoding: "utf-8",
    timeout: 15000,
  });
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

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

describe("parity R2: shared screen fixtures (node reproduces the canonical values)", () => {
  it("the fixtures file is present and versioned", () => {
    expect(fixtures.version).toBe(2);
    expect(fixtures.fixtures.length).toBeGreaterThan(0);
  });

  for (const fx of fixtures.fixtures) {
    if (fx.kind === "reaped-after-exit") {
      it(`fixture "${fx.id}" (${fx.kind}): finished session reaps itself — peek gone, ls omits`, async () => {
        const dir = makeSessionDir();
        const name = uniqueName();
        // Reap mode: the daemon removes itself as it exits, so there is no
        // persistent socket to wait for — spawn raw, let it run + reap, then
        // assert the session is gone.
        const config = JSON.stringify({
          name, command: fx.spawn.command, args: fx.spawn.args,
          displayCommand: fx.spawn.command, cwd: os.tmpdir(),
          rows: fx.spawn.rows, cols: fx.spawn.cols,
        });
        const child = spawn(nodeBin, [serverModule], {
          detached: true,
          stdio: ["ignore", "ignore", "ignore"],
          env: { ...process.env, PTY_SERVER_CONFIG: config, PTY_SESSION_DIR: dir, ...(fx.env ?? {}) },
        });
        child.unref();
        if (child.pid) bgPids.push(child.pid);
        await sleep(fx.settleMs);

        const peek = runCli(dir, ["peek", "--plain", name]);
        expect(peek.status).not.toBe(0);
        const list = JSON.parse(runCli(dir, ["list", "--json"]).stdout);
        expect(list.find((s: any) => s.name === name)).toBeUndefined();
      }, 20000);
      continue;
    }

    it(`fixture "${fx.id}" (${fx.kind}) reproduces the exact plain screen`, async () => {
      const dir = makeSessionDir();
      const name = uniqueName();
      await startDaemon(dir, name, fx.spawn.command, fx.spawn.args, fx.spawn.rows, fx.spawn.cols, fx.env ?? {});
      await sleep(fx.settleMs);

      const peek = runCli(dir, ["peek", "--plain", name]);
      expect(peek.status).toBe(0);
      const screen = peek.stdout.replace(/\n$/, "");
      // Plain-screen bytes must match EXACTLY (the core R2 contract).
      expect(screen).toBe(fx.expect.plainScreen);
      if (typeof fx.expect.plainScreenLength === "number") {
        expect(screen.length).toBe(fx.expect.plainScreenLength);
      }

      if (fx.kind === "plain-screen-after-exit") {
        const list = JSON.parse(runCli(dir, ["list", "--json"]).stdout);
        const found = list.find((s: any) => s.name === name);
        expect(found).toBeDefined();
        if (fx.expect.status) expect(found.status).toBe(fx.expect.status);
        if (typeof fx.expect.exitCode === "number") expect(found.exitCode).toBe(fx.expect.exitCode);

        // Idempotent: a second peek is byte-identical (peek does not consume).
        const again = runCli(dir, ["peek", "--plain", name]);
        expect(again.stdout).toBe(peek.stdout);
      }
    }, 20000);
  }
});
