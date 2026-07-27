import { afterAll, describe, expect, it } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { terminateAndWait } from "./setup/processes.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const nodeBin = process.execPath;
const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-restart-launch-"));
const daemonPids = new Map<string, number>();

afterAll(async () => {
  await terminateAndWait(daemonPids.values());
  fs.rmSync(testRoot, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

function runCli(root: string, args: string[], extraEnv: Record<string, string> = {}) {
  return spawnSync(nodeBin, [cliPath, ...args], {
    encoding: "utf8",
    timeout: 15_000,
    env: {
      ...(process.env as Record<string, string>),
      PTY_ROOT: root,
      PTY_ROOT_LEGACY_SILENT: "1",
      PTY_SESSION: "outer-test-session",
      ...extraEnv,
    },
  });
}

function trackPid(root: string, name: string): void {
  try {
    daemonPids.set(
      `${root}\0${name}`,
      Number(fs.readFileSync(path.join(root, `${name}.pid`), "utf8").trim()),
    );
  } catch {}
}

function waitForContent(file: string, timeoutMs = 5_000): string {
  const startedAt = Date.now();
  while (Date.now() - startedAt < timeoutMs) {
    try {
      const value = fs.readFileSync(file, "utf8");
      if (value.length > 0) return value;
    } catch {}
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 50);
  }
  try { return fs.readFileSync(file, "utf8"); } catch { return ""; }
}

function restartShape(metadata: any) {
  return {
    command: metadata.command,
    args: metadata.args,
    displayCommand: metadata.displayCommand,
    cwd: metadata.cwd,
    rows: metadata.rows,
    cols: metadata.cols,
    ephemeral: metadata.ephemeral,
    tags: metadata.tags,
    displayName: metadata.displayName,
    isolateEnv: metadata.isolateEnv,
    extraEnv: metadata.extraEnv,
  };
}

describe("complete persisted launch parity", () => {
  it("persists repeatable --env and every restart-relevant pty run setting", () => {
    const root = fs.mkdtempSync(path.join(testRoot, "run-root-"));
    const cwd = fs.mkdtempSync(path.join(testRoot, "run-cwd-"));
    const output = path.join(testRoot, "run-child.txt");
    const name = "launch-parity";
    const recorder =
      `printf '%s|%s|%s|%s' "$ST_AGENT" "$CATALOG" "$PTY_SESSION" "$PWD" > ${JSON.stringify(output)}; exec sleep 300`;

    const created = runCli(root, [
      "run", "-d", "-e", "--id", name, "--name", "Launch Parity",
      "--tag", "keep=true", "--tag", "role=service",
      "--cwd", cwd, "--isolate-env",
      "--env", "ST_AGENT=managed-first",
      "--env", "CATALOG=/managed/catalog",
      "--env", "PTY_SESSION=must-not-win",
      "--env", "ST_AGENT=managed-final",
      "--", "sh", "-c", recorder,
    ], { ST_AGENT: "ambient-create", CATALOG: "/ambient/create" });
    expect(created.status, created.stderr).toBe(0);
    trackPid(root, name);
    expect(waitForContent(output)).toBe(`managed-final|/managed/catalog|${name}|${cwd}`);

    const before = JSON.parse(fs.readFileSync(path.join(root, `${name}.json`), "utf8"));
    expect(before.ephemeral).toBe(true);
    expect(before.isolateEnv).toBe(true);
    expect(before.extraEnv).toEqual({
      ST_AGENT: "managed-final",
      CATALOG: "/managed/catalog",
      PTY_SESSION: "must-not-win",
    });
    expect(before.tags).toMatchObject({ keep: "true", role: "service" });
    expect(before.displayName).toBe("Launch Parity");
    expect(before.cwd).toBe(cwd);
    expect(before.rows).toBeGreaterThan(0);
    expect(before.cols).toBeGreaterThan(0);

    fs.rmSync(output, { force: true });
    const restarted = runCli(root, ["restart", "-y", name], {
      ST_AGENT: "ambient-restarter",
      CATALOG: "/ambient/restarter",
    });
    expect(restarted.status, restarted.stderr).toBe(0);
    expect(restarted.stdout).toContain("restarted");
    trackPid(root, name);
    expect(waitForContent(output)).toBe(`managed-final|/managed/catalog|${name}|${cwd}`);

    const after = JSON.parse(fs.readFileSync(path.join(root, `${name}.json`), "utf8"));
    expect(restartShape(after)).toEqual(restartShape(before));
  }, 30_000);

  it("keeps every pty.toml session launch field across manual restart", () => {
    const root = fs.mkdtempSync(path.join(testRoot, "toml-root-"));
    const project = fs.mkdtempSync(path.join(testRoot, "toml-project-"));
    const cwd = path.join(project, "work");
    fs.mkdirSync(cwd);
    const output = path.join(testRoot, "toml-child.txt");
    const name = "toml-parity";
    fs.writeFileSync(path.join(project, "pty.toml"), `
prefix = "ignored-by-override"

[sessions.worker]
id = "${name}"
display_name = "TOML Worker"
command = """printf '%s|%s|%s' "$TASK_VALUE" "$PTY_SESSION" "$PWD" > "${output}"; exec sleep 300"""
cwd = "work"

[sessions.worker.tags]
keep = "true"
role = "worker"

[sessions.worker.env]
TASK_VALUE = "from-toml"
PTY_SESSION = "must-not-win"
`);

    const created = runCli(root, ["up", project], { TASK_VALUE: "ambient-create" });
    expect(created.status, created.stderr).toBe(0);
    trackPid(root, name);
    expect(waitForContent(output)).toBe(`from-toml|${name}|${cwd}`);

    const before = JSON.parse(fs.readFileSync(path.join(root, `${name}.json`), "utf8"));
    expect(before.displayName).toBe("TOML Worker");
    expect(before.displayCommand).toContain("TASK_VALUE");
    expect(before.cwd).toBe(cwd);
    expect(before.tags).toMatchObject({
      keep: "true",
      role: "worker",
      "ptyfile.session": "worker",
    });
    expect(before.extraEnv).toEqual({
      TASK_VALUE: "from-toml",
      PTY_SESSION: "must-not-win",
    });

    fs.rmSync(output, { force: true });
    const restarted = runCli(root, ["restart", "-y", name], { TASK_VALUE: "ambient-restarter" });
    expect(restarted.status, restarted.stderr).toBe(0);
    trackPid(root, name);
    expect(waitForContent(output)).toBe(`from-toml|${name}|${cwd}`);
    const after = JSON.parse(fs.readFileSync(path.join(root, `${name}.json`), "utf8"));
    expect(restartShape(after)).toEqual(restartShape(before));
  }, 30_000);
});
