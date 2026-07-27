import { waitForProcessExit } from "../../src/sessions.ts";

/** Terminate detached test daemons and wait until none can write into their
 * temporary roots. Hooks must await this before clearing PID tracking or
 * recursively removing session directories. */
export async function terminateAndWait(
  pids: Iterable<number>,
  signal: NodeJS.Signals = "SIGTERM",
  timeoutMs = 7_000,
): Promise<void> {
  const uniquePids = Array.from(new Set(pids));
  for (const pid of uniquePids) {
    try { process.kill(pid, signal); } catch {}
  }
  const exited = await Promise.all(
    uniquePids.map((pid) => waitForProcessExit(pid, timeoutMs)),
  );
  const survivors = uniquePids.filter((_, index) => !exited[index]);
  if (survivors.length > 0) {
    throw new Error(
      `detached test daemons did not exit within ${timeoutMs}ms: ${survivors.join(", ")}`,
    );
  }
}
