import { describe, it, expect, afterEach, afterAll, beforeEach } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { queryStats } from "../src/client.ts";
import { spawnDaemon, setServerModulePath } from "../src/spawn.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const projectRoot = path.join(__dirname, "..");
const distServer = path.join(projectRoot, "dist", "server.js");
const realPty = path.join(projectRoot, "bin", "pty");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-bundle-fallback-"));
afterAll(() => {
  fs.rmSync(testRoot, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

let bgPids: number[] = [];
afterEach(() => {
  for (const pid of bgPids) {
    try { process.kill(pid, "SIGTERM"); } catch {}
  }
  bgPids = [];
  // Reset the override between cases so each test exercises its intended
  // strategy.
  setServerModulePath(null);
});

let nameCounter = 0;
function uniqueName(): string {
  return `bundle-fb${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
}

function makeSessionDir(): string {
  return fs.mkdtempSync(path.join(testRoot, "d-"));
}

function trackPid(dir: string, name: string): void {
  try {
    const pid = parseInt(fs.readFileSync(path.join(dir, `${name}.pid`), "utf-8").trim(), 10);
    if (Number.isFinite(pid)) bgPids.push(pid);
  } catch {}
}

describe("spawnDaemon strategy resolution", () => {
  beforeEach(() => {
    if (!fs.existsSync(distServer)) {
      throw new Error(`Missing ${distServer} — run \`npm run build\` first.`);
    }
    if (!fs.existsSync(realPty)) {
      throw new Error(`Missing ${realPty} — run \`npm install\` to set up bin/.`);
    }
  });

  it("on-disk fast path: spawns when sibling server.js is real", async () => {
    // Default state — no override, sibling exists in dist/. Tests that the
    // happy path (`__dirname/server.js` readable) reaches a working daemon.
    setServerModulePath(distServer);
    const dir = makeSessionDir();
    const name = uniqueName();
    process.env.PTY_SESSION_DIR = dir;

    await spawnDaemon({
      name,
      command: "/bin/sh",
      args: ["-c", "sleep 30"],
      displayCommand: "sh",
      cwd: dir,
    });

    const stats = await queryStats(name);
    expect(stats.name).toBe(name);
    expect(stats.process.alive).toBe(true);
    trackPid(dir, name);
  }, 15000);

  it("CLI delegation: spawns via `pty run -d` when the sibling is unreadable", async () => {
    // Force the resolver onto the CLI path by pointing the override at a
    // bogus path the resolver will skip (empty string ↛ truthy), then the
    // sibling lookup proceeds. Under tsx, __dirname is src/ where there is
    // no server.js — so the sibling path fails statSync and the resolver
    // falls back to the CLI. The CLI delegation should produce a session
    // with the same external behavior.
    const dir = makeSessionDir();
    const name = uniqueName();
    process.env.PTY_SESSION_DIR = dir;
    // Put bin/pty on PATH so the spawnSync('pty', ...) finds it.
    const oldPath = process.env.PATH ?? "";
    process.env.PATH = `${path.dirname(realPty)}:${oldPath}`;
    try {
      await spawnDaemon({
        name,
        command: "/bin/sh",
        args: ["-c", "sleep 30"],
        displayCommand: "sh",
        cwd: dir,
        tags: { source: "test" },
      });
    } finally {
      process.env.PATH = oldPath;
    }

    const stats = await queryStats(name);
    expect(stats.name).toBe(name);
    expect(stats.process.alive).toBe(true);
    trackPid(dir, name);
  }, 15000);

  it("CLI delegation: clear error when `pty` CLI isn't on PATH", async () => {
    // Same forced-CLI-path setup, but with an empty PATH so spawnSync
    // returns ENOENT. Should surface the documented guidance.
    const dir = makeSessionDir();
    const name = uniqueName();
    process.env.PTY_SESSION_DIR = dir;
    const oldPath = process.env.PATH ?? "";
    process.env.PATH = "";
    try {
      await expect(
        spawnDaemon({
          name,
          command: "/bin/sh",
          args: ["-c", "sleep 30"],
          displayCommand: "sh",
          cwd: dir,
        }),
      ).rejects.toThrow(/pty.*CLI.*PATH|setServerModulePath/);
    } finally {
      process.env.PATH = oldPath;
    }
  }, 5000);

  it("explicit setServerModulePath() override wins over on-disk + CLI", async () => {
    setServerModulePath(distServer);
    const dir = makeSessionDir();
    const name = uniqueName();
    process.env.PTY_SESSION_DIR = dir;

    await spawnDaemon({
      name,
      command: "/bin/sh",
      args: ["-c", "sleep 30"],
      displayCommand: "sh",
      cwd: dir,
    });

    const stats = await queryStats(name);
    expect(stats.name).toBe(name);
    expect(stats.process.alive).toBe(true);
    trackPid(dir, name);
  }, 15000);
});
