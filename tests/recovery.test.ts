import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { spawnSync } from "node:child_process";
import { afterEach, describe, expect, it } from "vitest";
import { PacketReader, MessageType, encodeAttach } from "../src/protocol.ts";
import { queryStats } from "../src/client.ts";
import { acquireRecoveryLock, type SessionMetadata } from "../src/sessions.ts";
import {
  RECOVERY_MAX_BYTES,
  readProcessStartToken,
  recoveryRequestPath,
} from "../src/recovery.ts";
import { terminateAndWait } from "./setup/processes.ts";

const projectRoot = path.resolve(import.meta.dirname, "..");
const cli = path.join(projectRoot, "dist", "cli.js");
const roots: string[] = [];
const daemonPids: number[] = [];

function makeRoot(): string {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "pty-recover-"));
  roots.push(root);
  process.env.PTY_ROOT = root;
  return root;
}

function run(root: string, args: string[]) {
  return spawnSync(process.execPath, [cli, "--root", root, ...args], {
    encoding: "utf8",
    timeout: 15_000,
    env: { ...process.env, PTY_SESSION: "", PTY_ROOT_LEGACY_SILENT: "1" },
  });
}

function metadata(root: string, name: string): SessionMetadata {
  return JSON.parse(fs.readFileSync(path.join(root, `${name}.json`), "utf8"));
}

function writeSnapshot(root: string, name: string, value: SessionMetadata): string {
  const file = path.join(root, `${name}.snapshot`);
  fs.writeFileSync(file, JSON.stringify(value));
  return file;
}

function unlinkRegistry(root: string, name: string): void {
  for (const suffix of ["sock", "pid", "json"]) {
    fs.unlinkSync(path.join(root, `${name}.${suffix}`));
  }
}

async function waitFor(check: () => boolean | Promise<boolean>, timeout = 5000): Promise<void> {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    if (await check()) return;
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  throw new Error("timed out waiting for condition");
}

async function attachCollector(socketPath: string) {
  const socket = net.createConnection(socketPath);
  const reader = new PacketReader();
  let output = "";
  socket.on("data", (chunk) => {
    for (const packet of reader.feed(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk))) {
      if (packet.type === MessageType.DATA || packet.type === MessageType.SCREEN) {
        output += packet.payload.toString();
      }
    }
  });
  await new Promise<void>((resolve, reject) => {
    socket.once("connect", resolve);
    socket.once("error", reject);
  });
  socket.write(encodeAttach(24, 80));
  return { socket, output: () => output };
}

function startProvider(root: string, name: string) {
  const marker = path.join(root, `${name}.launches`);
  const script = [
    "const fs=require('fs');",
    "const marker=process.argv[1];",
    "fs.appendFileSync(marker,'launch\\n');",
    "let n=0; setInterval(()=>process.stdout.write(`tick:${++n}\\n`),50);",
  ].join("");
  const started = run(root, [
    "run", "-d", "--id", name, "--no-display-name", "--",
    process.execPath, "-e", script, marker,
  ]);
  expect(started.status, started.stderr || started.stdout).toBe(0);
  const pid = Number(fs.readFileSync(path.join(root, `${name}.pid`), "utf8"));
  daemonPids.push(pid);
  return { marker, pid };
}

afterEach(async () => {
  await terminateAndWait(daemonPids.splice(0));
  delete process.env.PTY_ROOT;
  for (const root of roots.splice(0)) {
    fs.rmSync(root, { recursive: true, force: true, maxRetries: 3, retryDelay: 50 });
  }
});

describe("live daemon registry recovery", () => {
  it("refuses an existing creation lock without any liveness signal", () => {
    const root = makeRoot();
    fs.writeFileSync(path.join(root, "locked.lock"), "2147483647");
    const originalKill = process.kill;
    let calls = 0;
    process.kill = ((..._args: Parameters<typeof process.kill>) => {
      calls++;
      throw new Error("recovery must not signal");
    }) as typeof process.kill;
    try {
      expect(acquireRecoveryLock("locked")).toBe(false);
      expect(calls).toBe(0);
    } finally {
      process.kill = originalKill;
    }
  });

  it("does not advertise a recovery secret from a non-private root", () => {
    const root = makeRoot();
    fs.chmodSync(root, 0o755);
    startProvider(root, "public-root");
    expect(metadata(root, "public-root").recovery).toBeUndefined();
  });

  it("rebinds the original daemon while preserving provider and attached client", async () => {
    const root = makeRoot();
    const name = "positive";
    const { marker, pid } = startProvider(root, name);
    const before = metadata(root, name);
    const snapshot = writeSnapshot(root, name, before);
    const clientA = await attachCollector(path.join(root, `${name}.sock`));
    await waitFor(() => clientA.output().includes("tick:2"));

    unlinkRegistry(root, name);
    const outputBefore = clientA.output().length;
    await waitFor(() => clientA.output().length > outputBefore);
    await expect(queryStats(name)).rejects.toThrow();

    const recovered = run(root, ["recover", name, "--snapshot", snapshot]);
    expect(recovered.status, recovered.stderr || recovered.stdout).toBe(0);
    const after = metadata(root, name);
    const stats = await queryStats(name);
    expect(stats.daemon.pid).toBe(pid);
    expect(after.daemonPid).toBe(pid);
    expect(after.generation).toBe(before.generation);
    expect(after.recovery?.processStartToken).toBe(before.recovery?.processStartToken);
    expect(after.recovery?.launchIdentity).toBe(before.recovery?.launchIdentity);
    expect(after.recovery?.secret).not.toBe(before.recovery?.secret);
    expect(fs.readFileSync(marker, "utf8").trim().split("\n")).toHaveLength(1);
    const starts = fs.readFileSync(path.join(root, `${name}.events.jsonl`), "utf8")
      .split("\n").filter((line) => line.includes('"session_start"'));
    expect(starts).toHaveLength(1);
    const clientB = await attachCollector(path.join(root, `${name}.sock`));
    await waitFor(() => clientB.output().includes("tick:"));
    const preservedAt = clientA.output().length;
    await waitFor(() => clientA.output().length > preservedAt);
    clientA.socket.destroy();
    clientB.socket.destroy();
  }, 20_000);

  it("refuses malformed, unsupported, locked, wrong-root, and tampered requests", async () => {
    const root = makeRoot();
    const name = "tampered";
    const { pid } = startProvider(root, name);
    const before = metadata(root, name);
    unlinkRegistry(root, name);
    const requestPath = recoveryRequestPath(root, name);

    fs.writeFileSync(requestPath, "{not-json");
    await waitFor(() => !fs.existsSync(requestPath));
    fs.writeFileSync(requestPath, "x".repeat(RECOVERY_MAX_BYTES + 1));
    await waitFor(() => !fs.existsSync(requestPath));
    const symlinkTarget = path.join(root, "must-survive");
    fs.writeFileSync(symlinkTarget, "owned elsewhere");
    fs.symlinkSync(symlinkTarget, requestPath);
    await waitFor(() => !fs.existsSync(requestPath));
    expect(fs.readFileSync(symlinkTarget, "utf8")).toBe("owned elsewhere");
    expect(readProcessStartToken(pid)).toBe(before.recovery?.processStartToken);

    const unsupported = { ...before, recovery: undefined };
    expect(run(root, ["recover", name, "--snapshot", writeSnapshot(root, "unsupported", unsupported)]).status)
      .not.toBe(0);

    for (const [label, changed] of [
      ["pid", { ...before, daemonPid: pid + 1 }],
      ["generation", { ...before, generation: "wrong" }],
      ["start", { ...before, recovery: { ...before.recovery!, processStartToken: "wrong" } }],
      ["launch", { ...before, recovery: { ...before.recovery!, launchIdentity: "wrong" } }],
      ["secret", { ...before, recovery: { ...before.recovery!, secret: "00".repeat(32) } }],
    ] as const) {
      const result = run(root, [
        "recover", name, "--snapshot", writeSnapshot(root, label, changed),
      ]);
      expect(result.status, label).not.toBe(0);
      expect(readProcessStartToken(pid)).toBe(before.recovery?.processStartToken);
    }

    fs.writeFileSync(path.join(root, `${name}.lock`), String(process.pid));
    try {
      const locked = run(root, [
        "recover", name, "--snapshot", writeSnapshot(root, "locked", before),
      ]);
      expect(locked.status).not.toBe(0);
      expect(readProcessStartToken(pid)).toBe(before.recovery?.processStartToken);
    } finally {
      fs.unlinkSync(path.join(root, `${name}.lock`));
    }

    const otherRoot = makeRoot();
    const wrongRoot = run(otherRoot, [
      "recover", name, "--snapshot", writeSnapshot(root, "wrong-root", before),
    ]);
    expect(wrongRoot.status).not.toBe(0);
    expect(readProcessStartToken(pid)).toBe(before.recovery?.processStartToken);
  }, 20_000);

  it("never replaces a foreign pathname", async () => {
    const root = makeRoot();
    const name = "foreign";
    startProvider(root, name);
    const snapshot = writeSnapshot(root, name, metadata(root, name));
    unlinkRegistry(root, name);
    const foreign = net.createServer();
    await new Promise<void>((resolve) => foreign.listen(path.join(root, `${name}.sock`), resolve));
    try {
      const result = run(root, ["recover", name, "--snapshot", snapshot]);
      expect(result.status).not.toBe(0);
      expect(fs.lstatSync(path.join(root, `${name}.sock`)).isSocket()).toBe(true);
      const probe = net.createConnection(path.join(root, `${name}.sock`));
      await new Promise<void>((resolve, reject) => {
        probe.once("connect", resolve);
        probe.once("error", reject);
      });
      probe.destroy();
    } finally {
      await new Promise<void>((resolve) => foreign.close(() => resolve()));
    }
  }, 15_000);

  it("rejects replay of the rotated snapshot", () => {
    const root = makeRoot();
    const name = "replay";
    startProvider(root, name);
    const first = metadata(root, name);
    const oldSnapshot = writeSnapshot(root, "old", first);
    unlinkRegistry(root, name);
    expect(run(root, ["recover", name, "--snapshot", oldSnapshot]).status).toBe(0);
    const current = metadata(root, name);
    const currentSnapshot = writeSnapshot(root, "current", current);
    unlinkRegistry(root, name);
    expect(run(root, ["recover", name, "--snapshot", oldSnapshot]).status).not.toBe(0);
    expect(run(root, ["recover", name, "--snapshot", currentSnapshot]).status).toBe(0);
  }, 20_000);
});
