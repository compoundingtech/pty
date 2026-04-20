import { describe, it, expect, afterEach } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, spawnSync } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");

const bgPids: number[] = [];
const cleanupUnits: string[] = [];
const cleanupPaths: string[] = [];

function runCli(env: NodeJS.ProcessEnv, ...args: string[]) {
  return spawnSync(nodeBin, [cliPath, ...args], {
    env,
    encoding: "utf-8",
    timeout: 20000,
  });
}

function waitForFile(filePath: string, timeoutMs = 10000): Promise<void> {
  const start = Date.now();
  return new Promise((resolve, reject) => {
    function check() {
      if (fs.existsSync(filePath)) return resolve();
      if (Date.now() - start > timeoutMs) return reject(new Error(`Timeout waiting for ${filePath}`));
      setTimeout(check, 100);
    }
    check();
  });
}

function killPid(pid: number | undefined) {
  if (!pid) return;
  try { process.kill(pid, "SIGTERM"); } catch {}
}

afterEach(() => {
  for (const unit of cleanupUnits.splice(0)) {
    spawnSync("systemctl", ["--user", "disable", "--now", unit], { encoding: "utf-8" });
    spawnSync("systemctl", ["--user", "reset-failed", unit], { encoding: "utf-8" });
    const unitPath = path.join(os.homedir(), ".config", "systemd", "user", unit);
    try { fs.unlinkSync(unitPath); } catch {}
  }
  if (cleanupUnits.length > 0) {
    spawnSync("systemctl", ["--user", "daemon-reload"], { encoding: "utf-8" });
  } else {
    spawnSync("systemctl", ["--user", "daemon-reload"], { encoding: "utf-8" });
  }

  for (const pid of bgPids.splice(0)) killPid(pid);
  for (const p of cleanupPaths.splice(0)) {
    try { fs.rmSync(p, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 }); } catch {}
  }
});

describe("supervisor service installers", () => {
  it("installs and uninstalls a user systemd service", async () => {
    const sessionDir = fs.mkdtempSync(path.join(os.tmpdir(), "pty-systemd-"));
    cleanupPaths.push(sessionDir);

    const unitBase = `pty-supervisor-test-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`;
    const unitFile = `${unitBase}.service`;
    cleanupUnits.push(unitFile);

    const env = { ...process.env, PTY_SESSION_DIR: sessionDir };
    const install = runCli(env, "supervisor", "systemd", "install", "--name", unitBase);
    expect(install.status).toBe(0);
    expect(install.stdout).toContain(unitFile);

    await waitForFile(path.join(sessionDir, "supervisor", "supervisor.pid"));

    const active = spawnSync("systemctl", ["--user", "is-active", unitFile], {
      encoding: "utf-8",
      timeout: 10000,
    });
    expect(active.status).toBe(0);
    expect(active.stdout.trim()).toBe("active");

    const uninstall = runCli(env, "supervisor", "systemd", "uninstall", "--name", unitBase);
    expect(uninstall.status).toBe(0);

    const activeAfter = spawnSync("systemctl", ["--user", "is-active", unitFile], {
      encoding: "utf-8",
      timeout: 10000,
    });
    expect(activeAfter.status).not.toBe(0);
  }, 30000);

  it("installs a runit service that can be started by a private runsvdir", async () => {
    if (spawnSync("sh", ["-lc", "command -v runsvdir >/dev/null 2>&1"]).status !== 0) {
      throw new Error("runsvdir is not installed");
    }

    const root = fs.mkdtempSync(path.join(os.tmpdir(), "pty-runit-"));
    const sessionDir = path.join(root, "sessions");
    const svDir = path.join(root, "sv");
    const serviceDir = path.join(root, "service");
    fs.mkdirSync(sessionDir, { recursive: true });
    cleanupPaths.push(root);

    const serviceName = `pty-supervisor-test-${Math.random().toString(36).slice(2, 8)}`;
    const env = { ...process.env, PTY_SESSION_DIR: sessionDir };

    const install = runCli(
      env,
      "supervisor", "runit", "install",
      "--name", serviceName,
      "--svdir", svDir,
      "--service-dir", serviceDir,
    );
    expect(install.status).toBe(0);
    expect(fs.existsSync(path.join(svDir, serviceName, "run"))).toBe(true);
    expect(fs.lstatSync(path.join(serviceDir, serviceName)).isSymbolicLink()).toBe(true);

    const child = spawn("runsvdir", [serviceDir], {
      detached: true,
      stdio: ["ignore", "ignore", "ignore"],
      env,
    });
    child.unref();
    bgPids.push(child.pid!);

    await waitForFile(path.join(sessionDir, "supervisor", "supervisor.pid"));

    const uninstall = runCli(
      env,
      "supervisor", "runit", "uninstall",
      "--name", serviceName,
      "--svdir", svDir,
      "--service-dir", serviceDir,
    );
    expect(uninstall.status).toBe(0);
    expect(fs.existsSync(path.join(svDir, serviceName))).toBe(false);
    expect(fs.existsSync(path.join(serviceDir, serviceName))).toBe(false);
  }, 30000);
});
