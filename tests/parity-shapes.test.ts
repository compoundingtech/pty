// PARITY DRIVE — Round 2: shared JSON-SHAPE fixtures (companion to
// parity-fixtures.test.ts / screens.json).
//
// tests/fixtures/parity/shapes.json is the canonical source both suites assert.
// Node OWNS it; pty-rust mirrors byte-identical. Unlike the plain-screen
// fixtures (exact bytes), these assert machine-readable output FIELD-BY-FIELD
// per policy: {exact:v} | {type:'number'|'string'} | {omitWhenUnset:true}.

import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");

const shapesPath = path.join(__dirname, "fixtures", "parity", "shapes.json");
const shapes = JSON.parse(fs.readFileSync(shapesPath, "utf-8")) as {
  version: number;
  fixtures: any[];
};

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-shapes-"));
afterAll(() => {
  fs.rmSync(testRoot, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
});

let sessionDirs: string[] = [];
function makeSessionDir(): string {
  const dir = fs.mkdtempSync(path.join(testRoot, "d-"));
  sessionDirs.push(dir);
  return dir;
}

function runCli(dir: string, args: string[], env: Record<string, string> = {}) {
  return spawnSync(nodeBin, [cliPath, ...args], {
    env: { ...process.env, PTY_SESSION_DIR: dir, ...env },
    encoding: "utf-8",
    timeout: 15000,
  });
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Wait (bounded) for every pid to leave the process table. */
async function waitForPidsGone(pids: number[], budgetMs = 5000): Promise<void> {
  const alive = (pid: number) => { try { process.kill(pid, 0); return true; } catch { return false; } };
  const deadline = Date.now() + budgetMs;
  while (Date.now() < deadline && pids.some(alive)) {
    await new Promise((r) => setTimeout(r, 50));
  }
}

afterEach(async () => {
  // Kill every daemon this test spawned (found via its .pid file), then WAIT
  // for it to actually leave the process table before removing files. A
  // SIGTERM'd daemon keeps writing as it shuts down (exit metadata via
  // tmp+rename, socket/pid cleanup); letting that race afterAll's recursive
  // rmSync re-creates a file into a just-emptied dir → ENOTEMPTY, a false-red
  // teardown flake. Waiting here means nothing is writing during teardown.
  const pids: number[] = [];
  for (const dir of sessionDirs) {
    try {
      for (const e of fs.readdirSync(dir)) {
        if (e.endsWith(".pid")) {
          const p = Number(fs.readFileSync(path.join(dir, e), "utf8").trim());
          if (p > 0) pids.push(p);
        }
      }
    } catch {}
  }
  for (const p of pids) { try { process.kill(p, "SIGTERM"); } catch {} }
  await waitForPidsGone(pids);
  for (const dir of sessionDirs) {
    try {
      for (const e of fs.readdirSync(dir)) { try { fs.unlinkSync(path.join(dir, e)); } catch {} }
    } catch {}
  }
  sessionDirs = [];
});

// Assert one field of `entry` against a policy: exact | type | omit-when-unset.
function assertField(entry: any, key: string, policy: any, label: string): void {
  if (policy.omitWhenUnset) {
    expect(entry[key], `${label}.${key} should be omitted when unset`).toBeUndefined();
  } else if ("exact" in policy) {
    expect(entry[key], `${label}.${key} exact`).toStrictEqual(policy.exact);
  } else if (policy.type) {
    expect(entry[key], `${label}.${key} present`).not.toBeNull();
    expect(typeof entry[key], `${label}.${key} type`).toBe(policy.type);
  } else {
    throw new Error(`no assertable policy for ${label}.${key}`);
  }
}

describe("parity R2: shared JSON-shape fixtures (node reproduces the canonical shapes)", () => {
  it("the shapes file is present and versioned", () => {
    expect(shapes.version).toBe(1);
    expect(shapes.fixtures.length).toBeGreaterThan(0);
  });

  for (const fx of shapes.fixtures) {
    if (fx.kind === "ls-json-shape") {
      it(`shape "${fx.id}": ls --json entry shape (running + exited)`, async () => {
        const dir = makeSessionDir();
        for (const sess of fx.sessions) {
          const r = runCli(dir, sess.run, fx.env ?? {});
          expect(r.status, `run ${sess.id} failed: ${r.stderr}`).toBe(0);
        }
        await sleep(fx.settleMs);

        const list = JSON.parse(runCli(dir, ["list", "--json"]).stdout);
        for (const sess of fx.sessions) {
          const entry = list.find((e: any) => e.name === sess.expect.name.exact);
          expect(entry, `entry ${sess.id} present in ls --json`).toBeDefined();
          if (fx.statusEnum) expect(fx.statusEnum).toContain(entry.status);
          for (const [key, policy] of Object.entries(sess.expect)) {
            assertField(entry, key, policy, sess.id);
          }
        }
      }, 25000);
      continue;
    }

    if (fx.kind === "stats-clients") {
      it(`shape "${fx.id}": transient peek is not an attached client`, async () => {
        const dir = makeSessionDir();
        const r = runCli(dir, fx.run, fx.env ?? {});
        expect(r.status, `run failed: ${r.stderr}`).toBe(0);
        await sleep(fx.settleMs);

        const name = fx.run[fx.run.indexOf("--id") + 1];
        if (fx.peekFirst) {
          const peek = runCli(dir, ["peek", "--plain", name]);
          expect(peek.status).toBe(0);
        }
        const stats = JSON.parse(runCli(dir, ["stats", "--json", name]).stdout);
        for (const [key, policy] of Object.entries(fx.expect.clients)) {
          assertField(stats.clients, key, policy, `${fx.id}.clients`);
        }
      }, 25000);
      continue;
    }
  }
});
