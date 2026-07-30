import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { spawn, spawnSync } from "node:child_process";
import { pathToFileURL } from "node:url";
import { afterEach, describe, expect, it } from "vitest";
import { PacketReader, MessageType, encodeAttach } from "../src/protocol.ts";
import { queryStats } from "../src/client.ts";
import {
  acquireRecoveryLock,
  writeMetadata,
  type SessionMetadata,
} from "../src/sessions.ts";
import {
  RECOVERY_PROTOCOL,
  RECOVERY_MAX_BYTES,
  metadataRevision,
  readProcessStartToken,
  readBoundedJson,
  recoveryLockContents,
  recoveryLockIdentity,
  recoveryRevisionPath,
  recoveryResultPath,
  recoveryRequestPath,
  verifyRecoveryRevision,
  type RecoveryRevision,
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
      expect(acquireRecoveryLock("locked", "recovery-owner")).toBe(false);
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

  it("advances the signed revision before publishing mutated metadata", () => {
    const root = makeRoot();
    const name = "ordered-revision";
    const recoveryDir = path.join(root, ".recovery");
    fs.mkdirSync(recoveryDir, { mode: 0o700 });
    const rootStat = fs.lstatSync(root);
    const recoveryStat = fs.lstatSync(recoveryDir);
    const initial: SessionMetadata = {
      generation: "ordered-generation",
      daemonPid: process.pid,
      recovery: {
        protocol: RECOVERY_PROTOCOL,
        secret: "11".repeat(32),
        processStartToken: "test-start",
        launchIdentity: "22".repeat(32),
        rootDevice: rootStat.dev,
        rootInode: rootStat.ino,
        recoveryDirDevice: recoveryStat.dev,
        recoveryDirInode: recoveryStat.ino,
        metadataRevision: "",
      },
      command: "/bin/sh",
      args: [],
      displayCommand: "sh",
      cwd: root,
      createdAt: new Date().toISOString(),
    };
    writeMetadata(name, initial);
    const before = metadata(root, name);
    const mutated: SessionMetadata = {
      ...before,
      tags: { role: "current", strategy: "permanent" },
      displayName: "Current Display",
      lastAttachAt: new Date().toISOString(),
    };
    let seamObserved = false;

    writeMetadata(name, mutated, {
      afterRecoveryRevisionPublished: () => {
        seamObserved = true;
        // The old metadata is still the only visible snapshot at this exact
        // seam, but its revision has already stopped being authoritative.
        expect(metadata(root, name)).toEqual(before);
        const revision = readBoundedJson<RecoveryRevision>(
          recoveryRevisionPath(root, name),
        );
        expect(verifyRecoveryRevision(initial.recovery!.secret, revision)).toBe(true);
        expect(revision.metadataRevision).toBe(metadataRevision(mutated));
        expect(revision.metadataRevision).not.toBe(before.recovery!.metadataRevision);
      },
    });

    expect(seamObserved).toBe(true);
    expect(metadata(root, name).tags).toEqual(mutated.tags);
    expect(metadata(root, name).displayName).toBe(mutated.displayName);
    expect(metadata(root, name).lastAttachAt).toBe(mutated.lastAttachAt);
  });

  it("fails closed when metadata publication stops after revision advancement", () => {
    const root = makeRoot();
    const name = "interrupted-revision";
    startProvider(root, name);
    const before = metadata(root, name);
    const snapshot = writeSnapshot(root, name, before);
    const mutated: SessionMetadata = {
      ...before,
      tags: { role: "must-not-publish" },
    };

    expect(() => writeMetadata(name, mutated, {
      afterRecoveryRevisionPublished: () => {
        throw new Error("stop before metadata publication");
      },
    })).toThrow("stop before metadata publication");

    // Publication stopped in the exact revision-before-metadata window: the
    // old metadata remains visible, but its signed revision is no longer
    // authoritative.
    expect(metadata(root, name)).toEqual(before);
    const advanced = readBoundedJson<RecoveryRevision>(
      recoveryRevisionPath(root, name),
    );
    expect(verifyRecoveryRevision(before.recovery!.secret, advanced)).toBe(true);
    expect(advanced.metadataRevision).toBe(metadataRevision(mutated));
    expect(advanced.metadataRevision).not.toBe(before.recovery!.metadataRevision);

    unlinkRegistry(root, name);
    for (let attempt = 0; attempt < 2; attempt++) {
      const refused = run(root, ["recover", name, "--snapshot", snapshot]);
      expect(refused.status).not.toBe(0);
      expect(fs.existsSync(path.join(root, `${name}.sock`))).toBe(false);
      expect(fs.existsSync(path.join(root, `${name}.pid`))).toBe(false);
      expect(fs.existsSync(path.join(root, `${name}.json`))).toBe(false);
      expect(readBoundedJson<RecoveryRevision>(
        recoveryRevisionPath(root, name),
      )).toEqual(advanced);
    }
  }, 20_000);

  it("rebinds the original daemon while preserving provider and attached client", async () => {
    const root = makeRoot();
    const name = "positive";
    const { marker, pid } = startProvider(root, name);
    const clientA = await attachCollector(path.join(root, `${name}.sock`));
    await waitFor(() => clientA.output().includes("tick:2"));
    const before = metadata(root, name);
    const snapshot = writeSnapshot(root, name, before);

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
    expect(after.recovery?.metadataRevision).not.toBe(before.recovery?.metadataRevision);
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

  it("refuses a stale metadata snapshot instead of rolling back live mutations", async () => {
    const root = makeRoot();
    const name = "stale-metadata";
    startProvider(root, name);
    const stale = metadata(root, name);
    const staleSnapshot = writeSnapshot(root, "stale", stale);

    expect(run(root, ["tag", name, "role=current", "strategy=permanent"]).status).toBe(0);
    expect(run(root, ["rename", name, "Current Display"]).status).toBe(0);
    const client = await attachCollector(path.join(root, `${name}.sock`));
    await waitFor(() => metadata(root, name).lastAttachAt !== undefined);
    const current = metadata(root, name);
    const currentSnapshot = writeSnapshot(root, "current", current);
    expect(current.recovery?.metadataRevision).not.toBe(stale.recovery?.metadataRevision);

    unlinkRegistry(root, name);
    const refused = run(root, ["recover", name, "--snapshot", staleSnapshot]);
    expect(refused.status).not.toBe(0);
    expect(fs.existsSync(path.join(root, `${name}.json`))).toBe(false);

    const recovered = run(root, ["recover", name, "--snapshot", currentSnapshot]);
    expect(recovered.status, recovered.stderr || recovered.stdout).toBe(0);
    const after = metadata(root, name);
    expect(after.tags).toEqual(current.tags);
    expect(after.displayName).toBe(current.displayName);
    expect(after.lastAttachAt).toBe(current.lastAttachAt);
    client.socket.destroy();
  }, 20_000);

  it("resumes an authenticated lock after its recoverer is killed", async () => {
    const root = makeRoot();
    const name = "interrupted-lock";
    const { marker, pid } = startProvider(root, name);
    const client = await attachCollector(path.join(root, `${name}.sock`));
    await waitFor(() => client.output().includes("tick:2"));
    const before = metadata(root, name);
    const snapshot = writeSnapshot(root, name, before);
    unlinkRegistry(root, name);

    const capability = before.recovery!;
    const identity = recoveryLockIdentity({
      name,
      daemonPid: pid,
      processStartToken: capability.processStartToken,
      rootDevice: capability.rootDevice,
      rootInode: capability.rootInode,
      recoveryDirDevice: capability.recoveryDirDevice,
      recoveryDirInode: capability.recoveryDirInode,
    });
    const contents = recoveryLockContents(pid, identity);
    const sessionsModule = pathToFileURL(path.join(projectRoot, "dist", "sessions.js")).href;
    const lockHolder = spawn(process.execPath, [
      "-e",
      "import(process.argv[1]).then(m=>{process.env.PTY_ROOT=process.argv[2];if(!m.acquireRecoveryLock(process.argv[3],process.argv[4]))process.exit(2);process.stdout.write('locked\\n');setInterval(()=>{},1000)})",
      sessionsModule,
      root,
      name,
      contents,
    ], { stdio: ["ignore", "pipe", "pipe"] });
    await new Promise<void>((resolve, reject) => {
      lockHolder.stdout!.once("data", () => resolve());
      lockHolder.once("error", reject);
      lockHolder.once("exit", (code) => {
        if (code !== null) reject(new Error(`lock holder exited ${code}`));
      });
    });
    lockHolder.kill("SIGKILL");
    await new Promise<void>((resolve) => lockHolder.once("exit", () => resolve()));
    expect(fs.readFileSync(path.join(root, `${name}.lock`), "utf8")).toBe(contents);

    const outputBefore = client.output().length;
    const recovered = run(root, ["recover", name, "--snapshot", snapshot]);
    expect(recovered.status, recovered.stderr || recovered.stdout).toBe(0);
    await waitFor(() => client.output().length > outputBefore);
    expect((await queryStats(name)).daemon.pid).toBe(pid);
    expect(fs.readFileSync(marker, "utf8").trim().split("\n")).toHaveLength(1);
    expect(fs.existsSync(path.join(root, `${name}.lock`))).toBe(false);
    client.socket.destroy();
  }, 20_000);

  it("refuses permission downgrades before writing secret-bearing recovery state", () => {
    for (const downgraded of ["root", "recovery-dir"] as const) {
      const root = makeRoot();
      const name = `privacy-${downgraded}`;
      startProvider(root, name);
      const snapshot = writeSnapshot(root, name, metadata(root, name));
      unlinkRegistry(root, name);
      const recoveryDir = path.join(root, ".recovery");
      const before = fs.readdirSync(recoveryDir).sort();
      fs.chmodSync(downgraded === "root" ? root : recoveryDir, 0o755);
      try {
        const refused = run(root, ["recover", name, "--snapshot", snapshot]);
        expect(refused.status).not.toBe(0);
        expect(fs.readdirSync(recoveryDir).sort()).toEqual(before);
        expect(fs.existsSync(recoveryRequestPath(root, name))).toBe(false);
        expect(fs.existsSync(recoveryResultPath(root, name))).toBe(false);
        expect(fs.existsSync(path.join(root, `${name}.lock`))).toBe(false);
        expect(fs.existsSync(path.join(root, `${name}.sock`))).toBe(false);
        expect(fs.existsSync(path.join(root, `${name}.pid`))).toBe(false);
        expect(fs.existsSync(path.join(root, `${name}.json`))).toBe(false);
      } finally {
        fs.chmodSync(downgraded === "root" ? root : recoveryDir, 0o700);
      }
    }
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
