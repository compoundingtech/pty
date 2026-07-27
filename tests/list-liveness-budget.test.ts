import { afterEach, describe, expect, it, vi } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { isProcessAlive, listSessions } from "../src/sessions.ts";

const roots: string[] = [];

function makeRoot(): string {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "pty-list-budget-"));
  roots.push(root);
  process.env.PTY_ROOT = root;
  return root;
}

function errno(code: string): NodeJS.ErrnoException {
  const error = new Error(code) as NodeJS.ErrnoException;
  error.code = code;
  return error;
}

afterEach(() => {
  vi.restoreAllMocks();
  delete process.env.PTY_ROOT;
  for (const root of roots.splice(0)) {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

describe("process liveness under restricted seats", () => {
  it("treats POSIX kill(pid, 0) EPERM as process-present", () => {
    if (process.platform === "win32") return;
    vi.spyOn(process, "kill").mockImplementation(() => {
      throw errno("EPERM");
    });
    expect(isProcessAlive(12345)).toBe(true);
  });

  it("does not socket-probe a permission-denied fleet and returns name order", async () => {
    if (process.platform === "win32") return;
    const root = makeRoot();
    const names = Array.from({ length: 80 }, (_, i) => `seat-${String(79 - i).padStart(3, "0")}`);
    for (const name of names) {
      fs.writeFileSync(path.join(root, `${name}.sock`), "");
      fs.writeFileSync(path.join(root, `${name}.pid`), "12345");
    }
    vi.spyOn(process, "kill").mockImplementation(() => {
      throw errno("EPERM");
    });
    const socketProbe = vi.fn(async () => false);

    const sessions = await listSessions({ socketProbe, socketProbeBudgetMs: 20 });

    expect(socketProbe).not.toHaveBeenCalled();
    expect(sessions.map((session) => session.name)).toEqual([...names].sort());
    expect(sessions.every((session) => session.status === "running")).toBe(true);
  });
});

describe("fleet-wide socket fallback budget", () => {
  it("starts every fallback concurrently but waits only one shared deadline", async () => {
    const root = makeRoot();
    const names = Array.from({ length: 100 }, (_, i) => `unreachable-${String(99 - i).padStart(3, "0")}`);
    for (const name of names) {
      // No pidfile: an unreadable pid plus an unreachable socket is reported
      // defensively as running, but must not serialize 100 timeout waits.
      fs.writeFileSync(path.join(root, `${name}.sock`), "");
    }
    const socketProbe = vi.fn(() => new Promise<boolean>(() => {}));
    const startedAt = Date.now();

    const sessions = await listSessions({ socketProbe, socketProbeBudgetMs: 25 });
    const elapsed = Date.now() - startedAt;

    expect(socketProbe).toHaveBeenCalledTimes(names.length);
    expect(elapsed).toBeLessThan(250);
    expect(sessions.map((session) => session.name)).toEqual([...names].sort());
  });
});
