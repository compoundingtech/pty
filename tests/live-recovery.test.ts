import { afterAll, afterEach, describe, expect, it } from "vitest";
import { spawn, spawnSync } from "node:child_process";
import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { terminateAndWait } from "./setup/processes.ts";
import { encodeAttach, encodeData } from "../src/protocol.ts";

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

    const result = runRecover(root, name, snapshotPath, 200);
    expect(result.status).toBe(1);
    expect(result.stderr).toContain("did not republish");
    expect(() => process.kill(pid, 0)).not.toThrow();
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
      path.join(root, `${name}.recover-request`),
      JSON.stringify({
        protocol: 1,
        name,
        nonce: "stale-request",
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
});
