import { describe, it, expect, beforeAll, afterAll } from "vitest";
import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, spawnSync } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");

// Short roots: PTY_ROOT must stay under the 90-byte socket-path backstop.
const rand = () => Math.random().toString(36).slice(2, 7);
const srvRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pr-srv-"));
const cliRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pr-cli-"));
const ctrlSock = path.join(os.tmpdir(), `pr-ctrl-${rand()}.sock`);
const fakeFabric = path.join(os.tmpdir(), `pr-fabric-${rand()}.sh`);
const bgPids: number[] = [];

function runCli(root: string, args: string[], env: Record<string, string> = {}) {
  return spawnSync(nodeBin, [cliPath, ...args], {
    env: { ...process.env, PTY_ROOT: root, PTY_ROOT_LEGACY_SILENT: "1", ...env },
    encoding: "utf8",
    timeout: 15000,
  });
}

function waitForFile(p: string, timeoutMs: number): boolean {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    try { fs.statSync(p); return true; } catch {}
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 50);
  }
  return false;
}

/** Speak the control protocol directly (no fabric): one line {"op":"list"},
 *  read until the server half-closes, parse the JSON response. */
function rawList(sockPath: string): Promise<{ sessions: { name: string }[] }> {
  return new Promise((resolve, reject) => {
    const c = net.createConnection(sockPath);
    let buf = "";
    c.on("connect", () => c.write(JSON.stringify({ op: "list" }) + "\n"));
    c.on("data", (d: Buffer) => { buf += d.toString("utf8"); });
    c.on("end", () => {
      try { resolve(JSON.parse(buf.trim())); } catch (e) { reject(e); }
    });
    c.on("error", reject);
  });
}

beforeAll(() => {
  // A real session on the "remote" (srvRoot).
  const r = runCli(srvRoot, ["run", "-d", "--id", "demo", "--name", "Demo Session", "--", "sleep", "300"]);
  expect(r.status).toBe(0);
  try { bgPids.push(Number(fs.readFileSync(path.join(srvRoot, "demo.pid"), "utf8").trim())); } catch {}

  // Real `pty remote-serve` subprocess, reading the remote's PTY_ROOT.
  const serve = spawn(nodeBin, [cliPath, "remote-serve", "--socket", ctrlSock], {
    env: { ...process.env, PTY_ROOT: srvRoot, PTY_ROOT_LEGACY_SILENT: "1" },
    detached: true,
    stdio: ["ignore", "ignore", "ignore"],
  });
  serve.unref();
  bgPids.push(serve.pid!);
  expect(waitForFile(ctrlSock, 5000)).toBe(true);

  // Fake fabric: `dial <peer> <alpn>` prints the local control socket path.
  fs.writeFileSync(
    fakeFabric,
    `#!/bin/sh\nif [ "$1" = "dial" ]; then printf '%s' "${ctrlSock}"; fi\n`,
    { mode: 0o755 },
  );
});

afterAll(() => {
  for (const pid of bgPids) { try { process.kill(pid, "SIGKILL"); } catch {} }
  for (const p of [ctrlSock, fakeFabric]) { try { fs.rmSync(p, { force: true }); } catch {} }
  for (const d of [srvRoot, cliRoot]) { try { fs.rmSync(d, { recursive: true, force: true }); } catch {} }
});

describe("pty ls --remote over fabric", () => {
  it("lists the remote peer's sessions (json), local root empty", () => {
    const r = runCli(cliRoot, ["ls", "--remote", "testpeer", "--json"], { PTY_FABRIC_BIN: fakeFabric });
    expect(r.status).toBe(0);
    const out = JSON.parse(r.stdout);
    expect(out.local).toEqual([]);
    expect(out.remote).toHaveLength(1);
    expect(out.remote[0].label).toBe("testpeer");
    expect(out.remote[0].error).toBeNull();
    const names = out.remote[0].sessions.map((s: { name: string }) => s.name);
    expect(names).toContain("demo");
    const demo = out.remote[0].sessions.find((s: { name: string }) => s.name === "demo");
    expect(demo.status).toBe("running");
    expect(demo.command).toBe("sleep 300");
    expect(demo.displayName).toBe("Demo Session");
  }, 20000);

  it("renders the remote host group in human output", () => {
    const r = runCli(cliRoot, ["ls", "--remote", "testpeer"], { PTY_FABRIC_BIN: fakeFabric });
    expect(r.status).toBe(0);
    expect(r.stdout).toContain("testpeer");
    expect(r.stdout).toContain("Demo Session");
    expect(r.stdout).toContain("sleep 300");
  }, 20000);

  it("surfaces a fabric-dial failure as a host-group error, not a crash", () => {
    // A fabric shim that exits non-zero → execFileSync throws → error captured.
    const badFabric = path.join(os.tmpdir(), `pr-badfab-${rand()}.sh`);
    fs.writeFileSync(badFabric, `#!/bin/sh\nexit 3\n`, { mode: 0o755 });
    try {
      const r = runCli(cliRoot, ["ls", "--remote", "downpeer", "--json"], { PTY_FABRIC_BIN: badFabric });
      expect(r.status).toBe(0); // still exits cleanly
      const out = JSON.parse(r.stdout);
      expect(out.remote[0].label).toBe("downpeer");
      expect(out.remote[0].error).toBeTruthy();
      expect(out.remote[0].sessions).toEqual([]);
    } finally {
      fs.rmSync(badFabric, { force: true });
    }
  }, 20000);

  it("survives a detached (session-leader) stdin=/dev/null launch and keeps serving", async () => {
    // Topology note: Node `detached: true` calls setsid(), so the spawned pty
    // IS the session leader (PGID==PID) with no controlling TTY — i.e. the exact
    // `setsid pty remote-serve …` shape, NOT a wrapped child. This locks the
    // detached/loop-drain + SIGHUP behavior. CAVEAT: the deeper "pty as session
    // leader dies below the JS layer" failure cos hit is Linux-specific and does
    // NOT reproduce on macOS, so on a Mac this asserts the topology stays up but
    // cannot exercise that Linux-only death; a Linux run would. remote-serve must
    // not exit when stdin closes, and must keep answering `list`.
    const sock = path.join(os.tmpdir(), `pr-svc-${rand()}.sock`);
    const proc = spawn(nodeBin, [cliPath, "remote-serve", "--socket", sock], {
      env: { ...process.env, PTY_ROOT: srvRoot, PTY_ROOT_LEGACY_SILENT: "1" },
      detached: true,
      stdio: ["ignore", "ignore", "ignore"], // stdin = /dev/null
    });
    proc.unref();
    bgPids.push(proc.pid!);
    expect(waitForFile(sock, 5000)).toBe(true);

    // Past the ~2s window where a stdin-dependent loop would have drained.
    await new Promise((r) => setTimeout(r, 2500));
    expect(() => process.kill(proc.pid!, 0)).not.toThrow(); // still alive

    // And still functional over its socket.
    const resp = await rawList(sock);
    expect(resp.sessions.map((s) => s.name)).toContain("demo");

    try { process.kill(proc.pid!, "SIGTERM"); } catch {}
    try { fs.rmSync(sock, { force: true }); } catch {}
  }, 15000);

  it("ignores SIGHUP and keeps serving (the detached-launch death on Linux)", async () => {
    // The Hetzner failure: a detached launch is killed by the SIGHUP its
    // launching session sends on teardown (SIGHUP's default action terminates).
    // remote-serve must ignore it. Deterministic everywhere — POSIX SIGHUP.
    const sock = path.join(os.tmpdir(), `pr-hup-${rand()}.sock`);
    const proc = spawn(nodeBin, [cliPath, "remote-serve", "--socket", sock], {
      env: { ...process.env, PTY_ROOT: srvRoot, PTY_ROOT_LEGACY_SILENT: "1" },
      detached: true,
      stdio: ["ignore", "ignore", "ignore"],
    });
    proc.unref();
    bgPids.push(proc.pid!);
    expect(waitForFile(sock, 5000)).toBe(true);

    process.kill(proc.pid!, "SIGHUP");
    await new Promise((r) => setTimeout(r, 600));
    expect(() => process.kill(proc.pid!, 0)).not.toThrow(); // survived SIGHUP

    const resp = await rawList(sock); // and still functional
    expect(resp.sessions.map((s) => s.name)).toContain("demo");

    try { process.kill(proc.pid!, "SIGTERM"); } catch {}
    try { fs.rmSync(sock, { force: true }); } catch {}
  }, 15000);
});
