import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-updc-"));
afterAll(() => {
  fs.rmSync(testRoot, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

const sessionDirs: string[] = [];

function makeProjectDir(): string {
  const dir = fs.mkdtempSync(path.join(testRoot, "p-"));
  return dir;
}

function makeSessionDir(): string {
  const dir = fs.mkdtempSync(path.join(testRoot, "s-"));
  sessionDirs.push(dir);
  return dir;
}

function writePtyToml(dir: string, content: string): void {
  fs.writeFileSync(path.join(dir, "pty.toml"), content);
}

function runCli(sessionDir: string, ...args: string[]): { status: number | null; stdout: string; stderr: string } {
  const r = spawnSync(nodeBin, [cliPath, ...args], {
    cwd: os.tmpdir(),
    env: { ...process.env, PTY_SESSION_DIR: sessionDir },
    encoding: "utf-8",
    timeout: 15000,
  });
  return { status: r.status, stdout: r.stdout, stderr: r.stderr };
}

function listJson(sessionDir: string): any[] {
  const r = runCli(sessionDir, "list", "--json");
  if (!r.stdout.trim()) return [];
  return JSON.parse(r.stdout);
}

afterEach(() => {
  for (const dir of sessionDirs) {
    if (!fs.existsSync(dir)) continue;
    try {
      const entries = fs.readdirSync(dir);
      for (const e of entries) {
        if (e.endsWith(".pid")) {
          const pid = parseInt(fs.readFileSync(path.join(dir, e), "utf-8").trim(), 10);
          if (!isNaN(pid)) { try { process.kill(pid, "SIGTERM"); } catch {} }
        }
      }
      for (const e of entries) { try { fs.unlinkSync(path.join(dir, e)); } catch {} }
    } catch {}
  }
  sessionDirs.length = 0;
});

describe("pty up: on-disk name decoupled from display label", () => {
  it("spawns sessions with a random short id; displayName carries the toml-derived label", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();
    writePtyToml(projDir, `
prefix = "myapp"

[sessions.web]
command = "cat"
`);

    runCli(sessDir, "up", projDir);
    const sessions = listJson(sessDir);
    expect(sessions).toHaveLength(1);
    const s = sessions[0];
    // On-disk name is a short random base32-ish id, NOT "myapp-web".
    expect(s.name).toMatch(/^[a-z0-9]{6,12}$/);
    expect(s.name).not.toBe("myapp-web");
    // Display label is the prefix-shortName combo.
    expect(s.displayName).toBe("myapp-web");
    expect(s.tags["ptyfile.session"]).toBe("web");
  }, 15000);

  it("supports a long prefix that would have exceeded the sock path limit under the old model", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();
    // 90-char prefix + "-web" + ".sock" + session-dir would blow past 104.
    const longPrefix = "p".repeat(90);
    writePtyToml(projDir, `
prefix = "${longPrefix}"

[sessions.web]
command = "cat"
`);

    const result = runCli(sessDir, "up", projDir);
    expect(result.status).toBe(0);
    const sessions = listJson(sessDir);
    expect(sessions).toHaveLength(1);
    expect(sessions[0].displayName).toBe(`${longPrefix}-web`);
    // On-disk name is short.
    expect(sessions[0].name.length).toBeLessThan(20);
  }, 15000);

  it("re-running pty up matches existing sessions by (ptyfile, ptyfile.session) tag pair, not by name", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();
    writePtyToml(projDir, `
[sessions.svc]
command = "cat"
`);

    runCli(sessDir, "up", projDir);
    const firstId = listJson(sessDir).find((s: any) => s.displayName === "svc")!.name;

    // Re-run pty up; should detect already-running and NOT spawn a second.
    const second = runCli(sessDir, "up", projDir);
    expect(second.stdout).toContain("svc (already running)");
    const sessions = listJson(sessDir).filter((s: any) => s.displayName === "svc");
    expect(sessions).toHaveLength(1);
    expect(sessions[0].name).toBe(firstId);
  }, 15000);

  it("honors pty.toml `id = \"...\"` to pin the on-disk identifier", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();
    writePtyToml(projDir, `
[sessions.svc]
id = "pinned"
command = "cat"
`);

    runCli(sessDir, "up", projDir);
    const sessions = listJson(sessDir);
    expect(sessions).toHaveLength(1);
    expect(sessions[0].name).toBe("pinned");
    expect(sessions[0].displayName).toBe("svc");
  }, 15000);

  it("honors pty.toml `display_name = \"...\"` to override the default label", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();
    writePtyToml(projDir, `
prefix = "myapp"

[sessions.web]
display_name = "My Web Server"
command = "cat"
`);

    runCli(sessDir, "up", projDir);
    const sessions = listJson(sessDir);
    expect(sessions).toHaveLength(1);
    expect(sessions[0].displayName).toBe("My Web Server");
    expect(sessions[0].name).toMatch(/^[a-z0-9]{6,12}$/);
  }, 15000);

  it("operations (kill, peek, send) resolve by displayName for toml-spawned sessions", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();
    writePtyToml(projDir, `
[sessions.svc]
command = "cat"
`);

    runCli(sessDir, "up", projDir);
    // Resolve by displayName
    const killResult = runCli(sessDir, "kill", "svc");
    expect(killResult.status).toBe(0);
    // No running sessions left
    const sessions = listJson(sessDir).filter((s: any) => s.status === "running");
    expect(sessions).toHaveLength(0);
  }, 15000);
});
