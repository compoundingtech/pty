import { execSync } from "node:child_process";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

let runRoot: string | undefined;

function shortBaseTmp(): string {
  return process.platform === "win32" ? os.tmpdir() : "/tmp";
}

export async function setup(): Promise<void> {
  runRoot = fs.mkdtempSync(path.join(shortBaseTmp(), `pv-`));
  process.env.PTY_VITEST_RUN_ROOT = runRoot;
  process.env.TMPDIR = runRoot;
  // Existing tests set the legacy PTY_SESSION_DIR env for isolation;
  // silence its Phase-2 deprecation notice so test stderr stays clean.
  // A single dedicated test (tests/pty-root.test.ts) opts back in
  // by unsetting this in the spawned child's env.
  process.env.PTY_ROOT_LEGACY_SILENT = "1";
  process.stderr.write(`[vitest-global] runRoot=${runRoot}\n`);
}

function collectPidsHoldingUnder(root: string): Set<number> {
  const pids = new Set<number>();
  // macOS symlinks /tmp -> /private/tmp; a socket bound to /tmp/pv-X/sock
  // reports as /private/tmp/pv-X/sock in `lsof -U`.
  const prefixes = [root + "/", "/private" + root + "/"];

  // Sweep 1: files or directories under root (cwd, open files, mmaps).
  // Misses processes whose ONLY tie to root is a bound listen socket.
  let dTree = "";
  try {
    dTree = execSync(`lsof -Fpn +D "${root}" 2>/dev/null || true`, {
      encoding: "utf8",
      maxBuffer: 32 * 1024 * 1024,
    });
  } catch {
    /* lsof exits non-zero on no matches */
  }
  for (const line of dTree.split("\n")) {
    if (line.startsWith("p")) {
      const n = Number(line.slice(1));
      if (Number.isFinite(n) && n !== process.pid && n > 1) pids.add(n);
    }
  }

  // Sweep 2: unix-domain sockets. `+D` does not index socket-name entries,
  // so a daemon holding only its listen socket inside root is invisible to
  // sweep 1. Walk `lsof -U` and match sockets by name prefix.
  let uSockets = "";
  try {
    uSockets = execSync(`lsof -Fpn -U 2>/dev/null || true`, {
      encoding: "utf8",
      maxBuffer: 64 * 1024 * 1024,
    });
  } catch {
    /* same */
  }
  let curPid: number | undefined;
  for (const line of uSockets.split("\n")) {
    if (line.startsWith("p")) {
      const n = Number(line.slice(1));
      curPid = Number.isFinite(n) ? n : undefined;
    } else if (line.startsWith("n") && curPid !== undefined) {
      const name = line.slice(1);
      if (prefixes.some((p) => name.startsWith(p))) {
        if (curPid !== process.pid && curPid > 1) pids.add(curPid);
      }
    }
  }
  return pids;
}

export async function teardown(): Promise<void> {
  if (!runRoot) return;
  const root = runRoot;

  const pids = collectPidsHoldingUnder(root);

  const pidArr = Array.from(pids);
  if (pidArr.length > 0) {
    process.stderr.write(`[vitest-global] SIGTERM ${pidArr.length} leaked pids\n`);
    for (const pid of pidArr) {
      try {
        process.kill(pid, "SIGTERM");
      } catch {
        // already gone
      }
    }
    await new Promise((r) => setTimeout(r, 2000));
    const survivors: number[] = [];
    for (const pid of pidArr) {
      try {
        process.kill(pid, 0);
        survivors.push(pid);
      } catch {
        // exited
      }
    }
    if (survivors.length > 0) {
      process.stderr.write(`[vitest-global] SIGKILL ${survivors.length} survivors\n`);
      for (const pid of survivors) {
        try {
          process.kill(pid, "SIGKILL");
        } catch {
          // gone between poll and kill
        }
      }
      await new Promise((r) => setTimeout(r, 500));
    }
  }

  try {
    fs.rmSync(root, { recursive: true, force: true, maxRetries: 3 });
  } catch {
    // best-effort; something inside may still be busy briefly
  }
}
