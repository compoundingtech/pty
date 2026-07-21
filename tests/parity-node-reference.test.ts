// PARITY DRIVE — locks node pty's REFERENCE behavior so the Rust port
// (pty-rust) can match it EXACTLY. Node is the reference; every assertion here
// pins node's OBSERVED behavior (each value was captured empirically against
// the real dist/cli.js + dist/server.js, then frozen). pty-rust must produce
// byte-identical output for the same inputs — this file IS the shared
// behavioral spec both implementations must pass.
//
// Round 1 covers the 4 divergences the nesting verification surfaced:
//   1. post-exit peek preserves the final screen after a session exits
//   2. plain peek keeps the bash-prompt trailing cursor-cell blank
//   3. send --seq input pacing (canonical 300ms inter-item default)
//   4. the -d / --force CLI nesting-guard behavior
//
// The CLI-level tests drive the real binary exactly the way pty-rust is
// exercised (spawn a daemon, then run `pty <verb>` against it); the pacing
// unit test pins the canonical constant directly.

import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, spawnSync } from "node:child_process";
import { resolveSeqDelayMs, DEFAULT_SEQ_DELAY_MS } from "../src/client.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const serverModule = path.join(__dirname, "..", "dist", "server.js");

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-parity-"));
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
  return `par${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
}

// Spawn a detached daemon (the same server module the CLI daemonizes) running
// `command`, and wait until its control socket exists. Mirrors the harness used
// by peek-wait.test.ts / nesting.test.ts.
async function startDaemon(
  sessionDir: string,
  name: string,
  command: string,
  args: string[] = [],
  env: Record<string, string> = {},
): Promise<number> {
  const config = JSON.stringify({
    name, command, args, displayCommand: command,
    cwd: os.tmpdir(), rows: 24, cols: 80,
  });
  const child = spawn(nodeBin, [serverModule], {
    detached: true,
    stdio: ["ignore", "ignore", "pipe"],
    env: { ...process.env, PTY_SERVER_CONFIG: config, PTY_SESSION_DIR: sessionDir, ...env },
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
  throw new Error(`Timeout waiting for daemon socket: ${socketPath}`);
}

// Run the real `pty` CLI against a session dir. `env` overlays extra vars
// (e.g. PTY_SESSION to simulate a nested caller).
function runCli(
  sessionDir: string,
  args: string[],
  env: Record<string, string | undefined> = {},
) {
  return spawnSync(nodeBin, [cliPath, ...args], {
    env: { ...process.env, PTY_SESSION_DIR: sessionDir, ...env },
    encoding: "utf-8",
    timeout: 15000,
  });
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

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

// ---------------------------------------------------------------------------
// #1 — post-exit peek, under BOTH exit-behavior modes
// ---------------------------------------------------------------------------
// Exit-time reaping is configurable (`PTY_REAP_ON_EXIT`; shipped default REAP).
// The parity contract for post-exit peek therefore has TWO reference modes both
// implementations must match:
//   * preserve (PTY_REAP_ON_EXIT=false): the finished session is kept, and peek
//     returns the exact final viewport, idempotently.
//   * reap (default): the finished session removes itself, and peek reports it
//     is gone (exit non-zero / "not found"), with no registry entry left.
const PRESERVE_ENV = { PTY_REAP_ON_EXIT: "false" };

describe("parity #1: post-exit peek — preserve vs reap modes", () => {
  it("preserve mode: returns the exact final viewport after exit, idempotently", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    // Three lines, DONE with no trailing newline, then exit 7.
    await startDaemon(dir, name, "sh", ["-c", 'printf "LINE_A\\nLINE_B\\nDONE"; exit 7'], PRESERVE_ENV);
    await sleep(1200); // let it exit + persist the final screen

    const first = runCli(dir, ["peek", "--plain", name]);
    expect(first.status).toBe(0);
    // The final rendered viewport survives verbatim (trailing newline is the
    // CLI's line terminator around the screen payload).
    expect(first.stdout.replace(/\n$/, "")).toBe("LINE_A\nLINE_B\nDONE");

    // Peeking does NOT consume/clear the preserved screen: a second peek is
    // byte-identical to the first.
    const second = runCli(dir, ["peek", "--plain", name]);
    expect(second.status).toBe(0);
    expect(second.stdout).toBe(first.stdout);

    // The registry records the session as exited with its real exit code.
    const list = JSON.parse(runCli(dir, ["list", "--json"]).stdout);
    const found = list.find((s: any) => s.name === name);
    expect(found).toBeDefined();
    expect(found.status).toBe("exited");
    expect(found.exitCode).toBe(7);
  }, 20000);

  it("preserve mode: non-plain (ANSI) peek preserves the same content after exit", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "sh", ["-c", 'printf "ALPHA\\nBETA"; exit 0'], PRESERVE_ENV);
    await sleep(1200);

    const ansi = runCli(dir, ["peek", name]);
    expect(ansi.status).toBe(0);
    expect(ansi.stdout).toContain("ALPHA");
    expect(ansi.stdout).toContain("BETA");
  }, 20000);

  it("reap mode (default): the finished session reaps itself — peek reports it gone", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    // No PTY_REAP_ON_EXIT → shipped default REAP: the daemon removes its own
    // registry entry as it exits, so there is nothing left to peek.
    await startDaemon(dir, name, "sh", ["-c", 'printf "GONE"; exit 0']);
    await sleep(1200);

    const peek = runCli(dir, ["peek", "--plain", name]);
    expect(peek.status).not.toBe(0);
    // And the registry no longer lists it.
    const list = JSON.parse(runCli(dir, ["list", "--json"]).stdout);
    expect(list.find((s: any) => s.name === name)).toBeUndefined();
  }, 20000);
});

// ---------------------------------------------------------------------------
// #2 — plain peek keeps the trailing cursor-cell blank
// ---------------------------------------------------------------------------
// Node's getPlainScreen() serializes each row via xterm translateToString with
// trimRight=true, which trims ONLY never-written (null) cells. A space that the
// program explicitly wrote — e.g. the blank a shell prompt emits before the
// cursor ("READY> ") — is a real cell and is PRESERVED. pty-rust must not
// blanket-rstrip rows; it must preserve explicitly-written trailing spaces and
// trim only unwritten cells. We use a printf prompt so the expected bytes are
// deterministic and shell-version independent.
describe("parity #2: plain peek keeps the trailing cursor-cell blank", () => {
  it("preserves an explicitly-written trailing space (prompt cursor cell)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    await startDaemon(dir, name, "sh", ["-c", "printf 'READY> '; exec cat"]);
    await sleep(700);

    const result = runCli(dir, ["peek", "--plain", name]);
    expect(result.status).toBe(0);

    const line = result.stdout.replace(/\n$/, "");
    // The trailing space (the cell the cursor sits on) is KEPT.
    expect(line).toBe("READY> ");
    // Guard against a blanket right-trim — that would drop the cursor cell.
    expect(line).not.toBe("READY>");
    // And it is NOT padded out to the full column width: only written cells
    // survive, unwritten cells past the cursor are trimmed.
    expect(line.length).toBe(7);
  }, 20000);
});

// ---------------------------------------------------------------------------
// #3 — send --seq input pacing (canonical 300ms inter-item default)
// ---------------------------------------------------------------------------
// CANONICAL DECISION (node reference, to be matched by pty-rust — confirmed
// with pty-rust-claude on the bus): the default spacing between `--seq` ITEMS
// is 300ms. It is applied between items only (never before the first, never
// after the last — see send() in client.ts: the delay is gated on `i > 0`) and
// is NOT per-character — a single `--seq "git status"` is one write.
// `--with-delay <sec>` overrides it; `--with-delay 0` is the straight-stream
// escape hatch. Intent: a trailing `key:return` mustn't race ahead of the
// program parsing the typed text. (The "~428ms/key" figure some measurements
// show = 300ms intentional spacing + ~128ms node-startup/socket-connect
// overhead measured end-to-end; the intentional, spec-worthy value is 300ms.)
//
// The full #3 spec — the canonical constant, the --with-delay override, the
// 0=straight-stream escape hatch, AND the end-to-end proof that the gap is
// actually applied — is ALREADY locked by tests/seq-delay.test.ts. That file is
// the authoritative #3 parity spec; pty-rust should mirror it. We only add here
// the one edge it doesn't cover: the ms-conversion rounding.
describe("parity #3: send --seq pacing — canonical 300ms inter-item default", () => {
  it("the canonical default is 300ms (spec anchor; full spec in seq-delay.test.ts)", () => {
    expect(DEFAULT_SEQ_DELAY_MS).toBe(300);
    expect(resolveSeqDelayMs(undefined)).toBe(DEFAULT_SEQ_DELAY_MS);
  });

  it("seconds -> ms conversion rounds (Math.round)", () => {
    // Not covered by seq-delay.test.ts's clean multiples: 0.4285s -> 428.5ms
    // -> 429ms. pty-rust must round the same way, not truncate.
    expect(resolveSeqDelayMs(0.4285)).toBe(429);
    expect(resolveSeqDelayMs(0.0001)).toBe(0); // 0.1ms -> rounds to 0
  });
});

// ---------------------------------------------------------------------------
// #4 — -d / --force nesting-guard behavior
// ---------------------------------------------------------------------------
// Node's nesting-guard semantics inside a session:
//   * `pty run` runs the command DIRECTLY and creates NO session UNLESS one of
//     two escape hatches is given: `-d`/`--detach` (create a background session
//     and return) or `--force` (create a nested session and attach to it).
//   * `pty attach` / `restart` / bare `pty` inside a session ERROR (exit 1);
//     `--force` DOES bypass those.
//
// Most of this matrix is ALREADY the authoritative node spec in
// tests/nesting-prevention.test.ts (attach refuse + --force bypass, run -a
// refuse/fall-through, plain run-nested runs-directly, restart, interactive)
// and tests/nesting.test.ts (`-d` bypasses, exit-code propagation). pty-rust
// should mirror those files. The case we pin HERE is the R1 divergence the
// nesting review surfaced and CoS ruled on (decision a, 2026-07-21): plain
// `run --force` (no -a, no -d) CREATES a nested session — `--force` bypasses
// run's guard the same way it bypasses attach's/restart's, matching pty's own
// `run --help` ("Create even from inside another pty session"). Node code was
// the outlier (it used to treat `--force` as a no-op on `run` and still run
// in-place); it now matches the docs + rust. pty-rust moves in lockstep.
const nested = { PTY_SESSION: "outer-session" };

describe("parity #4: -d / --force nesting-guard behavior", () => {
  it("plain `run --force` (no -a, no -d) CREATES a nested session (bypasses run's guard)", async () => {
    const dir = makeSessionDir();
    const name = uniqueName();
    // Nested caller + --force + a long-lived command. Old behavior: --force was
    // a no-op, the command ran directly in-place and NO session was created.
    // New behavior: --force bypasses run's guard, so a real session is created
    // (and, being interactive/no -d, attached). We launch it detached and poll
    // the registry until the session shows up RUNNING — proof the guard was
    // bypassed and a session exists, which the old no-op path never produced.
    const child = spawn(nodeBin, [cliPath, "run", "--force", "--id", name, "--", "cat"], {
      detached: true,
      stdio: ["ignore", "ignore", "ignore"],
      env: { ...process.env, PTY_SESSION_DIR: dir, PTY_SESSION: "outer-session" },
    });
    child.unref();
    bgPids.push(child.pid!);

    let found: any;
    const start = Date.now();
    while (Date.now() - start < 8000) {
      const list = JSON.parse(runCli(dir, ["list", "--json"]).stdout);
      found = list.find((s: any) => s.name === name);
      if (found && found.status === "running") break;
      await sleep(150);
    }
    // A real session was created and is running (the daemon pid is a separate,
    // killable process — matches node's own list --json pid semantics).
    expect(found).toBeDefined();
    expect(found.status).toBe("running");
    expect(typeof found.pid).toBe("number");

    // Tear down the created session daemon (SIGTERM forwards to the child).
    if (found?.pid) { bgPids.push(found.pid); try { process.kill(found.pid, "SIGTERM"); } catch {} }
  }, 20000);
});
