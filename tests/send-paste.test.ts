// CLI-surface tests for `pty send --paste`. Exercises the binary via
// spawnSync so the argv-parsing path is covered end-to-end, including
// the `--paste` flag's position-independence (before/after --seq, with
// --with-delay, etc.).

import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, spawnSync } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const serverModule = path.join(__dirname, "..", "dist", "server.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-paste-"));
afterAll(() => {
  fs.rmSync(testRoot, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

let bgPids: number[] = [];
let sessionDirs: string[] = [];

function makeSessionDir(): string {
  const dir = fs.mkdtempSync(path.join(testRoot, "d-"));
  sessionDirs.push(dir);
  return dir;
}

let nameCounter = 0;
function uniqueName(): string {
  return `sp${++nameCounter}${Math.random().toString(36).slice(2, 5)}`;
}

async function startDaemon(
  sessionDir: string,
  name: string,
  command: string,
  args: string[] = [],
): Promise<number> {
  const config = JSON.stringify({
    name, command, args, displayCommand: command,
    cwd: os.tmpdir(), rows: 24, cols: 80,
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
    if (exitCode !== null) throw new Error(`Daemon exited: ${stderr}`);
    try {
      fs.statSync(socketPath);
      await new Promise((r) => setTimeout(r, 100));
      bgPids.push(child.pid!);
      return child.pid!;
    } catch {}
    await new Promise((r) => setTimeout(r, 50));
  }
  throw new Error(`Timeout waiting for daemon`);
}

function runCli(sessionDir: string, ...args: string[]) {
  return spawnSync(nodeBin, [cliPath, ...args], {
    cwd: os.tmpdir(),
    env: { ...process.env, PTY_SESSION_DIR: sessionDir },
    encoding: "utf-8",
    timeout: 10_000,
  });
}

/** Run a dump-to-file child in the session so we can read the raw bytes
 *  the `send` write produced — xterm-headless interprets bracketed-paste
 *  START/END as valid CSI sequences and absorbs them, so peeking the
 *  screen wouldn't show the markers. `stty raw -echo` keeps the PTY line
 *  discipline from munging or echoing the ESC bytes. */
async function startDumpSession(sessionDir: string, name: string, dumpFile: string) {
  await startDaemon(sessionDir, name, "sh", [
    "-c",
    `stty raw -echo; cat > ${JSON.stringify(dumpFile)}`,
  ]);
  await new Promise((r) => setTimeout(r, 150));
}

async function waitForDump(dumpFile: string, minBytes: number, timeoutMs: number): Promise<string> {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    try {
      const buf = fs.readFileSync(dumpFile);
      if (buf.length >= minBytes) return buf.toString("utf-8");
    } catch {}
    await new Promise((r) => setTimeout(r, 50));
  }
  return fs.existsSync(dumpFile) ? fs.readFileSync(dumpFile).toString("utf-8") : "";
}

afterEach(() => {
  for (const pid of bgPids) { try { process.kill(pid, "SIGTERM"); } catch {} }
  bgPids = [];
  for (const dir of sessionDirs) {
    try {
      for (const e of fs.readdirSync(dir)) { try { fs.unlinkSync(path.join(dir, e)); } catch {} }
    } catch {}
  }
  sessionDirs = [];
});

describe("pty send --paste", () => {
  it("wraps positional text in bracketed-paste markers", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const dump = path.join(dir, "dump.bin");
    await startDumpSession(dir, name, dump);

    const r = runCli(dir, "send", name, "--paste", "hello-paste");
    expect(r.status).toBe(0);

    const received = await waitForDump(dump, "hello-paste".length + 12, 3000);
    expect(received).toBe("\x1b[200~hello-paste\x1b[201~");
  }, 15_000);

  it("works with --paste placed AFTER the text (filter extracts it regardless)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const dump = path.join(dir, "dump.bin");
    await startDumpSession(dir, name, dump);

    const r = runCli(dir, "send", name, "post-paste", "--paste");
    expect(r.status).toBe(0);

    const received = await waitForDump(dump, "post-paste".length + 12, 3000);
    expect(received).toBe("\x1b[200~post-paste\x1b[201~");
  }, 15_000);

  it("wraps an ordered --seq payload as a single paste", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const dump = path.join(dir, "dump.bin");
    await startDumpSession(dir, name, dump);

    const r = runCli(
      dir, "send", name, "--paste",
      "--seq", "first ",
      "--seq", "second ",
      "--seq", "third",
    );
    expect(r.status).toBe(0);

    const expected = "\x1b[200~first second third\x1b[201~";
    const received = await waitForDump(dump, expected.length, 3000);
    expect((received.match(/\x1b\[200~/g) ?? []).length).toBe(1);
    expect((received.match(/\x1b\[201~/g) ?? []).length).toBe(1);
    expect(received).toBe(expected);
  }, 15_000);

  it("composes with --with-delay", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const dump = path.join(dir, "dump.bin");
    await startDumpSession(dir, name, dump);

    const r = runCli(
      dir, "send", name, "--with-delay", "0.05", "--paste",
      "--seq", "A",
      "--seq", "B",
    );
    expect(r.status).toBe(0);

    const expected = "\x1b[200~AB\x1b[201~";
    const received = await waitForDump(dump, expected.length, 3000);
    expect((received.match(/\x1b\[200~/g) ?? []).length).toBe(1);
    expect((received.match(/\x1b\[201~/g) ?? []).length).toBe(1);
    expect(received).toBe(expected);
  }, 15_000);

  it("without --paste, no bracketed-paste markers are emitted (control)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const dump = path.join(dir, "dump.bin");
    await startDumpSession(dir, name, dump);

    const r = runCli(dir, "send", name, "plain-text");
    expect(r.status).toBe(0);

    const received = await waitForDump(dump, "plain-text".length, 3000);
    expect(received).toBe("plain-text");
    expect(received).not.toContain("\x1b[200~");
    expect(received).not.toContain("\x1b[201~");
  }, 15_000);

  it("handles multi-line payload inside one paste event", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const dump = path.join(dir, "dump.bin");
    await startDumpSession(dir, name, dump);

    // A newline inside the paste is preserved as a literal byte between
    // the markers; the receiver treats the whole thing as one paste.
    const r = runCli(dir, "send", name, "--paste", "line-one\nline-two\n");
    expect(r.status).toBe(0);

    const expected = "\x1b[200~line-one\nline-two\n\x1b[201~";
    const received = await waitForDump(dump, expected.length, 3000);
    expect(received).toBe(expected);
    expect((received.match(/\x1b\[200~/g) ?? []).length).toBe(1);
    expect((received.match(/\x1b\[201~/g) ?? []).length).toBe(1);
  }, 15_000);
});

// Arg-parsing validation runs before any socket connection, so these
// tests don't need a daemon — just spawn the CLI and check stderr/exit.
describe("pty send strict flag parsing (#20)", () => {
  it("rejects an unknown flag after positional text", () => {
    const dir = makeSessionDir();
    const r = runCli(dir, "send", "somename", "hello world", "--bogus");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toContain("Unexpected argument");
    expect(r.stderr).toContain("--bogus");
  }, 10_000);

  it("suggests --seq key:return when --enter is used", () => {
    const dir = makeSessionDir();
    const r = runCli(dir, "send", "somename", "sudo cmd", "--enter");
    expect(r.status).not.toBe(0);
    expect(r.stderr).toContain("--enter");
    expect(r.stderr).toContain("--seq");
    expect(r.stderr).toContain("key:return");
  }, 10_000);

  it("suggests the real syntax for --newline and --return aliases too", () => {
    const dir = makeSessionDir();
    for (const flag of ["--newline", "--return", "--cr"]) {
      const r = runCli(dir, "send", "somename", "text", flag);
      expect(r.status).not.toBe(0);
      expect(r.stderr).toContain(flag);
      expect(r.stderr).toContain("key:return");
    }
  }, 15_000);

  it("plain positional text still works (regression guard)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    const dump = path.join(dir, "dump.bin");
    await startDumpSession(dir, name, dump);

    const r = runCli(dir, "send", name, "still-works");
    expect(r.status).toBe(0);

    const received = await waitForDump(dump, "still-works".length, 3000);
    expect(received).toBe("still-works");
  }, 15_000);
});
