// Phase-2 per-network isolation: PTY_ROOT canonical env, --root flag,
// PTY_SESSION_DIR legacy alias + one-time deprecation notice, per-root
// gc plist Label + logPath.

import { describe, it, expect, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-root-"));
afterAll(() => {
  fs.rmSync(testRoot, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

function makeRoot(): string {
  return fs.mkdtempSync(path.join(testRoot, "r-"));
}

function runCli(args: string[], env: Record<string, string | undefined>) {
  const cleanEnv: Record<string, string> = {};
  for (const [k, v] of Object.entries(env)) {
    if (v !== undefined) cleanEnv[k] = v;
  }
  return spawnSync(nodeBin, [cliPath, ...args], {
    encoding: "utf8",
    env: cleanEnv,
  });
}

describe("PTY_ROOT canonical + PTY_SESSION_DIR legacy alias", () => {
  it("PTY_ROOT wins over PTY_SESSION_DIR when both set", () => {
    const winner = makeRoot();
    const loser = makeRoot();
    const res = runCli(["list", "--json"], {
      PATH: process.env.PATH,
      HOME: process.env.HOME,
      PTY_ROOT: winner,
      PTY_SESSION_DIR: loser,
      PTY_ROOT_LEGACY_SILENT: "1",
    });
    expect(res.status).toBe(0);
    // Empty registry → empty list. Both dirs exist and are empty, so
    // the assertion is really about "no crash on either path" — the
    // real precedence check is the deprecation-notice absence test
    // below, which only fires when PTY_SESSION_DIR is actually consulted.
    expect(JSON.parse(res.stdout)).toEqual([]);
  });

  it("PTY_SESSION_DIR-only path emits deprecation notice once", () => {
    const dir = makeRoot();
    const res = runCli(["list", "--json"], {
      PATH: process.env.PATH,
      HOME: process.env.HOME,
      PTY_SESSION_DIR: dir,
    });
    expect(res.status).toBe(0);
    expect(res.stderr).toMatch(/PTY_SESSION_DIR is deprecated/);
    // Emitted exactly once (single invocation → single warning line).
    expect(res.stderr.match(/PTY_SESSION_DIR is deprecated/g)?.length).toBe(1);
  });

  it("PTY_ROOT-only path emits no deprecation notice", () => {
    const dir = makeRoot();
    const res = runCli(["list", "--json"], {
      PATH: process.env.PATH,
      HOME: process.env.HOME,
      PTY_ROOT: dir,
    });
    expect(res.status).toBe(0);
    expect(res.stderr).not.toMatch(/deprecated/);
  });

  it("PTY_ROOT_LEGACY_SILENT suppresses the deprecation notice", () => {
    const dir = makeRoot();
    const res = runCli(["list", "--json"], {
      PATH: process.env.PATH,
      HOME: process.env.HOME,
      PTY_SESSION_DIR: dir,
      PTY_ROOT_LEGACY_SILENT: "1",
    });
    expect(res.status).toBe(0);
    expect(res.stderr).not.toMatch(/deprecated/);
  });
});

describe("pty --root <path> global flag", () => {
  it("--root scopes list to the given registry (empty on unused root)", () => {
    const dir = makeRoot();
    const res = runCli(["--root", dir, "list", "--json"], {
      PATH: process.env.PATH,
      HOME: process.env.HOME,
      PTY_ROOT_LEGACY_SILENT: "1",
    });
    expect(res.status).toBe(0);
    expect(JSON.parse(res.stdout)).toEqual([]);
  });

  it("--root overrides PTY_ROOT env when both are given", () => {
    const flagRoot = makeRoot();
    const envRoot = makeRoot();
    // Drop a fake metadata into envRoot so a leak (--root ignored)
    // would show up in the list. The flag path stays empty.
    fs.writeFileSync(
      path.join(envRoot, "leak.json"),
      JSON.stringify({
        command: "sh", args: [], displayCommand: "sh",
        cwd: os.tmpdir(), rows: 24, cols: 80, tags: {}, pid: 999999,
        createdAt: new Date().toISOString(),
      }),
    );
    const res = runCli(["--root", flagRoot, "list", "--json"], {
      PATH: process.env.PATH,
      HOME: process.env.HOME,
      PTY_ROOT: envRoot,
    });
    expect(res.status).toBe(0);
    expect(JSON.parse(res.stdout)).toEqual([]);
  });

  it("--root without a value exits non-zero with a clear error", () => {
    const res = runCli(["--root"], {
      PATH: process.env.PATH,
      HOME: process.env.HOME,
    });
    expect(res.status).not.toBe(0);
    expect(res.stderr).toMatch(/--root requires a path/);
  });

  it("--root followed by another flag exits non-zero (no value swallow)", () => {
    const res = runCli(["--root", "--json", "list"], {
      PATH: process.env.PATH,
      HOME: process.env.HOME,
    });
    expect(res.status).not.toBe(0);
    expect(res.stderr).toMatch(/--root requires a path/);
  });
});

describe("pty gc --print-launchd-plist per-root Label + logPath", () => {
  function plistFor(env: Record<string, string | undefined>) {
    const res = runCli(["gc", "--print-launchd-plist"], {
      PATH: process.env.PATH,
      HOME: process.env.HOME,
      PTY_ROOT_LEGACY_SILENT: "1",
      ...env,
    });
    expect(res.status).toBe(0);
    return res.stdout;
  }

  it("default root keeps the legacy Label (backwards compat)", () => {
    // No PTY_ROOT / PTY_SESSION_DIR — resolves to DEFAULT_SESSION_DIR.
    const plist = plistFor({});
    expect(plist).toContain("<string>com.myobie.pty.gc</string>");
    expect(plist).not.toContain("<string>com.myobie.pty.gc.");
  });

  it("non-default root gets a suffixed Label (basename of root)", () => {
    const dir = path.join(testRoot, "my-network");
    fs.mkdirSync(dir, { recursive: true });
    const plist = plistFor({ PTY_ROOT: dir });
    expect(plist).toContain("<string>com.myobie.pty.gc.my-network</string>");
  });

  it("non-default root's logPath lives inside that root", () => {
    const dir = path.join(testRoot, "another-net");
    fs.mkdirSync(dir, { recursive: true });
    const plist = plistFor({ PTY_ROOT: dir });
    expect(plist).toContain(`<string>${dir}/gc.log</string>`);
  });

  it("emits PTY_ROOT (canonical) in the plist's EnvironmentVariables", () => {
    const dir = path.join(testRoot, "envcheck");
    fs.mkdirSync(dir, { recursive: true });
    const plist = plistFor({ PTY_ROOT: dir });
    // The key line for PTY_ROOT appears; PTY_SESSION_DIR does NOT appear
    // in the emitted plist (we no longer bake the legacy name).
    expect(plist).toContain("<key>PTY_ROOT</key>");
    expect(plist).not.toContain("<key>PTY_SESSION_DIR</key>");
  });

  it("sanitizes a pathological basename into a safe Label suffix", () => {
    const dir = path.join(testRoot, "weird name with spaces");
    fs.mkdirSync(dir, { recursive: true });
    const plist = plistFor({ PTY_ROOT: dir });
    // Spaces collapse to a single hyphen; label stays reverse-DNS-safe.
    expect(plist).toContain("<string>com.myobie.pty.gc.weird-name-with-spaces</string>");
  });
});

describe("run -d honors PTY_ROOT for isolation; PTY_SESSION_DIR masking is visible", () => {
  // Regression for the scratch-session leak: a detached session must land in
  // PTY_ROOT and NOT in a co-set (deprecated) PTY_SESSION_DIR or the default
  // registry. PTY_ROOT is the isolation mechanism; PTY_SESSION_DIR is legacy.
  it("a -d session lands ONLY under PTY_ROOT (not the co-set PTY_SESSION_DIR, not the default)", () => {
    const root = makeRoot();
    const scratch = makeRoot(); // legacy var that must be ignored
    const name = `rd${Math.random().toString(36).slice(2, 7)}`;
    // `cat` waits on stdin, so the session stays running while we inspect.
    const res = runCli(["run", "-d", "--id", name, "--", "cat"], {
      PATH: process.env.PATH,
      HOME: process.env.HOME,
      PTY_ROOT: root,
      PTY_SESSION_DIR: scratch,
      PTY_ROOT_LEGACY_SILENT: "1",
    });
    try {
      expect(res.status).toBe(0);
      // Landed under PTY_ROOT.
      expect(fs.existsSync(path.join(root, `${name}.json`))).toBe(true);
      expect(fs.existsSync(path.join(root, `${name}.sock`))).toBe(true);
      // NOT under the deprecated PTY_SESSION_DIR.
      expect(fs.existsSync(path.join(scratch, `${name}.json`))).toBe(false);
      // NOT in the real default registry.
      const def = path.join(os.homedir(), ".local", "state", "pty", `${name}.json`);
      expect(fs.existsSync(def)).toBe(false);
    } finally {
      runCli(["kill", name], {
        PATH: process.env.PATH,
        HOME: process.env.HOME,
        PTY_ROOT: root,
        PTY_ROOT_LEGACY_SILENT: "1",
      });
    }
  });

  it("warns (once) that PTY_ROOT wins when PTY_SESSION_DIR is also set", () => {
    const res = runCli(["list", "--json"], {
      PATH: process.env.PATH,
      HOME: process.env.HOME,
      PTY_ROOT: makeRoot(),
      PTY_SESSION_DIR: makeRoot(),
      // no PTY_ROOT_LEGACY_SILENT → the masking warning should surface
    });
    expect(res.status).toBe(0);
    expect(res.stderr).toMatch(/both PTY_ROOT and PTY_SESSION_DIR are set/);
    // Exactly once per invocation.
    expect(res.stderr.match(/both PTY_ROOT and PTY_SESSION_DIR are set/g)?.length).toBe(1);
  });

  it("PTY_ROOT_LEGACY_SILENT suppresses the masking warning", () => {
    const res = runCli(["list", "--json"], {
      PATH: process.env.PATH,
      HOME: process.env.HOME,
      PTY_ROOT: makeRoot(),
      PTY_SESSION_DIR: makeRoot(),
      PTY_ROOT_LEGACY_SILENT: "1",
    });
    expect(res.status).toBe(0);
    expect(res.stderr).not.toMatch(/both PTY_ROOT and PTY_SESSION_DIR/);
  });
});
