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
  process.stderr.write(`[vitest-global] runRoot=${runRoot}\n`);
}

export async function teardown(): Promise<void> {
  if (!runRoot) return;
  const root = runRoot;

  let pidLines = "";
  try {
    pidLines = execSync(`lsof -Fpn +D "${root}" 2>/dev/null || true`, {
      encoding: "utf8",
      maxBuffer: 32 * 1024 * 1024,
    });
  } catch {
    // lsof may exit non-zero when no matches — treat as empty.
  }

  const pids = new Set<number>();
  for (const line of pidLines.split("\n")) {
    if (line.startsWith("p")) {
      const n = Number(line.slice(1));
      if (Number.isFinite(n) && n !== process.pid && n > 1) pids.add(n);
    }
  }

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
