import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-dn-"));
afterAll(() => {
  fs.rmSync(testRoot, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

let sessionDirs: string[] = [];
let bgPids: number[] = [];

function makeSessionDir(): string {
  const dir = fs.mkdtempSync(path.join(testRoot, "d-"));
  sessionDirs.push(dir);
  return dir;
}

function runCli(sessionDir: string, env: Record<string, string>, ...args: string[]) {
  // Strip PTY_SESSION from the parent environment so tests can explicitly
  // decide whether the CLI invocation should look like it's "inside" a
  // session. Otherwise the test runner's own PTY_SESSION (if any) leaks in.
  const baseEnv = { ...process.env } as Record<string, string | undefined>;
  delete baseEnv.PTY_SESSION;
  return spawnSync(nodeBin, [cliPath, ...args], {
    cwd: os.tmpdir(),
    env: { ...baseEnv, PTY_SESSION_DIR: sessionDir, ...env } as Record<string, string>,
    encoding: "utf-8",
    timeout: 15000,
  });
}

function listJson(sessionDir: string): any[] {
  const r = runCli(sessionDir, {}, "list", "--json");
  if (!r.stdout.trim()) return [];
  return JSON.parse(r.stdout);
}

function collectPid(dir: string, name: string) {
  try {
    const pidFile = path.join(dir, `${name}.pid`);
    const pid = parseInt(fs.readFileSync(pidFile, "utf-8").trim(), 10);
    if (!isNaN(pid)) bgPids.push(pid);
  } catch {}
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

describe("pty run default: random name + auto displayName", () => {
  it("generates a short random name and a cwd+command-style displayName", () => {
    const dir = makeSessionDir();
    const r = runCli(dir, {}, "run", "-d", "--", "cat");
    expect(r.status).toBe(0);

    const sessions = listJson(dir);
    expect(sessions).toHaveLength(1);
    const s = sessions[0];
    // Random names from randomSessionName() are 8 chars, all lowercase alnum
    expect(s.name).toMatch(/^[a-z0-9]{6,12}$/);
    expect(typeof s.displayName).toBe("string");
    expect(s.displayName.length).toBeGreaterThan(0);
    collectPid(dir, s.name);
  });
});

describe("--no-display-name: random name, no displayName", () => {
  it("leaves displayName unset", () => {
    const dir = makeSessionDir();
    const r = runCli(dir, {}, "run", "-d", "--no-display-name", "--", "cat");
    expect(r.status).toBe(0);

    const sessions = listJson(dir);
    expect(sessions).toHaveLength(1);
    const s = sessions[0];
    expect(s.name).toMatch(/^[a-z0-9]{6,12}$/);
    expect(s.displayName).toBeUndefined();
    collectPid(dir, s.name);
  });
});

describe("--id: explicit on-disk id", () => {
  it("honors --id (pinned on-disk id) and auto-generates a displayName", () => {
    const dir = makeSessionDir();
    const r = runCli(dir, {}, "run", "-d", "--id", "mysvc", "--", "cat");
    expect(r.status).toBe(0);

    const sessions = listJson(dir);
    const s = sessions.find((s: any) => s.name === "mysvc")!;
    expect(s).toBeDefined();
    expect(s.displayName).toBeTruthy();
    collectPid(dir, "mysvc");
  });

  it("--id combined with --no-display-name skips displayName", () => {
    const dir = makeSessionDir();
    const r = runCli(dir, {}, "run", "-d", "--id", "raw", "--no-display-name", "--", "cat");
    expect(r.status).toBe(0);

    const sessions = listJson(dir);
    const s = sessions.find((s: any) => s.name === "raw")!;
    expect(s.displayName).toBeUndefined();
    collectPid(dir, "raw");
  });

  it("--id combined with explicit --name pins both id and display label", () => {
    const dir = makeSessionDir();
    const r = runCli(dir, {}, "run", "-d", "--id", "svc", "--name", "My Pretty Service", "--", "cat");
    expect(r.status).toBe(0);

    const sessions = listJson(dir);
    const s = sessions.find((s: any) => s.name === "svc")!;
    expect(s).toBeDefined();
    expect(s.displayName).toBe("My Pretty Service");
    collectPid(dir, "svc");
  });

  it("rejects an --id whose sock path would exceed the kernel limit", () => {
    const dir = makeSessionDir();
    const longId = "x".repeat(120);
    const r = runCli(dir, {}, "run", "-d", "--id", longId, "--", "cat");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/exceeds the.*byte kernel limit|too long/i);
  });

  it("rejects an --id that collides with an existing session", () => {
    const dir = makeSessionDir();
    runCli(dir, {}, "run", "-d", "--id", "dup", "--", "cat");
    const r = runCli(dir, {}, "run", "-d", "--id", "dup", "--", "cat");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toContain("already in use");
    collectPid(dir, "dup");
  });
});

describe("--name: explicit display label (any length / chars)", () => {
  it("accepts a long display label that would not be a valid id", () => {
    const dir = makeSessionDir();
    const longLabel = "My Very Long Display Label With Spaces and Punctuation";
    const r = runCli(dir, {}, "run", "-d", "--name", longLabel, "--", "cat");
    expect(r.status).toBe(0);

    const sessions = listJson(dir);
    expect(sessions).toHaveLength(1);
    expect(sessions[0].displayName).toBe(longLabel);
    // The on-disk name is a random short id, not the display label.
    expect(sessions[0].name).toMatch(/^[a-z0-9]{6,12}$/);
    expect(sessions[0].name).not.toBe(longLabel);
    collectPid(dir, sessions[0].name);
  });

  it("rejects an --id equal to the explicit --name", () => {
    const dir = makeSessionDir();
    const r = runCli(dir, {}, "run", "-d", "--id", "same", "--name", "same", "--", "cat");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/cannot equal/i);
  });

  it("rejects an --name that collides with another session", () => {
    const dir = makeSessionDir();
    runCli(dir, {}, "run", "-d", "--id", "a1", "--name", "shared", "--", "cat");
    const r = runCli(dir, {}, "run", "-d", "--id", "a2", "--name", "shared", "--", "cat");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toContain("already in use");
    collectPid(dir, "a1");
  });
});

describe("pty rename (outside a session)", () => {
  it("pty rename <ref> <new> sets displayName", () => {
    const dir = makeSessionDir();
    runCli(dir, {}, "run", "-d", "--id", "webapp", "--no-display-name", "--", "cat");

    const r = runCli(dir, {}, "rename", "webapp", "my-label");
    expect(r.status).toBe(0);
    expect(r.stdout).toContain("my-label");

    const s = listJson(dir).find((s: any) => s.name === "webapp")!;
    expect(s.displayName).toBe("my-label");
    collectPid(dir, "webapp");
  });

  it("pty rename --show <ref> prints current displayName", () => {
    const dir = makeSessionDir();
    runCli(dir, {}, "run", "-d", "--id", "api", "--no-display-name", "--", "cat");
    runCli(dir, {}, "rename", "api", "friendly-api");

    const r = runCli(dir, {}, "rename", "--show", "api");
    expect(r.status).toBe(0);
    expect(r.stdout.trim()).toBe("friendly-api");
    collectPid(dir, "api");
  });

  it("pty rename --show <ref> without displayName prints a hint", () => {
    const dir = makeSessionDir();
    runCli(dir, {}, "run", "-d", "--id", "bare", "--no-display-name", "--", "cat");

    const r = runCli(dir, {}, "rename", "--show", "bare");
    expect(r.status).toBe(0);
    expect(r.stdout).toContain("no displayName");
    collectPid(dir, "bare");
  });

  it("pty rename --clear <ref> removes displayName", () => {
    const dir = makeSessionDir();
    runCli(dir, {}, "run", "-d", "--id", "svc", "--no-display-name", "--", "cat");
    runCli(dir, {}, "rename", "svc", "pretty");
    expect(listJson(dir).find((s: any) => s.name === "svc")!.displayName).toBe("pretty");

    const r = runCli(dir, {}, "rename", "--clear", "svc");
    expect(r.status).toBe(0);
    expect(listJson(dir).find((s: any) => s.name === "svc")!.displayName).toBeUndefined();
    collectPid(dir, "svc");
  });

  it("pty rename with one positional arg OUTSIDE a session errors with usage", () => {
    const dir = makeSessionDir();
    const r = runCli(dir, {}, "rename", "only-one");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toContain("only allowed inside a pty session");
  });

  it("rejects displayName that collides with another session's name", () => {
    const dir = makeSessionDir();
    runCli(dir, {}, "run", "-d", "--id", "aaa", "--no-display-name", "--", "cat");
    runCli(dir, {}, "run", "-d", "--id", "bbb", "--no-display-name", "--", "cat");

    const r = runCli(dir, {}, "rename", "aaa", "bbb");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toContain("already in use");
    collectPid(dir, "aaa");
    collectPid(dir, "bbb");
  });

  it("rejects displayName equal to the session's own name", () => {
    const dir = makeSessionDir();
    runCli(dir, {}, "run", "-d", "--id", "same", "--no-display-name", "--", "cat");
    const r = runCli(dir, {}, "rename", "same", "same");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toContain("cannot equal");
    collectPid(dir, "same");
  });
});

describe("pty rename (inside a session)", () => {
  it("pty rename <new> with PTY_SESSION set renames the current session", () => {
    const dir = makeSessionDir();
    runCli(dir, {}, "run", "-d", "--id", "insider", "--no-display-name", "--", "cat");

    const r = runCli(dir, { PTY_SESSION: "insider" }, "rename", "from-inside");
    expect(r.status).toBe(0);
    const s = listJson(dir).find((s: any) => s.name === "insider")!;
    expect(s.displayName).toBe("from-inside");
    collectPid(dir, "insider");
  });

  it("pty rename --clear with PTY_SESSION clears the current session's displayName", () => {
    const dir = makeSessionDir();
    runCli(dir, {}, "run", "-d", "--id", "i2", "--no-display-name", "--", "cat");
    runCli(dir, { PTY_SESSION: "i2" }, "rename", "has-a-display");

    const r = runCli(dir, { PTY_SESSION: "i2" }, "rename", "--clear");
    expect(r.status).toBe(0);
    expect(listJson(dir).find((s: any) => s.name === "i2")!.displayName).toBeUndefined();
    collectPid(dir, "i2");
  });
});

describe("lookup by displayName", () => {
  it("pty list references a session by its displayName for peek/stats/etc", () => {
    const dir = makeSessionDir();
    runCli(dir, {}, "run", "-d", "--id", "raw1", "--no-display-name", "--", "cat");
    runCli(dir, {}, "rename", "raw1", "friendly");

    // stats by displayName should resolve the same session as by name
    const r = runCli(dir, {}, "stats", "friendly", "--json");
    expect(r.status).toBe(0);
    const stats = JSON.parse(r.stdout);
    expect(stats.name).toBe("raw1");
    collectPid(dir, "raw1");
  });
});

describe("commands resolve a long displayName even when it would fail validateName", () => {
  // Regression test for schickling-assistant's PR #45 finding:
  // long displayNames could be CREATED but not OPERATED on, because the
  // command handlers ran `validateName(ref)` before `resolveRef(ref)`.
  // `validateName` is the strict, sock-path-bounded validator; a long
  // displayName legitimately fails it. Resolution paths must NOT run
  // strict validation — that's reserved for id creation.
  const LONG_LABEL = "org.cos.orc-payments-platform.orc-checkout-api.worker-authz-service.subworker-db-migrations.verifier-contracts";

  it("creates a 110-char displayName cleanly", () => {
    const dir = makeSessionDir();
    expect(LONG_LABEL.length).toBe(110);
    const r = runCli(dir, {}, "run", "-d", "--name", LONG_LABEL, "--", "cat");
    expect(r.status).toBe(0);
    const sessions = listJson(dir);
    expect(sessions).toHaveLength(1);
    expect(sessions[0].displayName).toBe(LONG_LABEL);
    collectPid(dir, sessions[0].name);
  });

  it("pty peek <longDisplayName> works", () => {
    const dir = makeSessionDir();
    runCli(dir, {}, "run", "-d", "--name", LONG_LABEL, "--", "cat");
    const r = runCli(dir, {}, "peek", "--plain", LONG_LABEL);
    expect(r.status).toBe(0);
    // Don't assert on screen content — just that the command resolves and exits 0.
    collectPid(dir, listJson(dir)[0].name);
  });

  it("pty send <longDisplayName> works", () => {
    const dir = makeSessionDir();
    runCli(dir, {}, "run", "-d", "--name", LONG_LABEL, "--", "cat");
    const r = runCli(dir, {}, "send", LONG_LABEL, "hi");
    expect(r.status).toBe(0);
    collectPid(dir, listJson(dir)[0].name);
  });

  it("pty tag <longDisplayName> works", () => {
    const dir = makeSessionDir();
    runCli(dir, {}, "run", "-d", "--name", LONG_LABEL, "--", "cat");
    const r = runCli(dir, {}, "tag", LONG_LABEL, "role=worker");
    expect(r.status).toBe(0);
    const s = listJson(dir).find((s: any) => s.displayName === LONG_LABEL)!;
    expect(s.tags.role).toBe("worker");
    collectPid(dir, s.name);
  });

  it("pty events <longDisplayName> --recent works", () => {
    const dir = makeSessionDir();
    runCli(dir, {}, "run", "-d", "--name", LONG_LABEL, "--", "cat");
    const r = runCli(dir, {}, "events", "--recent", LONG_LABEL);
    expect(r.status).toBe(0);
    collectPid(dir, listJson(dir)[0].name);
  });

  it("pty kill <longDisplayName> works", () => {
    const dir = makeSessionDir();
    runCli(dir, {}, "run", "-d", "--name", LONG_LABEL, "--", "cat");
    const r = runCli(dir, {}, "kill", LONG_LABEL);
    expect(r.status).toBe(0);
    const running = listJson(dir).filter((s: any) => s.status === "running");
    expect(running).toHaveLength(0);
  });
});

describe("pty restart preserves displayName + tags", () => {
  // Regression: `pty restart` re-spawned the daemon from stored metadata but
  // dropped `displayName`, so a restarted session read as its raw id (e.g.
  // claude-203827) instead of its name — breaking naming and the TUI peek.
  // Tags were already carried; this locks in both.
  it("keeps displayName and tags across a restart", () => {
    const dir = makeSessionDir();
    const create = runCli(
      dir, {},
      "run", "-d", "--id", "svc", "--name", "My Service", "--tag", "role=web", "--", "cat",
    );
    expect(create.status).toBe(0);

    let s = listJson(dir).find((x: any) => x.name === "svc")!;
    expect(s).toBeDefined();
    expect(s.displayName).toBe("My Service");
    expect(s.tags?.role).toBe("web");
    collectPid(dir, "svc");

    // Restart. Setting PTY_SESSION makes the CLI take the "already inside a
    // session, not attaching" branch and return, instead of hanging on attach
    // in this non-TTY test process.
    const restart = runCli(dir, { PTY_SESSION: "outer" }, "restart", "-y", "svc");
    expect(restart.status).toBe(0);
    expect(restart.stdout).toContain("restarted");

    s = listJson(dir).find((x: any) => x.name === "svc")!;
    expect(s).toBeDefined();
    expect(s.displayName).toBe("My Service");
    expect(s.tags?.role).toBe("web");
    collectPid(dir, "svc");
  });
});
