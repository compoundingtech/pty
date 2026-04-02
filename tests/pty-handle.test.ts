import { describe, it, expect, afterEach } from "vitest";
import { createPty, type PtyHandle } from "../src/tui/index.ts";

const handles: PtyHandle[] = [];

function spawn(
  command: string,
  args: string[] = [],
  opts?: { cols?: number; rows?: number; scrollback?: number },
): PtyHandle {
  const handle = createPty(command, args, {
    cols: opts?.cols ?? 80,
    rows: opts?.rows ?? 24,
    scrollback: opts?.scrollback,
  });
  handles.push(handle);
  return handle;
}

/** Wait for the handle's onActivity to fire. */
function waitForActivity(handle: PtyHandle, timeoutMs = 5000): Promise<void> {
  return new Promise((resolve, reject) => {
    const prev = handle.onActivity;
    const timer = setTimeout(() => {
      handle.onActivity = prev;
      reject(new Error(`Timed out waiting for activity after ${timeoutMs}ms`));
    }, timeoutMs);
    handle.onActivity = () => {
      clearTimeout(timer);
      handle.onActivity = prev;
      prev?.();
      resolve();
    };
  });
}

/** Wait until a predicate returns true by polling onActivity. */
async function waitFor(
  handle: PtyHandle,
  predicate: () => boolean,
  timeoutMs = 5000,
): Promise<void> {
  if (predicate()) return;
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    await waitForActivity(handle, deadline - Date.now());
    if (predicate()) return;
  }
  throw new Error(`waitFor timed out after ${timeoutMs}ms`);
}

/** Read the text content of a cell grid as joined lines. */
function cellsToText(cells: ReturnType<PtyHandle["readCells"]>): string {
  return cells.map(row => row.map(c => c.char).join("")).join("\n");
}

afterEach(() => {
  for (const h of handles) {
    try { h.kill(); } catch {}
  }
  handles.length = 0;
});

// --- cursorRow / cursorCol ---

describe("cursorRow and cursorCol", () => {
  it("reports initial cursor at (0, 0)", () => {
    const h = spawn("cat");
    expect(h.cursorRow).toBe(0);
    expect(h.cursorCol).toBe(0);
  });

  it("cursor moves when output is written", async () => {
    const h = spawn("bash", ["-c", "printf 'hello'"]);
    await waitFor(h, () => h.cursorCol > 0);
    expect(h.cursorRow).toBe(0);
    expect(h.cursorCol).toBe(5);
  });

  it("cursor row advances with newlines", async () => {
    const h = spawn("bash", ["-c", "printf 'a\\nb\\nc'"]);
    await waitFor(h, () => h.cursorRow >= 2);
    expect(h.cursorRow).toBe(2);
    expect(h.cursorCol).toBe(1);
  });
});

// --- mouseMode ---

describe("mouseMode", () => {
  it("is false by default", () => {
    const h = spawn("cat");
    expect(h.mouseMode).toBe(false);
  });

  it("becomes true when child enables mouse tracking (mode 1000)", async () => {
    const h = spawn("bash", ["-c", "printf '\\x1b[?1000h'; sleep 10"]);
    await waitFor(h, () => h.mouseMode === true);
    expect(h.mouseMode).toBe(true);
  });

  it("becomes false when child disables mouse tracking", async () => {
    const h = spawn("bash", ["-c", "printf '\\x1b[?1000h'; sleep 0.1; printf '\\x1b[?1000l'; sleep 10"]);
    await waitFor(h, () => h.mouseMode === true);
    await waitFor(h, () => h.mouseMode === false);
    expect(h.mouseMode).toBe(false);
  });

  it("tracks mode 1002 (button-motion)", async () => {
    const h = spawn("bash", ["-c", "printf '\\x1b[?1002h'; sleep 10"]);
    await waitFor(h, () => h.mouseMode === true);
    expect(h.mouseMode).toBe(true);
  });

  it("tracks mode 1003 (any-motion)", async () => {
    const h = spawn("bash", ["-c", "printf '\\x1b[?1003h'; sleep 10"]);
    await waitFor(h, () => h.mouseMode === true);
    expect(h.mouseMode).toBe(true);
  });
});

// --- scrollback ---

describe("scrollback", () => {
  it("defaults to 0", () => {
    const h = spawn("cat");
    expect(h.scrollback).toBe(0);
    expect(h.baseY).toBe(0);
  });

  it("respects configured scrollback", () => {
    const h = spawn("cat", [], { scrollback: 500 });
    expect(h.scrollback).toBe(500);
  });

  it("bufferLength equals rows when no scrollback content", () => {
    const h = spawn("cat", [], { rows: 10 });
    expect(h.bufferLength).toBe(10);
  });

  it("bufferLength grows as content pushes into scrollback", async () => {
    const h = spawn("bash", ["-c", "for i in $(seq 1 50); do echo line-$i; done; sleep 10"], {
      rows: 10,
      scrollback: 100,
    });
    await waitFor(h, () => h.baseY > 0);
    expect(h.bufferLength).toBeGreaterThan(10);
    expect(h.baseY).toBeGreaterThan(0);
  });

  it("baseY stays 0 with scrollback: 0 (old lines discarded)", async () => {
    const h = spawn("bash", ["-c", "for i in $(seq 1 50); do echo line-$i; done; sleep 10"], {
      rows: 10,
      scrollback: 0,
    });
    await waitFor(h, () => h.dirty);
    // Give it a moment to process all output
    await new Promise(r => setTimeout(r, 500));
    expect(h.baseY).toBe(0);
  });
});

// --- readCells with scrollOffset ---

describe("readCells", () => {
  it("returns viewport-sized grid", () => {
    const h = spawn("cat", [], { rows: 10, cols: 20 });
    const cells = h.readCells();
    expect(cells.length).toBe(10);
    expect(cells[0]!.length).toBe(20);
  });

  it("readCells(0) shows live viewport content", async () => {
    const h = spawn("bash", ["-c", "for i in $(seq 1 50); do echo line-$i; done; sleep 10"], {
      rows: 10,
      scrollback: 100,
    });
    await waitFor(h, () => h.baseY > 0);
    // Give time for all output
    await new Promise(r => setTimeout(r, 500));

    const text = cellsToText(h.readCells(0));
    // Live viewport should have recent lines (high numbers)
    expect(text).toMatch(/line-[4-5]\d/);
  });

  it("readCells with scrollOffset reads history", async () => {
    const h = spawn("bash", ["-c", "for i in $(seq 1 50); do echo line-$i; done; sleep 10"], {
      rows: 10,
      scrollback: 100,
    });
    await waitFor(h, () => h.baseY > 0);
    await new Promise(r => setTimeout(r, 500));

    // Scroll all the way back
    const cells = h.readCells(h.baseY);
    const text = cellsToText(cells);
    // Should see early lines
    expect(text).toMatch(/line-[1-9]\b/);
  });

  it("scrollOffset is clamped (does not crash for large values)", () => {
    const h = spawn("cat", [], { rows: 10, scrollback: 100 });
    const cells = h.readCells(99999);
    expect(cells.length).toBe(10);
  });

  it("readCells without scrollback returns current content", async () => {
    const h = spawn("bash", ["-c", "echo hello-world; sleep 10"], {
      rows: 10,
      scrollback: 0,
    });
    await waitFor(h, () => {
      const text = cellsToText(h.readCells());
      return text.includes("hello-world");
    });
    const text = cellsToText(h.readCells());
    expect(text).toContain("hello-world");
  });
});
