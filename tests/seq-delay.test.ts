import { describe, it, expect, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, spawnSync } from "node:child_process";
import { resolveSeqDelayMs, DEFAULT_SEQ_DELAY_MS } from "../src/client.ts";

// (a) default (no --with-delay) inserts 0.3s between --seq items;
// (b) --with-delay 0 = straight stream, NO spacing (the escape hatch);
// (c) --with-delay N = N.

describe("resolveSeqDelayMs — the `pty send --seq` delay decision", () => {
  it("(a) defaults to 0.3s when --with-delay is absent", () => {
    expect(DEFAULT_SEQ_DELAY_MS).toBe(300);
    expect(resolveSeqDelayMs(undefined)).toBe(300);
  });

  it("(b) --with-delay 0 resolves to 0 (straight stream, no spacing)", () => {
    expect(resolveSeqDelayMs(0)).toBe(0);
  });

  it("(c) --with-delay N resolves to N * 1000 ms", () => {
    expect(resolveSeqDelayMs(0.1)).toBe(100);
    expect(resolveSeqDelayMs(0.5)).toBe(500);
    expect(resolveSeqDelayMs(2)).toBe(2000);
  });
});

// End-to-end: prove the resolved delay is actually applied by `pty send`.
// Timing is compared as a DELTA against the straight-stream baseline so node
// startup cancels out and the assertions stay robust.
const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const serverModule = path.join(__dirname, "..", "dist", "server.js");
const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-seq-"));
const bgPids: number[] = [];
afterAll(() => {
  for (const pid of bgPids) { try { process.kill(pid, "SIGTERM"); } catch {} }
  fs.rmSync(testRoot, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

async function startDaemon(dir: string, name: string): Promise<void> {
  const config = JSON.stringify({
    name, command: "cat", args: [], displayCommand: "cat",
    cwd: os.tmpdir(), rows: 24, cols: 80,
  });
  const child = spawn(nodeBin, [serverModule], {
    detached: true, stdio: ["ignore", "ignore", "ignore"],
    env: { ...process.env, PTY_SERVER_CONFIG: config, PTY_SESSION_DIR: dir },
  });
  child.unref();
  if (child.pid) bgPids.push(child.pid);
  const sock = path.join(dir, `${name}.sock`);
  const start = Date.now();
  while (Date.now() - start < 5000) {
    try { fs.statSync(sock); return; } catch {}
    await new Promise((r) => setTimeout(r, 50));
  }
  throw new Error(`daemon "${name}" never started`);
}

/** Wall-clock ms of one `pty send` invocation. */
function timeSend(dir: string, args: string[]): number {
  const start = Date.now();
  spawnSync(nodeBin, [cliPath, "send", ...args], {
    env: { ...process.env, PTY_SESSION_DIR: dir, PTY_ROOT_LEGACY_SILENT: "1" },
    encoding: "utf8", timeout: 20000,
  });
  return Date.now() - start;
}

describe("pty send --seq delay is applied end-to-end", () => {
  // 4 items = 3 inter-item gaps → default ≈ 900ms of spacing, plenty above the
  // node-startup noise floor once we subtract the straight-stream baseline.
  const ITEMS = ["--seq", "a", "--seq", "b", "--seq", "c", "--seq", "d"];

  it("default spaces items but --with-delay 0 does not (0 = straight stream)", async () => {
    const dir = fs.mkdtempSync(path.join(testRoot, "d-"));
    const name = `sq${Math.random().toString(36).slice(2, 6)}`;
    await startDaemon(dir, name);

    const straight = timeSend(dir, [name, "--with-delay", "0", ...ITEMS]);
    const dflt = timeSend(dir, [name, ...ITEMS]);

    // The default adds ~900ms (3 × 0.3s) over the straight stream. Generous
    // margin (≥ 600ms) keeps it robust under load.
    expect(dflt - straight).toBeGreaterThan(600);
    // And the straight stream itself is quick (no 0.9s of inserted delay).
    expect(straight).toBeLessThan(dflt - 500);
  }, 30000);

  it("--with-delay N scales the spacing", async () => {
    const dir = fs.mkdtempSync(path.join(testRoot, "d-"));
    const name = `sq${Math.random().toString(36).slice(2, 6)}`;
    await startDaemon(dir, name);

    const straight = timeSend(dir, [name, "--with-delay", "0", ...ITEMS]);
    const slow = timeSend(dir, [name, "--with-delay", "0.2", ...ITEMS]);

    // 3 gaps × 0.2s ≈ 600ms over the straight baseline.
    expect(slow - straight).toBeGreaterThan(400);
  }, 30000);
});
