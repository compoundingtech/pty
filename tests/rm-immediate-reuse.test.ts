import { afterAll, describe, expect, it } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import {
  cleanupOwnedAll,
  cleanupOwnedSocket,
} from "../src/sessions.ts";
import { terminateAndWait } from "./setup/processes.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-rm-reuse-"));
const liveDaemons = new Set<number>();

afterAll(async () => {
  await terminateAndWait(liveDaemons);
  fs.rmSync(testRoot, {
    recursive: true,
    force: true,
    maxRetries: 3,
    retryDelay: 100,
  });
});

function runCli(dir: string, args: string[]) {
  return spawnSync(nodeBin, [cliPath, ...args], {
    env: {
      ...process.env,
      PTY_ROOT: dir,
      PTY_SESSION_DIR: "",
      PTY_ROOT_LEGACY_SILENT: "1",
    },
    encoding: "utf8",
    timeout: 12_000,
  });
}

function metadata(dir: string, name: string): Record<string, unknown> {
  return JSON.parse(fs.readFileSync(path.join(dir, `${name}.json`), "utf8"));
}

function isAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

async function waitForExitMetadata(
  dir: string,
  name: string,
): Promise<Record<string, unknown>> {
  const deadline = Date.now() + 5_000;
  while (Date.now() < deadline) {
    try {
      const value = metadata(dir, name);
      if (typeof value.exitedAt === "string") return value;
    } catch {}
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error(`timed out waiting for ${name} exit metadata`);
}

describe("pty rm immediate same-name reuse", () => {
  it("stale generation cleanup cannot unlink replacement files", () => {
    const dir = fs.mkdtempSync(path.join(testRoot, "owned-"));
    const previousRoot = process.env.PTY_ROOT;
    const previousLegacyRoot = process.env.PTY_SESSION_DIR;
    process.env.PTY_ROOT = dir;
    delete process.env.PTY_SESSION_DIR;

    try {
      const name = "owned";
      const replacementPid = process.pid;
      fs.writeFileSync(path.join(dir, `${name}.sock`), "replacement socket");
      fs.writeFileSync(path.join(dir, `${name}.pid`), String(replacementPid));
      fs.writeFileSync(path.join(dir, `${name}.events.jsonl`), "replacement event\n");
      fs.writeFileSync(path.join(dir, `${name}.json`), JSON.stringify({
        generation: "new-generation",
        daemonPid: replacementPid,
        command: "cat",
        args: [],
        displayCommand: "cat",
        cwd: dir,
        createdAt: new Date().toISOString(),
      }));

      const oldOwner = { generation: "old-generation", pid: replacementPid - 1 };
      expect(cleanupOwnedSocket(name, oldOwner)).toBe(false);
      expect(cleanupOwnedAll(name, oldOwner)).toBe(false);

      expect(fs.readFileSync(path.join(dir, `${name}.sock`), "utf8"))
        .toBe("replacement socket");
      expect(fs.readFileSync(path.join(dir, `${name}.pid`), "utf8"))
        .toBe(String(replacementPid));
      expect(fs.existsSync(path.join(dir, `${name}.json`))).toBe(true);
      expect(fs.existsSync(path.join(dir, `${name}.events.jsonl`))).toBe(true);
    } finally {
      if (previousRoot === undefined) delete process.env.PTY_ROOT;
      else process.env.PTY_ROOT = previousRoot;
      if (previousLegacyRoot === undefined) delete process.env.PTY_SESSION_DIR;
      else process.env.PTY_SESSION_DIR = previousLegacyRoot;
    }
  });

  it("repeatedly waits out the old generation before permitting replacement", async () => {
    const dir = fs.mkdtempSync(path.join(testRoot, "d-"));
    const name = "reuse";

    // Repeat the formerly deterministic 500ms race enough times to ensure
    // success is a lifecycle contract rather than favorable scheduling.
    for (let iteration = 0; iteration < 5; iteration++) {
      const first = runCli(dir, [
        "run", "-d", "--id", name, "--tag", "keep=true", "--",
        "sh", "-c", "sleep 0.05; exit 0",
      ]);
      expect(first.status, first.stderr).toBe(0);

      const oldMetadata = await waitForExitMetadata(dir, name);
      const oldPid = Number(oldMetadata.daemonPid);
      const oldGeneration = String(oldMetadata.generation);
      expect(oldPid).toBeGreaterThan(0);
      expect(oldGeneration.length).toBeGreaterThan(0);
      expect(isAlive(oldPid)).toBe(true);
      liveDaemons.add(oldPid);

      const removed = runCli(dir, ["rm", name]);
      expect(removed.status, removed.stderr).toBe(0);
      expect(removed.stdout).toContain("removed");
      expect(isAlive(oldPid)).toBe(false);
      liveDaemons.delete(oldPid);

      const replacement = runCli(dir, [
        "run", "-d", "--id", name, "--", "cat",
      ]);
      expect(replacement.status, replacement.stderr).toBe(0);

      const replacementMetadata = metadata(dir, name);
      const replacementPid = Number(replacementMetadata.daemonPid);
      liveDaemons.add(replacementPid);
      expect(replacementMetadata.generation).not.toBe(oldGeneration);

      // Cross the old daemon's former 500ms deferred-cleanup window. Its
      // cleanup must not remove any file belonging to the replacement.
      await new Promise((resolve) => setTimeout(resolve, 650));
      expect(fs.existsSync(path.join(dir, `${name}.sock`))).toBe(true);
      expect(
        Number(fs.readFileSync(path.join(dir, `${name}.pid`), "utf8").trim()),
      ).toBe(replacementPid);
      expect(metadata(dir, name).generation).toBe(replacementMetadata.generation);
      expect(isAlive(replacementPid)).toBe(true);

      expect(runCli(dir, ["kill", name]).status).toBe(0);
      liveDaemons.delete(replacementPid);
      expect(runCli(dir, ["rm", name]).status).toBe(0);
    }
  }, 60_000);
});
