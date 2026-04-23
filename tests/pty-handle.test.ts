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

// --- alternateScreen ---

describe("alternateScreen", () => {
  it("is false by default (primary buffer)", () => {
    const h = spawn("cat");
    expect(h.alternateScreen).toBe(false);
  });

  it("becomes true when child enters alternate screen (mode 1049)", async () => {
    const h = spawn("bash", ["-c", "printf '\\x1b[?1049h'; sleep 10"]);
    await waitFor(h, () => h.alternateScreen === true);
    expect(h.alternateScreen).toBe(true);
  });

  it("becomes false when child leaves alternate screen", async () => {
    const h = spawn("bash", ["-c", "printf '\\x1b[?1049h'; sleep 0.1; printf '\\x1b[?1049l'; sleep 10"]);
    await waitFor(h, () => h.alternateScreen === true);
    await waitFor(h, () => h.alternateScreen === false);
    expect(h.alternateScreen).toBe(false);
  });
});

// --- kittyKeyboardFlags ---

describe("kittyKeyboardFlags", () => {
  it("is empty by default", () => {
    const h = spawn("cat");
    expect(h.kittyKeyboardFlags).toEqual([]);
  });

  it("tracks a single pushed flag", async () => {
    const h = spawn("bash", ["-c", "printf '\\x1b[>7u'; sleep 10"]);
    await waitFor(h, () => h.kittyKeyboardFlags.length > 0);
    expect(h.kittyKeyboardFlags).toEqual([7]);
  });

  it("tracks multiple nested pushes", async () => {
    const h = spawn("bash", ["-c", "printf '\\x1b[>1u\\x1b[>15u'; sleep 10"]);
    await waitFor(h, () => h.kittyKeyboardFlags.length === 2);
    expect(h.kittyKeyboardFlags).toEqual([1, 15]);
  });

  it("pops the most recent flag on CSI < u", async () => {
    // Sequences land together — just wait for the final state after the pop.
    const h = spawn("bash", ["-c", "printf '\\x1b[>1u\\x1b[>15u\\x1b[<u'; sleep 10"]);
    await waitFor(h, () => h.kittyKeyboardFlags.length === 1);
    expect(h.kittyKeyboardFlags).toEqual([1]);
  });

  it("returns a defensive copy (mutation does not affect internal state)", async () => {
    const h = spawn("bash", ["-c", "printf '\\x1b[>7u'; sleep 10"]);
    await waitFor(h, () => h.kittyKeyboardFlags.length > 0);
    const snapshot = h.kittyKeyboardFlags;
    snapshot.push(999);
    expect(h.kittyKeyboardFlags).toEqual([7]);
  });
});

// --- readWrappedFlags ---

describe("readWrappedFlags", () => {
  it("returns one flag per visible row", () => {
    const h = spawn("cat", [], { rows: 10, cols: 40 });
    const flags = h.readWrappedFlags();
    expect(flags).toHaveLength(10);
    expect(flags.every((f) => typeof f === "boolean")).toBe(true);
  });

  it("marks continuation rows as wrapped when a long line overflows", async () => {
    // 120-char line at 40-col terminal → wraps to 3 rows. First row is
    // not wrapped (it's the start of the line); the next two rows are
    // continuations and should both be flagged.
    const h = spawn("bash", ["-c", "printf '%0.sa' {1..120}; sleep 5"], {
      rows: 12, cols: 40,
    });
    await waitFor(h, () => h.readWrappedFlags().some((f) => f === true));
    const flags = h.readWrappedFlags();
    // First row: start of the logical line, not a wrap continuation.
    expect(flags[0]).toBe(false);
    // Next two rows: wrapped continuations.
    expect(flags[1]).toBe(true);
    expect(flags[2]).toBe(true);
    // Remaining empty rows after the wrapped content: not wrapped.
    expect(flags[3]).toBe(false);
  });

  it("short lines followed by a newline produce no wrapped flags", async () => {
    const h = spawn("bash", ["-c", "printf 'short\\n'; sleep 5"], {
      rows: 8, cols: 40,
    });
    await waitFor(h, () => h.cursorRow >= 1);
    const flags = h.readWrappedFlags();
    expect(flags.every((f) => f === false)).toBe(true);
  });

  it("scrollOffset shifts the window in sync with readCells", async () => {
    // Emit many short lines to push earlier content into scrollback,
    // then read with a nonzero offset and confirm flags array length
    // still matches readCells rows.
    const h = spawn("bash", ["-c", "for i in $(seq 1 30); do echo line $i; done; sleep 5"], {
      rows: 10, cols: 40, scrollback: 100,
    });
    await waitFor(h, () => h.bufferLength > h.rows);
    const flags0 = h.readWrappedFlags(0);
    const cells0 = h.readCells(0);
    expect(flags0.length).toBe(cells0.length);

    const flags5 = h.readWrappedFlags(5);
    const cells5 = h.readCells(5);
    expect(flags5.length).toBe(cells5.length);
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

// --- Palette-indexed color preservation (fgIndex / bgIndex) ---
//
// Regression guard for pty-layout: readCells used to flatten palette
// cells to a hardcoded VGA RGB via paletteToRgb(), which meant
// consumers that re-emit cells to a real terminal lost the outer
// terminal's theme (SGR 34 became [0,0,204] always, even in kitty
// with a blue theme). Cells now also carry `fgIndex` / `bgIndex`
// so re-emitters can produce SGR 30-37 / 38;5;N and let the outer
// terminal's palette win.

// Find the first cell whose char matches `ch` and return it. Used
// because the cursor trails the emitted output and the SGR attrs
// only live on the printed chars, not the cursor row.
function findCellByChar(
  cells: ReturnType<PtyHandle["readCells"]>,
  ch: string,
): ReturnType<PtyHandle["readCells"]>[0][0] | null {
  for (const row of cells) {
    for (const cell of row) {
      if (cell.char === ch) return cell;
    }
  }
  return null;
}

describe("readCells — palette-indexed colors", () => {
  it("preserves low-palette fg (SGR 34 → fgIndex=4)", async () => {
    const h = spawn("bash", ["-c", "printf '\\x1b[34mB\\x1b[0m'; sleep 10"]);
    await waitFor(h, () => findCellByChar(h.readCells(), "B") !== null);
    const cell = findCellByChar(h.readCells(), "B")!;
    expect(cell.fgIndex).toBe(4);
    // `fg` is still populated for back-compat — consumers that don't
    // know about fgIndex fall back to the flattened RGB.
    expect(cell.fg).not.toBeNull();
  });

  it("preserves bright-palette fg (SGR 94 → fgIndex=12)", async () => {
    const h = spawn("bash", ["-c", "printf '\\x1b[94mX\\x1b[0m'; sleep 10"]);
    await waitFor(h, () => findCellByChar(h.readCells(), "X") !== null);
    const cell = findCellByChar(h.readCells(), "X")!;
    expect(cell.fgIndex).toBe(12);
  });

  it("preserves 256-palette fg (SGR 38;5;17 → fgIndex=17)", async () => {
    const h = spawn("bash", ["-c", "printf '\\x1b[38;5;17mY\\x1b[0m'; sleep 10"]);
    await waitFor(h, () => findCellByChar(h.readCells(), "Y") !== null);
    const cell = findCellByChar(h.readCells(), "Y")!;
    expect(cell.fgIndex).toBe(17);
  });

  it("preserves 256-palette bg (SGR 48;5;124 → bgIndex=124)", async () => {
    const h = spawn("bash", ["-c", "printf '\\x1b[48;5;124mZ\\x1b[0m'; sleep 10"]);
    await waitFor(h, () => findCellByChar(h.readCells(), "Z") !== null);
    const cell = findCellByChar(h.readCells(), "Z")!;
    expect(cell.bgIndex).toBe(124);
  });

  it("truecolor RGB leaves fgIndex null (cell is not indexed)", async () => {
    const h = spawn("bash", ["-c", "printf '\\x1b[38;2;10;20;30mT\\x1b[0m'; sleep 10"]);
    await waitFor(h, () => findCellByChar(h.readCells(), "T") !== null);
    const cell = findCellByChar(h.readCells(), "T")!;
    expect(cell.fgIndex).toBeNull();
    expect(cell.fg).toEqual([10, 20, 30]);
  });

  it("default-color cells have both fg=null and fgIndex=null", async () => {
    const h = spawn("bash", ["-c", "printf 'D'; sleep 10"]);
    await waitFor(h, () => findCellByChar(h.readCells(), "D") !== null);
    const cell = findCellByChar(h.readCells(), "D")!;
    expect(cell.fg).toBeNull();
    expect(cell.fgIndex).toBeNull();
    expect(cell.bg).toBeNull();
    expect(cell.bgIndex).toBeNull();
  });

  it("fg and bg indices are tracked independently", async () => {
    const h = spawn("bash", ["-c", "printf '\\x1b[31;42mM\\x1b[0m'; sleep 10"]);
    await waitFor(h, () => findCellByChar(h.readCells(), "M") !== null);
    const cell = findCellByChar(h.readCells(), "M")!;
    expect(cell.fgIndex).toBe(1); // SGR 31
    expect(cell.bgIndex).toBe(2); // SGR 42
  });
});
