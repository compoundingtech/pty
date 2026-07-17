import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-updown-"));
afterAll(() => {
  fs.rmSync(testRoot, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

let sessionDirs: string[] = [];

function makeProjectDir(): string {
  const dir = fs.mkdtempSync(path.join(testRoot, "proj-"));
  sessionDirs.push(dir);
  return dir;
}

function makeSessionDir(): string {
  const dir = fs.mkdtempSync(path.join(testRoot, "sd-"));
  sessionDirs.push(dir);
  return dir;
}

function writePtyToml(dir: string, content: string): void {
  fs.writeFileSync(path.join(dir, "pty.toml"), content);
}

function runCli(sessionDir: string, ...args: string[]): { status: number | null; stdout: string; stderr: string } {
  const result = spawnSync(nodeBin, [cliPath, ...args], {
    cwd: os.tmpdir(),
    env: { ...process.env, PTY_SESSION_DIR: sessionDir },
    encoding: "utf-8",
    timeout: 15000,
  });
  return { status: result.status, stdout: result.stdout, stderr: result.stderr };
}

function listJson(sessionDir: string): any[] {
  const result = runCli(sessionDir, "list", "--json");
  return JSON.parse(result.stdout);
}

afterEach(() => {
  // Kill any sessions we may have started
  for (const dir of sessionDirs) {
    if (!fs.existsSync(dir)) continue;
    try {
      const entries = fs.readdirSync(dir);
      for (const e of entries) {
        if (e.endsWith(".pid")) {
          const pidStr = fs.readFileSync(path.join(dir, e), "utf-8").trim();
          const pid = parseInt(pidStr, 10);
          if (!isNaN(pid)) {
            try { process.kill(pid, "SIGTERM"); } catch {}
          }
        }
      }
      // Clean up files
      for (const e of entries) {
        try { fs.unlinkSync(path.join(dir, e)); } catch {}
      }
    } catch {}
  }
  sessionDirs = [];
});

describe("pty up", () => {
  it("starts all sessions from pty.toml", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();
    writePtyToml(projDir, `
[sessions.one]
command = "cat"

[sessions.two]
command = "cat"
`);

    const result = runCli(sessDir, "up", projDir);

    expect(result.status).toBe(0);
    expect(result.stdout).toContain("one (started)");
    expect(result.stdout).toContain("two (started)");
    expect(result.stdout).toContain("Started 2 sessions");

    const sessions = listJson(sessDir);
    expect(sessions.filter((s: any) => s.status === "running")).toHaveLength(2);
  }, 15000);

  it("starts only named sessions", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();
    writePtyToml(projDir, `
[sessions.web]
command = "cat"

[sessions.worker]
command = "cat"

[sessions.db]
command = "cat"
`);

    const result = runCli(sessDir, "up", projDir, "web", "db");

    expect(result.status).toBe(0);
    expect(result.stdout).toContain("web (started)");
    expect(result.stdout).toContain("db (started)");
    expect(result.stdout).not.toContain("worker");

    const sessions = listJson(sessDir);
    const running = sessions.filter((s: any) => s.status === "running");
    expect(running).toHaveLength(2);
    expect(running.map((s: any) => s.displayName).sort()).toEqual(["db", "web"]);
  }, 15000);

  it("propagates env from pty.toml into the spawned session", async () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();
    const outFile = path.join(projDir, "envcheck.out");
    writePtyToml(projDir, `
[sessions.envprobe]
command = "echo \\"$MY_VAR|$ANOTHER\\" > '${outFile}'; cat"

[sessions.envprobe.env]
MY_VAR = "hello"
ANOTHER = "world"
`);

    const result = runCli(sessDir, "up", projDir);
    expect(result.status).toBe(0);
    expect(result.stdout).toContain("envprobe (started)");

    // `cat` keeps the session alive after the redirect; wait for the file.
    const deadline = Date.now() + 3000;
    while (Date.now() < deadline && !fs.existsSync(outFile)) {
      await new Promise((r) => setTimeout(r, 50));
    }
    expect(fs.existsSync(outFile)).toBe(true);
    expect(fs.readFileSync(outFile, "utf-8").trim()).toBe("hello|world");
  }, 15000);

  it("skips already running sessions", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();
    writePtyToml(projDir, `
[sessions.mycat]
command = "cat"
`);

    // Start once
    runCli(sessDir, "up", projDir);

    // Start again
    const result = runCli(sessDir, "up", projDir);
    expect(result.stdout).toContain("mycat (already running)");
    expect(result.stdout).toContain("All sessions already running");
  }, 15000);

  it("syncs tags to already-running sessions on pty up", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();

    // Start with no tags
    writePtyToml(projDir, `
[sessions.syncme]
command = "cat"
`);
    runCli(sessDir, "up", projDir);

    // Update toml to add tags
    writePtyToml(projDir, `
[sessions.syncme]
command = "cat"
tags = { strategy = "permanent", role = "server" }
`);

    const result = runCli(sessDir, "up", projDir);
    expect(result.stdout).toContain("updated tags: strategy=permanent, role=server");

    // Verify tags were applied
    const sessions = listJson(sessDir);
    const session = sessions.find((s: any) => s.displayName === "syncme");
    expect(session.tags.strategy).toBe("permanent");
    expect(session.tags.role).toBe("server");
    expect(session.tags.ptyfile).toBeDefined();
  }, 15000);

  it("does not remove manually-added tags on pty up", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();
    writePtyToml(projDir, `
[sessions.manual]
command = "cat"
tags = { role = "server" }
`);

    runCli(sessDir, "up", projDir);

    // Manually add an extra tag
    runCli(sessDir, "tag", "manual", "custom=yes");

    // Run pty up again — should NOT remove the custom tag
    runCli(sessDir, "up", projDir);

    const sessions = listJson(sessDir);
    const session = sessions.find((s: any) => s.displayName === "manual");
    expect(session.tags.role).toBe("server");
    expect(session.tags.custom).toBe("yes");
  }, 15000);

  it("removes tags that were removed from pty.toml", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();

    writePtyToml(projDir, `
[sessions.remover]
command = "cat"
tags = { role = "server", env = "dev" }
`);
    runCli(sessDir, "up", projDir);

    let sessions = listJson(sessDir);
    let session = sessions.find((s: any) => s.displayName === "remover");
    expect(session.tags.role).toBe("server");
    expect(session.tags.env).toBe("dev");

    // Remove env from the toml
    writePtyToml(projDir, `
[sessions.remover]
command = "cat"
tags = { role = "server" }
`);
    const result = runCli(sessDir, "up", projDir);
    expect(result.stdout).toContain("-env");

    sessions = listJson(sessDir);
    session = sessions.find((s: any) => s.displayName === "remover");
    expect(session.tags.role).toBe("server");
    expect(session.tags.env).toBeUndefined();
    // Metadata tags should still be present
    expect(session.tags.ptyfile).toBeDefined();
    expect(session.tags["ptyfile.session"]).toBe("remover");
  }, 20000);

  it("removes all toml tags when the tags table is deleted", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();

    writePtyToml(projDir, `
[sessions.cleared]
command = "cat"
tags = { role = "server", env = "dev" }
`);
    runCli(sessDir, "up", projDir);

    // Remove the tags table entirely
    writePtyToml(projDir, `
[sessions.cleared]
command = "cat"
`);
    const result = runCli(sessDir, "up", projDir);
    expect(result.stdout).toContain("-env");
    expect(result.stdout).toContain("-role");

    const sessions = listJson(sessDir);
    const session = sessions.find((s: any) => s.displayName === "cleared");
    expect(session.tags.role).toBeUndefined();
    expect(session.tags.env).toBeUndefined();
    expect(session.tags.ptyfile).toBeDefined();
  }, 20000);

  it("preserves manually-added tags when toml tags are removed", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();

    writePtyToml(projDir, `
[sessions.mixer]
command = "cat"
tags = { role = "server" }
`);
    runCli(sessDir, "up", projDir);

    // Add a manual tag
    runCli(sessDir, "tag", "mixer", "custom=yes");

    // Remove the toml tag
    writePtyToml(projDir, `
[sessions.mixer]
command = "cat"
`);
    runCli(sessDir, "up", projDir);

    const sessions = listJson(sessDir);
    const session = sessions.find((s: any) => s.displayName === "mixer");
    expect(session.tags.role).toBeUndefined();
    expect(session.tags.custom).toBe("yes");
  }, 20000);

  it("replaces a toml tag's value (not remove+re-add)", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();

    writePtyToml(projDir, `
[sessions.mover]
command = "cat"
tags = { env = "dev" }
`);
    runCli(sessDir, "up", projDir);

    writePtyToml(projDir, `
[sessions.mover]
command = "cat"
tags = { env = "prod" }
`);
    const result = runCli(sessDir, "up", projDir);
    expect(result.stdout).toContain("env=prod");
    expect(result.stdout).not.toContain("-env");

    const sessions = listJson(sessDir);
    const session = sessions.find((s: any) => s.displayName === "mover");
    expect(session.tags.env).toBe("prod");
  }, 20000);

  it("no output for already-running sessions with matching tags", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();
    writePtyToml(projDir, `
[sessions.unchanged]
command = "cat"
tags = { role = "server" }
`);

    runCli(sessDir, "up", projDir);

    // Run again — tags match, no update message
    const result = runCli(sessDir, "up", projDir);
    expect(result.stdout).toContain("unchanged (already running)");
    expect(result.stdout).not.toContain("updated tags");
  }, 15000);

  it("propagates tags from pty.toml", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();
    writePtyToml(projDir, `
[sessions.tagged]
command = "cat"
tags = { role = "server", env = "dev" }
`);

    runCli(sessDir, "up", projDir);

    const sessions = listJson(sessDir);
    const session = sessions.find((s: any) => s.displayName === "tagged");
    expect(session).toBeDefined();
    expect(session.tags.role).toBe("server");
    expect(session.tags.env).toBe("dev");
    expect(session.tags.ptyfile).toBeDefined();
  }, 15000);

  it("sets cwd to the project directory", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();
    writePtyToml(projDir, `
[sessions.checkdir]
command = "cat"
`);

    runCli(sessDir, "up", projDir);

    const sessions = listJson(sessDir);
    const session = sessions.find((s: any) => s.displayName === "checkdir");
    expect(session).toBeDefined();
    expect(session.cwd).toBe(projDir);
  }, 15000);

  it("honors an explicit absolute cwd field (runs there, not in the manifest dir)", () => {
    const projDir = makeProjectDir();
    const runDir = makeProjectDir(); // where the session should actually run
    const sessDir = makeSessionDir();
    writePtyToml(projDir, `
[sessions.elsewhere]
command = "cat"
cwd = "${runDir}"
`);

    runCli(sessDir, "up", projDir);

    const session = listJson(sessDir).find((s: any) => s.displayName === "elsewhere");
    expect(session).toBeDefined();
    expect(session.cwd).toBe(runDir);      // explicit cwd wins
    expect(session.cwd).not.toBe(projDir); // NOT the manifest dir
  }, 15000);

  it("resolves a relative cwd against the manifest dir (the .convoy/pty.toml case)", () => {
    // Manifest kept in <project>/.convoy/pty.toml; the session should run in the
    // project root via cwd = "..", NOT in .convoy/. This is the convoy layout.
    const projDir = makeProjectDir();
    const convoyDir = path.join(projDir, ".convoy");
    fs.mkdirSync(convoyDir, { recursive: true });
    fs.writeFileSync(path.join(convoyDir, "pty.toml"), `
[sessions.rooted]
command = "cat"
cwd = ".."
`);
    const sessDir = makeSessionDir();

    runCli(sessDir, "up", convoyDir);

    const session = listJson(sessDir).find((s: any) => s.displayName === "rooted");
    expect(session).toBeDefined();
    expect(session.cwd).toBe(projDir);       // resolved to the repo root
    expect(session.cwd).not.toBe(convoyDir); // NOT the manifest (.convoy) dir
  }, 15000);

  it("uses prefix for session names", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();
    writePtyToml(projDir, `
prefix = "myapp"

[sessions.web]
command = "cat"

[sessions.worker]
command = "cat"
`);

    const result = runCli(sessDir, "up", projDir);

    expect(result.status).toBe(0);
    expect(result.stdout).toContain("myapp-web (started)");
    expect(result.stdout).toContain("myapp-worker (started)");

    const sessions = listJson(sessDir);
    const names = sessions.filter((s: any) => s.status === "running").map((s: any) => s.displayName);
    expect(names.sort()).toEqual(["myapp-web", "myapp-worker"]);
  }, 15000);

  it("filters by short name when prefix is set", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();
    writePtyToml(projDir, `
prefix = "myapp"

[sessions.web]
command = "cat"

[sessions.worker]
command = "cat"
`);

    const result = runCli(sessDir, "up", projDir, "web");

    expect(result.status).toBe(0);
    expect(result.stdout).toContain("myapp-web (started)");
    expect(result.stdout).not.toContain("worker");

    const sessions = listJson(sessDir);
    const running = sessions.filter((s: any) => s.status === "running");
    expect(running).toHaveLength(1);
    expect(running[0].displayName).toBe("myapp-web");
  }, 15000);

  it("sets ptyfile tags on created sessions", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();
    writePtyToml(projDir, `
[sessions.tracked]
command = "cat"
`);

    runCli(sessDir, "up", projDir);

    const sessions = listJson(sessDir);
    const session = sessions.find((s: any) => s.displayName === "tracked");
    expect(session).toBeDefined();
    expect(session.tags.ptyfile).toBe(projDir + "/pty.toml");
    expect(session.tags["ptyfile.session"]).toBe("tracked");
  }, 15000);

  it("errors on unknown session name", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();
    writePtyToml(projDir, `
[sessions.real]
command = "cat"
`);

    const result = runCli(sessDir, "up", projDir, "fake");
    expect(result.status).not.toBe(0);
    expect(result.stderr).toContain("Unknown session: fake");
    expect(result.stderr).toContain("Available: real");
  }, 15000);

  it("errors when no pty.toml exists", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();

    const result = runCli(sessDir, "up", projDir);
    expect(result.status).not.toBe(0);
    expect(result.stderr).toContain("No pty.toml found");
  }, 15000);

  it("errors on pty.toml with no sessions", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();
    writePtyToml(projDir, `# empty config\n`);

    const result = runCli(sessDir, "up", projDir);
    expect(result.status).not.toBe(0);
    expect(result.stderr).toContain("No sessions defined");
  }, 15000);

  it("errors on session without a command", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();
    writePtyToml(projDir, `
[sessions.bad]
tags = { foo = "bar" }
`);

    const result = runCli(sessDir, "up", projDir);
    expect(result.status).not.toBe(0);
    expect(result.stderr).toContain('missing a "command" field');
  }, 15000);
});

describe("pty down", () => {
  it("stops all running sessions from pty.toml", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();
    writePtyToml(projDir, `
[sessions.alpha]
command = "cat"

[sessions.beta]
command = "cat"
`);

    // Start them
    runCli(sessDir, "up", projDir);
    let sessions = listJson(sessDir);
    expect(sessions.filter((s: any) => s.status === "running")).toHaveLength(2);

    // Stop them
    const result = runCli(sessDir, "down", projDir);
    expect(result.status).toBe(0);
    expect(result.stdout).toContain("alpha (stopped)");
    expect(result.stdout).toContain("beta (stopped)");
    expect(result.stdout).toContain("Stopped 2 sessions");
  }, 15000);

  it("stops only named sessions", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();
    writePtyToml(projDir, `
[sessions.keep]
command = "cat"

[sessions.stop]
command = "cat"
`);

    runCli(sessDir, "up", projDir);

    const result = runCli(sessDir, "down", projDir, "stop");
    expect(result.stdout).toContain("stop (stopped)");
    expect(result.stdout).not.toContain("keep");

    // "keep" should still be running
    const sessions = listJson(sessDir);
    const running = sessions.filter((s: any) => s.status === "running");
    expect(running).toHaveLength(1);
    expect(running[0].displayName).toBe("keep");
  }, 15000);

  it("reports when nothing to stop", () => {
    const projDir = makeProjectDir();
    const sessDir = makeSessionDir();
    writePtyToml(projDir, `
[sessions.ghost]
command = "cat"
`);

    const result = runCli(sessDir, "down", projDir);
    expect(result.stdout).toContain("No sessions to stop");
  }, 15000);
});
