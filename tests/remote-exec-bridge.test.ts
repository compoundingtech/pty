import { describe, it, expect, beforeAll, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, spawnSync } from "node:child_process";

// Proves the on-demand `pty remote-serve-handle --stdio` end-to-end by mimicking
// fabric's `--exec` deploy: a local socket that, per connection, spawns the
// handler and pipes socket <-> child stdin/stdout. The REAL `--remote` client
// (list/peek/send) runs through it — no persistent remote-serve daemon involved.
//
// The exec-bridge MUST run in its own process, not in this test worker: the
// client is driven with `spawnSync`, which blocks the worker's event loop for
// the whole call, so an in-process bridge could never accept the dial or pump
// bytes while a client is running (it would deadlock every route to a 30s
// timeout). remote-fabric.test.ts spawns `pty remote-serve` the same way and
// for the same reason.

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const rand = () => Math.random().toString(36).slice(2, 7);
const sleepSync = (ms: number) => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);

const srvRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pr-xb-srv-"));
const cliRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pr-xb-cli-"));
const bridgeSock = path.join(os.tmpdir(), `pr-xb-bridge-${rand()}.sock`);
const bridgeScript = path.join(os.tmpdir(), `pr-xb-bridge-${rand()}.cjs`);
const fakeFabric = path.join(os.tmpdir(), `pr-xb-fabric-${rand()}.sh`);
const bgPids: number[] = [];

// Out-of-process exec-bridge: mimic `fabric expose pty-view --exec -- pty
// remote-serve-handle --stdio`. Per accepted connection, spawn the handler and
// bridge the socket <-> its stdin/stdout. Runs detached so it survives the
// test worker's blocking spawnSync calls.
const BRIDGE_SRC = `
const net = require("node:net");
const { spawn } = require("node:child_process");
const fs = require("node:fs");
const [bridgeSock, cliPath, srvRoot] = process.argv.slice(2);
try { fs.unlinkSync(bridgeSock); } catch {}
const server = net.createServer((sock) => {
  const child = spawn(process.execPath, [cliPath, "remote-serve-handle", "--stdio"], {
    env: { ...process.env, PTY_ROOT: srvRoot, PTY_ROOT_LEGACY_SILENT: "1" },
    stdio: ["pipe", "pipe", "ignore"], // fabric routes child stderr to its log; drop it here
  });
  sock.pipe(child.stdin);
  child.stdout.pipe(sock);
  sock.on("close", () => { try { child.kill(); } catch {} });
  sock.on("error", () => {});
  child.on("exit", () => { try { sock.end(); } catch {} });
  child.on("error", () => {});
});
server.listen(bridgeSock);
`;

function runCli(root: string, args: string[], env: Record<string, string> = {}) {
  return spawnSync(nodeBin, [cliPath, ...args], {
    env: { ...process.env, PTY_ROOT: root, PTY_ROOT_LEGACY_SILENT: "1", ...env },
    encoding: "utf8", timeout: 15000,
  });
}

beforeAll(() => {
  const c = runCli(srvRoot, ["run", "-d", "--id", "demo", "--name", "Demo", "--", "sh", "-c", "printf 'EXEC_MARK\\r\\n'; cat"]);
  expect(c.status).toBe(0);
  try { bgPids.push(Number(fs.readFileSync(path.join(srvRoot, "demo.pid"), "utf8").trim())); } catch {}
  sleepSync(500);

  fs.writeFileSync(bridgeScript, BRIDGE_SRC);
  const bridge = spawn(nodeBin, [bridgeScript, bridgeSock, cliPath, srvRoot], {
    detached: true, stdio: "ignore",
  });
  bridge.unref();
  if (bridge.pid) bgPids.push(bridge.pid);

  // Wait for the bridge to be listening before any client dials it.
  for (let i = 0; i < 100 && !fs.existsSync(bridgeSock); i++) sleepSync(50);
  expect(fs.existsSync(bridgeSock)).toBe(true);

  fs.writeFileSync(fakeFabric, `#!/bin/sh\nif [ "$1" = "dial" ]; then printf '%s' "${bridgeSock}"; fi\n`, { mode: 0o755 });
});

afterAll(() => {
  // bgPids: [demo session pid, bridge pid]. The bridge is detached (its own
  // process group) — kill the group so the listener and any live handler die.
  for (const pid of bgPids) {
    try { process.kill(-pid, "SIGKILL"); } catch { /* not a group leader */ }
    try { process.kill(pid, "SIGKILL"); } catch { /* already gone */ }
  }
  for (const p of [bridgeSock, bridgeScript, fakeFabric]) { try { fs.rmSync(p, { force: true }); } catch { /* none */ } }
  for (const d of [srvRoot, cliRoot]) { try { fs.rmSync(d, { recursive: true, force: true }); } catch { /* none */ } }
});

describe("pty --remote via fabric --exec (on-demand remote-serve-handle --stdio)", () => {
  it("list --remote works through the exec-bridge (handler spawned per dial)", () => {
    const r = runCli(cliRoot, ["ls", "--remote", "testpeer", "--json"], { PTY_FABRIC_BIN: fakeFabric });
    expect(r.status).toBe(0);
    const out = JSON.parse(r.stdout);
    expect(out.remote[0].sessions.map((s: { name: string }) => s.name)).toContain("demo");
  }, 20000);

  it("peek --remote routes + splices over stdio (screen streams back)", () => {
    const r = runCli(cliRoot, ["peek", "--remote", "testpeer", "demo", "--plain"], { PTY_FABRIC_BIN: fakeFabric });
    expect(r.status).toBe(0);
    expect(r.stdout).toContain("EXEC_MARK");
  }, 20000);

  it("send --remote delivers input over stdio (cat echoes it back on peek)", () => {
    const s = runCli(cliRoot, ["send", "--remote", "testpeer", "demo", "--seq", "STDIO_SEND_OK", "--seq", "key:return"], { PTY_FABRIC_BIN: fakeFabric });
    expect(s.status).toBe(0);
    sleepSync(400);
    const p = runCli(cliRoot, ["peek", "--remote", "testpeer", "demo", "--plain"], { PTY_FABRIC_BIN: fakeFabric });
    expect(p.stdout).toContain("STDIO_SEND_OK");
  }, 20000);

  it("peek --remote a nonexistent session → clean not-found (exit 1)", () => {
    const r = runCli(cliRoot, ["peek", "--remote", "testpeer", "does-not-exist", "--plain"], { PTY_FABRIC_BIN: fakeFabric });
    expect(r.status).not.toBe(0);
    expect(r.stderr).toMatch(/not found/);
  }, 20000);
});
