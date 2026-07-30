import { afterAll, afterEach, describe, expect, it } from "vitest";
import { spawn, spawnSync } from "node:child_process";
import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { terminateAndWait } from "./setup/processes.ts";
import { encodeAttach, encodeData } from "../src/protocol.ts";
import { PtyServer } from "../src/server.ts";
import {
  cleanupSocket,
  getRecoveryRequestPath,
  readLiveRecoveryRequest,
  removeLiveRecoveryRequest,
} from "../src/sessions.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const serverPath = path.join(__dirname, "..", "dist", "server.js");
const testBase = fs.mkdtempSync(path.join(os.tmpdir(), "pty-live-recovery-"));

let daemonPids: number[] = [];
let clients: net.Socket[] = [];
let foreignServers: net.Server[] = [];

afterEach(async () => {
  for (const client of clients) client.destroy();
  clients = [];
  for (const server of foreignServers) {
    await new Promise<void>((resolve) => server.close(() => resolve()));
  }
  foreignServers = [];
  await terminateAndWait(daemonPids);
  daemonPids = [];
});

afterAll(() => {
  fs.rmSync(testBase, {
    recursive: true,
    force: true,
    maxRetries: 3,
    retryDelay: 100,
  });
});

async function waitForPath(target: string, timeoutMs = 5000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (fs.existsSync(target)) return;
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  throw new Error(`Timed out waiting for ${target}`);
}

async function startDaemon(name: string): Promise<{
  root: string;
  pid: number;
  providerPid: number;
  snapshotPath: string;
  socketPath: string;
}> {
  const root = fs.mkdtempSync(path.join(testBase, "root-"));
  const providerPidPath = path.join(testBase, `${name}-provider.pid`);
  const config = JSON.stringify({
    name,
    command: "/bin/sh",
    args: [
      "-c",
      `printf '%s\\n' "$$" > '${providerPidPath}'; ` +
        "printf 'PROVIDER-READY\\n'; " +
        "while IFS= read -r line; do printf 'PROVIDER-ACK:%s\\n' \"$line\"; done",
    ],
    displayCommand: "synthetic-live-provider",
    cwd: os.tmpdir(),
    rows: 24,
    cols: 80,
    tags: { keep: "true" },
  });
  const child = spawn(nodeBin, [serverPath], {
    detached: true,
    stdio: ["ignore", "ignore", "inherit"],
    env: {
      ...process.env,
      PTY_ROOT: root,
      PTY_SESSION_DIR: "",
      PTY_SERVER_CONFIG: config,
    },
  });
  child.unref();
  if (!child.pid) throw new Error("daemon did not receive a PID");
  daemonPids.push(child.pid);

  const socketPath = path.join(root, `${name}.sock`);
  await waitForPath(socketPath);
  await waitForPath(path.join(root, `${name}.json`));
  await waitForPath(providerPidPath);
  const snapshotPath = path.join(testBase, `${name}-${child.pid}.snapshot`);
  fs.copyFileSync(path.join(root, `${name}.json`), snapshotPath);
  return {
    root,
    pid: child.pid,
    providerPid: Number(fs.readFileSync(providerPidPath, "utf-8")),
    snapshotPath,
    socketPath,
  };
}

function runRecover(
  root: string,
  name: string,
  snapshotPath: string,
  timeoutMs = 2000,
) {
  return spawnSync(
    nodeBin,
    [
      cliPath,
      "recover-live",
      "--metadata",
      snapshotPath,
      "--timeout-ms",
      String(timeoutMs),
      name,
    ],
    {
      encoding: "utf-8",
      timeout: timeoutMs + 3000,
      env: {
        ...process.env,
        PTY_ROOT: root,
        PTY_SESSION_DIR: "",
        PTY_SESSION: "",
      },
    },
  );
}

function connect(socketPath: string): Promise<net.Socket> {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection(socketPath);
    socket.once("connect", () => resolve(socket));
    socket.once("error", reject);
  });
}

function unlinkRegistry(root: string, name: string): void {
  fs.unlinkSync(path.join(root, `${name}.sock`));
  fs.unlinkSync(path.join(root, `${name}.pid`));
  fs.unlinkSync(path.join(root, `${name}.json`));
}

function waitForSocketText(
  socket: net.Socket,
  text: string,
  timeoutMs = 3000,
): Promise<void> {
  return new Promise((resolve, reject) => {
    let seen = "";
    const timer = setTimeout(
      () => reject(new Error(`Timed out waiting for socket text ${text}`)),
      timeoutMs,
    );
    const onData = (data: Buffer) => {
      seen += data.toString("utf-8");
      if (!seen.includes(text)) return;
      clearTimeout(timer);
      socket.off("data", onData);
      resolve();
    };
    socket.on("data", onData);
  });
}

describe("live daemon registry recovery", () => {
  it("rebinds in the same daemon and preserves an established client", async () => {
    const name = "recover-success";
    const { root, pid, providerPid, snapshotPath, socketPath } = await startDaemon(name);
    const snapshot = JSON.parse(fs.readFileSync(snapshotPath, "utf-8"));
    const established = await connect(socketPath);
    clients.push(established);
    const ready = waitForSocketText(established, "PROVIDER-READY");
    established.write(encodeAttach(24, 80));
    await ready;

    unlinkRegistry(root, name);
    expect(() => process.kill(pid, 0)).not.toThrow();

    const result = runRecover(root, name, snapshotPath);
    expect(result.status, result.stderr).toBe(0);
    expect(result.stdout).toContain(`daemon PID ${pid}`);
    expect(Number(fs.readFileSync(path.join(root, `${name}.pid`), "utf-8"))).toBe(pid);
    const recovered = JSON.parse(
      fs.readFileSync(path.join(root, `${name}.json`), "utf-8"),
    );
    expect(recovered.generation).toBe(snapshot.generation);
    expect(recovered.daemonStartToken).toBe(snapshot.daemonStartToken);
    expect(established.destroyed).toBe(false);
    expect(() => process.kill(providerPid, 0)).not.toThrow();
    const ack = waitForSocketText(established, "PROVIDER-ACK:continuity");
    established.write(encodeData("continuity\n"));
    await ack;

    const newClient = await connect(socketPath);
    clients.push(newClient);
    expect(newClient.destroyed).toBe(false);
  });

  it("refuses an unsupported snapshot without poking the daemon", async () => {
    const name = "recover-unsupported";
    const { root, pid, snapshotPath } = await startDaemon(name);
    const snapshot = JSON.parse(fs.readFileSync(snapshotPath, "utf-8"));
    delete snapshot.recoveryProtocol;
    delete snapshot.daemonStartToken;
    fs.writeFileSync(snapshotPath, JSON.stringify(snapshot));
    unlinkRegistry(root, name);

    const result = runRecover(root, name, snapshotPath, 200);
    expect(result.status).toBe(1);
    expect(result.stderr).toContain("refusing recovery");
    expect(() => process.kill(pid, 0)).not.toThrow();
  });

  it("never replaces a foreign listener at the recovered pathname", async () => {
    const name = "recover-conflict";
    const { root, pid, snapshotPath, socketPath } = await startDaemon(name);
    unlinkRegistry(root, name);

    const foreign = net.createServer();
    foreignServers.push(foreign);
    await new Promise<void>((resolve, reject) => {
      foreign.once("error", reject);
      foreign.listen(socketPath, () => resolve());
    });

    const beforeIdentity = fs.statSync(socketPath, { bigint: true });
    const result = runRecover(root, name, snapshotPath, 1200);
    expect(result.status).toBe(1);
    expect(result.stderr).toContain("did not republish");
    expect(() => process.kill(pid, 0)).not.toThrow();
    expect(fs.existsSync(path.join(root, `${name}.pid`))).toBe(false);
    expect(fs.existsSync(path.join(root, `${name}.json`))).toBe(false);
    const afterIdentity = fs.statSync(socketPath, { bigint: true });
    expect(afterIdentity.dev).toBe(beforeIdentity.dev);
    expect(afterIdentity.ino).toBe(beforeIdentity.ino);
    const probe = await connect(socketPath);
    clients.push(probe);
    expect(probe.destroyed).toBe(false);

    probe.destroy();
    await new Promise<void>((resolve) => foreign.close(() => resolve()));
    foreignServers = [];
    const retry = runRecover(root, name, snapshotPath);
    expect(retry.status, retry.stderr).toBe(0);
    expect(Number(fs.readFileSync(path.join(root, `${name}.pid`), "utf-8"))).toBe(pid);
  });

  it("ignores a stale request and accepts a fresh replacement", async () => {
    const name = "recover-stale";
    const { root, pid, snapshotPath } = await startDaemon(name);
    const snapshot = JSON.parse(fs.readFileSync(snapshotPath, "utf-8"));
    unlinkRegistry(root, name);

    fs.writeFileSync(
      path.join(root, `${name}.recover-request.00000000000000000000000000000000`),
      JSON.stringify({
        protocol: 1,
        name,
        nonce: "00000000000000000000000000000000",
        createdAt: new Date(Date.now() - 60_000).toISOString(),
        expectedPid: pid,
        expectedGeneration: snapshot.generation,
        expectedStartToken: snapshot.daemonStartToken,
        snapshot,
      }),
    );
    await new Promise((resolve) => setTimeout(resolve, 750));
    expect(fs.existsSync(path.join(root, `${name}.sock`))).toBe(false);
    expect(() => process.kill(pid, 0)).not.toThrow();

    const fresh = runRecover(root, name, snapshotPath);
    expect(fresh.status, fresh.stderr).toBe(0);
    expect(Number(fs.readFileSync(path.join(root, `${name}.pid`), "utf-8"))).toBe(pid);
  });

  it("recovers on retry after a bind race fails after listener close", async () => {
    const root = fs.mkdtempSync(path.join(testBase, "direct-root-"));
    const previousRoot = process.env.PTY_ROOT;
    const previousLegacyRoot = process.env.PTY_SESSION_DIR;
    process.env.PTY_ROOT = root;
    delete process.env.PTY_SESSION_DIR;
    const name = "recover-bind-race";
    const server = new PtyServer({
      name,
      command: "/bin/sh",
      args: ["-c", "while :; do sleep 1; done"],
      displayCommand: "synthetic-bind-race",
      cwd: os.tmpdir(),
      rows: 24,
      cols: 80,
    });
    let foreign: net.Server | null = null;
    let recoveredClient: net.Socket | null = null;
    try {
      await server.ready;
      const socketPath = path.join(root, `${name}.sock`);
      const snapshot = JSON.parse(
        fs.readFileSync(path.join(root, `${name}.json`), "utf-8"),
      );
      const request = {
        protocol: 1 as const,
        name,
        nonce: "11111111111111111111111111111111",
        createdAt: new Date().toISOString(),
        expectedPid: process.pid,
        expectedGeneration: snapshot.generation,
        expectedStartToken: snapshot.daemonStartToken,
        snapshot,
      };
      unlinkRegistry(root, name);

      await expect(server.recoverLiveRegistry(request, {
        beforeReplacementListen: async () => {
          foreign = net.createServer();
          await new Promise<void>((resolve, reject) => {
            foreign!.once("error", reject);
            foreign!.listen(socketPath, () => resolve());
          });
        },
      })).rejects.toMatchObject({ code: "EADDRINUSE" });
      expect(fs.existsSync(path.join(root, `${name}.pid`))).toBe(false);

      await new Promise<void>((resolve) => foreign!.close(() => resolve()));
      foreign = null;
      await server.recoverLiveRegistry({
        ...request,
        nonce: "22222222222222222222222222222222",
        createdAt: new Date().toISOString(),
      });
      recoveredClient = await connect(socketPath);
      expect(Number(fs.readFileSync(path.join(root, `${name}.pid`), "utf-8")))
        .toBe(process.pid);
    } finally {
      recoveredClient?.destroy();
      if (foreign) {
        await new Promise<void>((resolve) => foreign!.close(() => resolve()));
      }
      await server.close();
      if (previousRoot === undefined) delete process.env.PTY_ROOT;
      else process.env.PTY_ROOT = previousRoot;
      if (previousLegacyRoot === undefined) delete process.env.PTY_SESSION_DIR;
      else process.env.PTY_SESSION_DIR = previousLegacyRoot;
    }
  });

  it("removing one nonce can never delete a concurrently published request", () => {
    const root = fs.mkdtempSync(path.join(testBase, "nonce-root-"));
    const previousRoot = process.env.PTY_ROOT;
    process.env.PTY_ROOT = root;
    const name = "recover-concurrent";
    const nonceA = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const nonceB = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    try {
      fs.writeFileSync(getRecoveryRequestPath(name, nonceA), "{}");
      fs.writeFileSync(getRecoveryRequestPath(name, nonceB), "{}");
      expect(removeLiveRecoveryRequest(name, nonceA)).toBe(true);
      expect(fs.existsSync(getRecoveryRequestPath(name, nonceA))).toBe(false);
      expect(fs.existsSync(getRecoveryRequestPath(name, nonceB))).toBe(true);
    } finally {
      if (previousRoot === undefined) delete process.env.PTY_ROOT;
      else process.env.PTY_ROOT = previousRoot;
    }
  });

  it("rejects a request whose payload nonce differs from its filename", () => {
    const root = fs.mkdtempSync(path.join(testBase, "nonce-mismatch-root-"));
    const previousRoot = process.env.PTY_ROOT;
    process.env.PTY_ROOT = root;
    const name = "recover-nonce-mismatch";
    const nonceA = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const nonceB = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    try {
      fs.writeFileSync(
        getRecoveryRequestPath(name, nonceA),
        JSON.stringify({
          protocol: 1,
          name,
          nonce: nonceB,
          createdAt: new Date().toISOString(),
          expectedPid: process.pid,
          expectedGeneration: "generation",
          expectedStartToken: "start-token",
          snapshot: {
            recoveryProtocol: 1,
            daemonStartToken: "start-token",
          },
        }),
      );
      fs.writeFileSync(getRecoveryRequestPath(name, nonceB), "{}");

      expect(readLiveRecoveryRequest(name, nonceA)).toBeNull();
      expect(fs.existsSync(getRecoveryRequestPath(name, nonceB))).toBe(true);
    } finally {
      if (previousRoot === undefined) delete process.env.PTY_ROOT;
      else process.env.PTY_ROOT = previousRoot;
    }
  });

  it("cleanup cannot delete another legal session name's request", () => {
    const root = fs.mkdtempSync(path.join(testBase, "cleanup-isolation-root-"));
    const previousRoot = process.env.PTY_ROOT;
    process.env.PTY_ROOT = root;
    const name = "recover-cleanup";
    const otherName = `${name}.recover-request.child`;
    const nonce = "cccccccccccccccccccccccccccccccc";
    try {
      fs.writeFileSync(getRecoveryRequestPath(name, nonce), "{}");
      fs.writeFileSync(getRecoveryRequestPath(otherName, nonce), "{}");

      cleanupSocket(name);

      expect(fs.existsSync(getRecoveryRequestPath(name, nonce))).toBe(false);
      expect(fs.existsSync(getRecoveryRequestPath(otherName, nonce))).toBe(true);
    } finally {
      if (previousRoot === undefined) delete process.env.PTY_ROOT;
      else process.env.PTY_ROOT = previousRoot;
    }
  });
});
