// Session-owned (pinned) geometry: `pty run --size`, `pty resize`, and the
// `geometry` block in stats.
//
// The complement to tests/neutral-attach-cli.test.ts. That file covers the
// CLIENT opting out of size negotiation (`attach --no-resize`); this file
// covers the SESSION revoking the vote from every client — which is the only
// thing that stops a plain, non-neutral attacher from reflowing a running TUI.
//
// Daemons are started directly from dist/server.js with an explicit config
// (the same idiom as tests/stats-cli.test.ts) so the assertions exercise the
// shipped server, and raw sockets stand in for attaching clients so we can
// drive exact protocol frames.

import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, execFileSync } from "node:child_process";
import {
  MessageType,
  PacketReader,
  encodeAttach,
  encodeResize,
  encodeResizeAuthoritative,
  decodeAttachFlags,
  RESIZE_FLAG_AUTHORITATIVE,
} from "../src/protocol.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const serverModule = path.join(__dirname, "..", "dist", "server.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-geometry-"));
afterAll(() => {
  fs.rmSync(testRoot, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

let bgPids: number[] = [];
let openSockets: net.Socket[] = [];

function makeSessionDir(): string {
  return fs.mkdtempSync(path.join(testRoot, "d-"));
}

let nameCounter = 0;
function uniqueName(): string {
  return `g${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
}

/**
 * A child that reports its terminal dimensions and counts SIGWINCH deliveries.
 * `WINCH:<n>:<cols>x<rows>` is reprinted on every resize, so the *number* of
 * WINCHes is observable, not just the final size — that is what distinguishes
 * an authoritative resize (exactly one) from a nudge (two).
 */
const winchCounterScript = `
let n = 0;
function report() {
  process.stdout.write('WINCH:' + n + ':' + process.stdout.columns + 'x' + process.stdout.rows + '\\n');
}
report();
process.stdout.on('resize', () => { n++; report(); });
setTimeout(() => {}, 300000);
`;

/** Parse the child's most recent `WINCH:<n>:<cols>x<rows>` report. */
function lastWinch(text: string): { n: number; cols: number; rows: number } | null {
  const matches = [...text.matchAll(/WINCH:(\d+):(\d+)x(\d+)/g)];
  const m = matches[matches.length - 1];
  if (!m) return null;
  return { n: Number(m[1]), cols: Number(m[2]), rows: Number(m[3]) };
}

interface Daemon {
  name: string;
  sessionDir: string;
  socketPath: string;
}

async function startDaemon(opts: {
  command: string;
  args?: string[];
  rows?: number;
  cols?: number;
  pinGeometry?: boolean;
}): Promise<Daemon> {
  const sessionDir = makeSessionDir();
  const name = uniqueName();
  const config = JSON.stringify({
    name,
    command: opts.command,
    args: opts.args ?? [],
    displayCommand: opts.command,
    cwd: os.tmpdir(),
    rows: opts.rows ?? 24,
    cols: opts.cols ?? 80,
    ...(opts.pinGeometry ? { pinGeometry: true } : {}),
  });

  const child = spawn(nodeBin, [serverModule], {
    detached: true,
    stdio: ["ignore", "ignore", "pipe"],
    env: { ...process.env, PTY_SERVER_CONFIG: config, PTY_SESSION_DIR: sessionDir },
  });

  let stderr = "";
  child.stderr?.on("data", (d: Buffer) => { stderr += d.toString(); });
  let exitCode: number | null = null;
  child.on("exit", (code) => { exitCode = code; });
  (child.stderr as any)?.unref?.();
  child.unref();

  const socketPath = path.join(sessionDir, `${name}.sock`);
  const start = Date.now();
  while (Date.now() - start < 5000) {
    if (exitCode !== null) {
      throw new Error(`Daemon exited with code ${exitCode}. stderr:\n${stderr}`);
    }
    try {
      fs.statSync(socketPath);
      await new Promise((r) => setTimeout(r, 150));
      bgPids.push(child.pid!);
      return { name, sessionDir, socketPath };
    } catch {}
    await new Promise((r) => setTimeout(r, 50));
  }
  throw new Error(`Timeout waiting for daemon socket: ${socketPath}`);
}

/** Attach a raw socket client and collect the DATA it receives as text. */
async function attachRaw(
  d: Daemon,
  rows: number,
  cols: number,
  geometryNeutral = false,
): Promise<{ socket: net.Socket; text: () => string }> {
  let text = "";
  const reader = new PacketReader();
  const socket = net.createConnection(d.socketPath);
  openSockets.push(socket);
  socket.on("data", (data: Buffer) => {
    for (const packet of reader.feed(data)) {
      if (packet.type === MessageType.DATA || packet.type === MessageType.SCREEN) {
        text += packet.payload.toString();
      }
    }
  });
  await new Promise<void>((resolve, reject) => {
    socket.once("error", reject);
    socket.once("connect", () => resolve());
  });
  socket.write(encodeAttach(rows, cols, geometryNeutral));
  await new Promise((r) => setTimeout(r, 400));
  return { socket, text: () => text };
}

function stats(d: Daemon): any {
  const out = execFileSync(nodeBin, [cliPath, "stats", "--json", d.name], {
    env: { ...process.env, PTY_SESSION_DIR: d.sessionDir },
    encoding: "utf-8",
    timeout: 10000,
  });
  return JSON.parse(out);
}

afterEach(() => {
  for (const s of openSockets) { try { s.destroy(); } catch {} }
  openSockets = [];
  for (const pid of bgPids) { try { process.kill(pid, "SIGKILL"); } catch {} }
  bgPids = [];
});

// ---- protocol ----

describe("RESIZE_FLAG_AUTHORITATIVE", () => {
  it("rides in the same optional byte-4 slot as the ATTACH flags", () => {
    const frame = encodeResizeAuthoritative(48, 160);
    // 5-byte header + 5-byte payload (4 size bytes + 1 flags byte).
    const payload = frame.subarray(5);
    expect(payload.length).toBe(5);
    expect(payload.readUInt16BE(0)).toBe(48);
    expect(payload.readUInt16BE(2)).toBe(160);
    expect(decodeAttachFlags(payload) & RESIZE_FLAG_AUTHORITATIVE).toBeTruthy();
  });

  it("leaves an ordinary client RESIZE byte-identical to the legacy frame", () => {
    // Backward compatibility: a plain resize must not grow a flags byte, so
    // old daemons keep parsing it and read no flags.
    const payload = encodeResize(30, 100).subarray(5);
    expect(payload.length).toBe(4);
    expect(decodeAttachFlags(payload)).toBe(0);
  });
});

// ---- stats ----

describe("stats geometry block", () => {
  it("reports client-owned geometry for an unpinned session", async () => {
    const d = await startDaemon({ command: "sleep", args: ["300"] });
    const s = stats(d);
    expect(s.geometry).toEqual({ owner: "clients", pinned: null });
    // The capability flag is the clean contract; the block stays for consumers
    // that sniff it.
    expect(s.capabilities.pinnedGeometry).toBe(true);
    expect(s.capabilities.geometryNeutralAttach).toBe(true);
  }, 20000);

  it("reports session-owned geometry for a pinned session", async () => {
    const d = await startDaemon({
      command: "sleep", args: ["300"],
      rows: 48, cols: 160, pinGeometry: true,
    });
    const s = stats(d);
    expect(s.geometry.owner).toBe("session");
    expect(s.geometry.pinned).toEqual({ cols: 160, rows: 48 });
    expect(s.terminal.cols).toBe(160);
    expect(s.terminal.rows).toBe(48);
  }, 20000);
});

// ---- the load-bearing behaviour ----

describe("pinned geometry vs. attaching clients", () => {
  it("a PLAIN attach does not reflow a pinned session", async () => {
    const d = await startDaemon({
      command: "sleep", args: ["300"],
      rows: 48, cols: 160, pinGeometry: true,
    });

    // A deliberately non-neutral client at a much smaller size. Under min-wins
    // negotiation this would drag the child down to 80x24 — the exact failure
    // `attach --no-resize` cannot prevent, because this client never opted in.
    await attachRaw(d, 24, 80, false);

    const s = stats(d);
    expect(s.terminal.cols).toBe(160);
    expect(s.terminal.rows).toBe(48);
    expect(s.geometry.owner).toBe("session");
  }, 20000);

  it("a plain attach DOES reflow an unpinned session (negotiation still works)", async () => {
    // Guards against the pin being applied unconditionally: the default,
    // client-owned behaviour must be untouched.
    const d = await startDaemon({
      command: "sleep", args: ["300"], rows: 48, cols: 160,
    });
    await attachRaw(d, 24, 80, false);
    const s = stats(d);
    expect(s.terminal.cols).toBe(80);
    expect(s.terminal.rows).toBe(24);
  }, 20000);
});

describe("authoritative resize", () => {
  it("emits exactly one SIGWINCH and pins the new size", async () => {
    const dir = fs.mkdtempSync(path.join(testRoot, "winch-"));
    const scriptPath = path.join(dir, "winch.js");
    fs.writeFileSync(scriptPath, winchCounterScript);

    const d = await startDaemon({
      command: nodeBin, args: [scriptPath], rows: 24, cols: 80,
    });

    const client = await attachRaw(d, 24, 80, false);
    // Baseline is NOT zero: attaching a plain client triggers the redraw nudge
    // (resize -1 col and back), which is itself two real WINCHes. So count the
    // delta the resize adds rather than the absolute total.
    const before = lastWinch(client.text());
    expect(before).not.toBeNull();

    const socket = net.createConnection(d.socketPath);
    openSockets.push(socket);
    await new Promise<void>((resolve, reject) => {
      socket.once("error", reject);
      socket.once("connect", () => resolve());
    });
    socket.write(encodeResizeAuthoritative(40, 120));
    await new Promise((r) => setTimeout(r, 600));

    const after = lastWinch(client.text());
    // Exactly one WINCH for the resize: the authoritative path resizes once and
    // never nudges. A nudge would show up here as a delta of 2.
    expect(after).not.toBeNull();
    expect(after!.n - before!.n).toBe(1);
    expect(after!.cols).toBe(120);
    expect(after!.rows).toBe(40);

    const s = stats(d);
    expect(s.terminal.cols).toBe(120);
    expect(s.terminal.rows).toBe(40);
    // The resize pins the session, so it now owns its geometry.
    expect(s.geometry).toEqual({ owner: "session", pinned: { cols: 120, rows: 40 } });

    fs.rmSync(dir, { recursive: true, force: true });
  }, 25000);

  it("ignores a zero/negative size rather than resizing to nothing", async () => {
    const d = await startDaemon({ command: "sleep", args: ["300"], rows: 24, cols: 80 });
    const socket = net.createConnection(d.socketPath);
    openSockets.push(socket);
    await new Promise<void>((resolve, reject) => {
      socket.once("error", reject);
      socket.once("connect", () => resolve());
    });
    socket.write(encodeResizeAuthoritative(0, 0));
    await new Promise((r) => setTimeout(r, 400));
    const s = stats(d);
    expect(s.terminal.cols).toBe(80);
    expect(s.terminal.rows).toBe(24);
  }, 20000);
});

// ---- CLI surface ----

describe("pty run --size", () => {
  it("parses --size and keeps parsing the flags that follow it", async () => {
    // Regression guard for the reason this matters downstream: the `run` flag
    // loop ends in `else break`, so an unknown `--size` silently halts parsing
    // and every later flag (notably `--id`) is swallowed as part of the
    // command. The session would then get a random id instead of its name.
    const sessionDir = makeSessionDir();
    const name = uniqueName();
    execFileSync(nodeBin, [
      cliPath, "run", "-d", "--size", "160x48", "--id", name, "--", "sleep", "300",
    ], {
      env: { ...process.env, PTY_SESSION_DIR: sessionDir, PTY_SESSION: "" },
      encoding: "utf-8",
      timeout: 15000,
    });

    // --id survived the --size flag: the session exists under its intended name.
    const s = JSON.parse(execFileSync(nodeBin, [cliPath, "stats", "--json", name], {
      env: { ...process.env, PTY_SESSION_DIR: sessionDir },
      encoding: "utf-8",
      timeout: 10000,
    }));
    expect(s.name).toBe(name);
    // ...and --size pinned the geometry at spawn.
    expect(s.geometry).toEqual({ owner: "session", pinned: { cols: 160, rows: 48 } });
    expect(s.terminal.cols).toBe(160);
    expect(s.terminal.rows).toBe(48);

    execFileSync(nodeBin, [cliPath, "kill", name], {
      env: { ...process.env, PTY_SESSION_DIR: sessionDir },
      encoding: "utf-8", timeout: 10000,
    });
  }, 30000);

  it("rejects a malformed --size", () => {
    const sessionDir = makeSessionDir();
    let failed = false;
    let stderr = "";
    try {
      execFileSync(nodeBin, [cliPath, "run", "-d", "--size", "wide", "--", "sleep", "1"], {
        env: { ...process.env, PTY_SESSION_DIR: sessionDir, PTY_SESSION: "" },
        encoding: "utf-8", timeout: 10000, stdio: ["ignore", "pipe", "pipe"],
      });
    } catch (e: any) {
      failed = true;
      stderr = e.stderr ?? "";
    }
    expect(failed).toBe(true);
    expect(stderr).toContain("Invalid --size");
  }, 15000);
});

describe("pty resize", () => {
  it("resizes a running session end-to-end and reports it in stats", async () => {
    const d = await startDaemon({ command: "sleep", args: ["300"], rows: 24, cols: 80 });
    const out = execFileSync(nodeBin, [cliPath, "resize", d.name, "120x40"], {
      env: { ...process.env, PTY_SESSION_DIR: d.sessionDir },
      encoding: "utf-8", timeout: 10000,
    });
    expect(out).toContain("120x40");
    const s = stats(d);
    expect(s.geometry).toEqual({ owner: "session", pinned: { cols: 120, rows: 40 } });
  }, 20000);

  it("rejects a malformed size spec", () => {
    let failed = false;
    let stderr = "";
    try {
      execFileSync(nodeBin, [cliPath, "resize", "whatever", "big"], {
        env: { ...process.env, PTY_SESSION_DIR: makeSessionDir() },
        encoding: "utf-8", timeout: 10000, stdio: ["ignore", "pipe", "pipe"],
      });
    } catch (e: any) {
      failed = true;
      stderr = e.stderr ?? "";
    }
    expect(failed).toBe(true);
    expect(stderr).toContain("Usage: pty resize");
  }, 15000);
});
