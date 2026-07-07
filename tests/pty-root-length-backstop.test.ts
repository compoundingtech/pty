// Fail-loud startup backstop when PTY_ROOT is too long for the socket-path
// kernel limit — errors before any subcommand runs.

import { describe, it, expect, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-root-len-"));
afterAll(() => {
  fs.rmSync(testRoot, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
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
