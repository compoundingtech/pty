// Nesting prevention: pty attach / restart / interactive / run -a all
// refuse to start a client inside a session that's already a pty
// client, because detach keybindings would route to the outer client
// and the user gets tangled. --force overrides.
//
// Request originated from pty-layout-claude: every pty-layout pane
// runs a shell with PTY_SESSION set, so `pty attach <other>` inside
// a pane would silently create a nested client.

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

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-nest-"));
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
  return `nst${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
}

async function startDaemon(sessionDir: string, name: string, command = "cat"): Promise<number> {
  const config = JSON.stringify({
    name, command, args: [], displayCommand: command,
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

// Run the CLI with a controlled PTY_SESSION env — this is the key
// fixture for nesting-prevention tests. The outer vitest process may
// or may not have PTY_SESSION set (it usually is when the harness
// itself runs inside a pty session), so we always set it explicitly.
function runCliNested(
  sessionDir: string,
  ptySession: string | null,
  ...args: string[]
) {
  const env: Record<string, string | undefined> = {
    ...process.env,
    PTY_SESSION_DIR: sessionDir,
  };
  if (ptySession === null) {
    delete env.PTY_SESSION;
  } else {
    env.PTY_SESSION = ptySession;
  }
  return spawnSync(nodeBin, [cliPath, ...args], {
    env: env as Record<string, string>,
    encoding: "utf-8",
    timeout: 10_000,
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

describe("pty attach", () => {
  it("refuses when PTY_SESSION is set; exits non-zero with a clear message", async () => {
    const dir = makeSessionDir();
    const target = uniqueName();
    await startDaemon(dir, target);

    const r = runCliNested(dir, "outer-session", "attach", target);
    expect(r.status).not.toBe(0);
    expect(r.stderr).toContain('already inside pty session "outer-session"');
    expect(r.stderr).toMatch(/--force/i);
  }, 15000);

  it("refuses even for a dead-session attach (the restart-then-attach path)", async () => {
    const dir = makeSessionDir();
    const target = uniqueName();
    // Write metadata only — no daemon — so cmdAttach takes the dead-session
    // branch that would otherwise call doAttach after restart.
    fs.writeFileSync(
      path.join(dir, `${target}.json`),
      JSON.stringify({
        command: "cat", args: [], displayCommand: "cat",
        cwd: os.tmpdir(),
        createdAt: new Date().toISOString(),
        exitedAt: new Date().toISOString(),
        exitCode: 0,
      }),
    );

    const r = runCliNested(dir, "outer-session", "attach", "-r", target);
    expect(r.status).not.toBe(0);
    // Early guard — should not even reach the "Session X exited" / restart prompt.
    expect(r.stderr).toContain('already inside pty session "outer-session"');
    expect(r.stdout).not.toContain("Restart?");
  }, 15000);

  it("--force bypasses the guard — reaches the 'not found' check instead of the nesting refusal", () => {
    // Attach to a session that doesn't exist. Without --force: hits the
    // nesting guard and complains about nesting. With --force: skips the
    // guard and hits the normal "Session X not found." path.
    const dir = makeSessionDir();
    const bogus = `no-such-${Math.random().toString(36).slice(2, 8)}`;

    const refused = runCliNested(dir, "outer-session", "attach", bogus);
    expect(refused.status).not.toBe(0);
    expect(refused.stderr).toContain('already inside pty session "outer-session"');

    const forced = runCliNested(dir, "outer-session", "attach", "--force", bogus);
    expect(forced.status).not.toBe(0);
    expect(forced.stderr).not.toContain("already inside pty session");
    expect(forced.stderr).toMatch(/not found/);
  });

  it("--force can appear before or after -r in the argv", () => {
    // Same trick as above — use a non-existent session so the --force path
    // reaches the fast "not found" exit instead of the blocking attach.
    const dir = makeSessionDir();
    const bogus = `no-such-${Math.random().toString(36).slice(2, 8)}`;

    const a = runCliNested(dir, "outer", "attach", "--force", "-r", bogus);
    expect(a.stderr).not.toContain("already inside pty session");
    expect(a.stderr).toMatch(/not found/);

    const b = runCliNested(dir, "outer", "attach", "-r", "--force", bogus);
    expect(b.stderr).not.toContain("already inside pty session");
    expect(b.stderr).toMatch(/not found/);
  });
});

describe("pty restart", () => {
  it("restarts the session but skips the trailing attach when nested", async () => {
    const dir = makeSessionDir();
    const target = uniqueName();
    await startDaemon(dir, target);

    const r = runCliNested(dir, "outer-session", "restart", "-y", target);
    expect(r.status).toBe(0);
    expect(r.stdout).toContain(`Session "${target}" restarted.`);
    expect(r.stdout).toMatch(/not attached.*outer-session/);
  }, 20000);

  it("--force restores restart+attach behavior (attach actually runs)", async () => {
    const dir = makeSessionDir();
    const target = uniqueName();
    await startDaemon(dir, target);

    // With --force we expect the old behavior: restart, then attach. Attach
    // blocks on stdin in a non-TTY; we just verify the "not attached" notice
    // is absent.
    const r = runCliNested(dir, "outer-session", "restart", "-y", "--force", target);
    expect(r.stdout).not.toMatch(/not attached/);
  }, 20000);

  it("non-nested restart unchanged (attaches after restart)", async () => {
    const dir = makeSessionDir();
    const target = uniqueName();
    await startDaemon(dir, target);

    const r = runCliNested(dir, null, "restart", "-y", target);
    expect(r.stdout).toContain(`Session "${target}" restarted.`);
    expect(r.stdout).not.toMatch(/not attached/);
  }, 20000);
});

describe("pty interactive / bare pty", () => {
  it("refuses bare `pty` when nested", () => {
    const dir = makeSessionDir();
    const r = runCliNested(dir, "outer-session");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toContain("already inside pty session");
    expect(r.stderr).toMatch(/interactive picker|Ctrl/i);
  });

  it("refuses `pty i` when nested", () => {
    const dir = makeSessionDir();
    const r = runCliNested(dir, "outer-session", "i");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toContain("already inside pty session");
  });

  it("refuses `pty interactive` when nested", () => {
    const dir = makeSessionDir();
    const r = runCliNested(dir, "outer-session", "interactive");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toContain("already inside pty session");
  });

  it("--force bypasses the guard (won't actually run the picker in a non-TTY env, but guard doesn't fire)", () => {
    // The TUI picker itself will bail quickly when stdin isn't a TTY, so we
    // just verify our nesting error isn't the reason for exiting.
    const dir = makeSessionDir();
    const r = runCliNested(dir, "outer-session", "--force");
    expect(r.stderr).not.toContain("already inside pty session");
  });
});

describe("pty run -a", () => {
  it("refuses when target is already running AND nested", async () => {
    const dir = makeSessionDir();
    const target = uniqueName();
    await startDaemon(dir, target);

    const r = runCliNested(dir, "outer-session", "run", "-a", "--id", target, "--", "cat");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toContain('already inside pty session "outer-session"');
    expect(r.stderr).toContain(target);
  }, 15000);

  it("falls through to run-directly when target is NOT running (current behavior preserved)", async () => {
    const dir = makeSessionDir();
    const target = uniqueName();
    // No daemon started — target doesn't exist.

    const r = runCliNested(dir, "outer-session", "run", "-a", "--id", target, "--", "true");
    // Should hit the exec-directly path: exec `true` which exits 0.
    expect(r.stderr).toContain("Already inside pty session");
    expect(r.stderr).toContain("running directly");
  }, 10000);

  it("--force (nested) bypasses run's guard and CREATES a session instead of running in-place", async () => {
    const dir = makeSessionDir();
    const target = uniqueName();
    // Old behavior: --force was a no-op on `run`, so a nested `run --force` ran
    // the command directly in-place and created NO session. New behavior
    // (parity decision a, 2026-07-21): --force bypasses run's nesting guard the
    // same way it bypasses attach's/restart's, creating a real (nested) session
    // and attaching to it. Launch detached and poll until it appears running —
    // the old in-place path never produced a session at all.
    const child = spawn(nodeBin, [cliPath, "run", "--force", "--id", target, "--", "cat"], {
      detached: true,
      stdio: ["ignore", "ignore", "ignore"],
      env: { ...process.env, PTY_SESSION_DIR: dir, PTY_SESSION: "outer-session" },
    });
    child.unref();
    bgPids.push(child.pid!);

    let found: any;
    const start = Date.now();
    while (Date.now() - start < 8000) {
      const list = JSON.parse(runCliNested(dir, "outer-session", "list", "--json").stdout);
      found = list.find((s: any) => s.name === target);
      if (found && found.status === "running") break;
      await new Promise((r) => setTimeout(r, 150));
    }
    expect(found).toBeDefined();
    expect(found.status).toBe("running");

    if (found?.pid) { bgPids.push(found.pid); try { process.kill(found.pid, "SIGTERM"); } catch {} }
  }, 20000);

  it("plain `pty run` (no -a) unchanged when nested: runs directly", async () => {
    const dir = makeSessionDir();

    const r = runCliNested(dir, "outer-session", "run", "--", "true");
    expect(r.stderr).toContain("Already inside pty session");
    expect(r.stderr).toContain("running directly");
    expect(r.status).toBe(0);
  }, 10000);
});
