import { describe, it, expect, afterEach } from "vitest";
import {
  createPty, type PtyHandle,
  CellBuffer, themes,
  renderPtyPane, ptyPaneInnerRect, isSelectedInPane,
} from "../src/tui/index.ts";

const theme = themes.coolBlue!;
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

/** Find the (row, col) of the first cell in the buffer whose char matches. */
function findInBuffer(buf: CellBuffer, ch: string): { row: number; col: number } | null {
  for (let r = 0; r < buf.rows; r++) {
    for (let c = 0; c < buf.cols; c++) {
      if (buf.getCell(r, c)?.char === ch) return { row: r, col: c };
    }
  }
  return null;
}

const ROUNDED_CORNERS = new Set(["╭", "╮", "╰", "╯"]); // ╭ ╮ ╰ ╯

afterEach(() => {
  for (const h of handles) {
    try { h.kill(); } catch {}
  }
  handles.length = 0;
});

describe("ptyPaneInnerRect", () => {
  it("insets by one on every side when chrome is on", () => {
    expect(ptyPaneInnerRect({ x: 0, y: 0, width: 20, height: 6 }, true))
      .toEqual({ x: 1, y: 1, width: 18, height: 4 });
  });

  it("is the whole rect when chrome is off", () => {
    expect(ptyPaneInnerRect({ x: 3, y: 2, width: 10, height: 5 }, false))
      .toEqual({ x: 3, y: 2, width: 10, height: 5 });
  });

  it("clamps inner size to zero for a rect too small for chrome", () => {
    expect(ptyPaneInnerRect({ x: 0, y: 0, width: 1, height: 1 }, true))
      .toEqual({ x: 1, y: 1, width: 0, height: 0 });
  });
});

describe("isSelectedInPane", () => {
  const sel = { startRow: 1, startCol: 2, endRow: 3, endCol: 4, scrollOffset: 0 };

  it("includes the interior of the selection", () => {
    expect(isSelectedInPane(2, 0, sel, 0)).toBe(true); // full middle row
  });

  it("respects start/end columns on the edge rows", () => {
    expect(isSelectedInPane(1, 1, sel, 0)).toBe(false); // before start col
    expect(isSelectedInPane(1, 2, sel, 0)).toBe(true);  // at start col
    expect(isSelectedInPane(3, 4, sel, 0)).toBe(true);  // at end col
    expect(isSelectedInPane(3, 5, sel, 0)).toBe(false); // past end col
  });

  it("translates the highlight by the scroll delta so it tracks content", () => {
    // Captured at offset 0; now viewing offset 1 → content moved down a row,
    // so the row-2 selection is now checked against screen row 3.
    expect(isSelectedInPane(3, 0, sel, 1)).toBe(true);
    expect(isSelectedInPane(2, 0, sel, 1)).toBe(false);
  });
});

describe("renderPtyPane", () => {
  it("draws a rounded border with a title around the content", async () => {
    const h = spawn("bash", ["-c", "printf HELLO; sleep 10"]);
    await waitFor(h, () => h.cursorCol >= 5);

    const buf = new CellBuffer(10, 30);
    const res = renderPtyPane(buf, { x: 0, y: 0, width: 20, height: 6 }, h, {
      theme, title: "term", focused: true,
    });

    // Corner of the box.
    expect(ROUNDED_CORNERS.has(buf.getCell(0, 0)?.char ?? "")).toBe(true);
    // Title text rendered into the top border row.
    const titleRow = Array.from({ length: 20 }, (_, c) => buf.getCell(0, c)?.char ?? "").join("");
    expect(titleRow).toContain("term");
    // Content blitted into the inner area (inset by 1,1).
    const hit = findInBuffer(buf, "H");
    expect(hit).toEqual({ row: 1, col: 1 });
    expect(res.inner).toEqual({ x: 1, y: 1, width: 18, height: 4 });
  });

  it("blits at the rect origin with no border when chrome is off", async () => {
    const h = spawn("bash", ["-c", "printf HELLO; sleep 10"]);
    await waitFor(h, () => h.cursorCol >= 5);

    const buf = new CellBuffer(10, 30);
    renderPtyPane(buf, { x: 0, y: 0, width: 20, height: 5 }, h, { theme, chrome: false });

    expect(ROUNDED_CORNERS.has(buf.getCell(0, 0)?.char ?? "")).toBe(false);
    expect(findInBuffer(buf, "H")).toEqual({ row: 0, col: 0 });
  });

  it("preserves the palette index of blitted cells (SGR 34 → fgIndex 4)", async () => {
    const h = spawn("bash", ["-c", "printf '\\x1b[34mB\\x1b[0m'; sleep 10"]);
    await waitFor(h, () => {
      const cells = h.readCells();
      return cells.some(row => row.some(c => c.char === "B"));
    });

    const buf = new CellBuffer(6, 20);
    renderPtyPane(buf, { x: 0, y: 0, width: 10, height: 3 }, h, { theme, chrome: false });

    const hit = findInBuffer(buf, "B")!;
    expect(buf.getCell(hit.row, hit.col)?.fgIndex).toBe(4);
  });

  it("reports the focused cursor position (1-based, offset by chrome)", async () => {
    const h = spawn("bash", ["-c", "printf hi; sleep 10"]);
    await waitFor(h, () => h.cursorCol >= 2);

    const buf = new CellBuffer(10, 30);
    const res = renderPtyPane(buf, { x: 0, y: 0, width: 20, height: 6 }, h, {
      theme, focused: true,
    });
    // inner origin is (1,1); cursor at row 0 col 2 → 1-based (2, 4).
    expect(res.cursor).toEqual({ row: 2, col: 4 });
  });

  it("reports no cursor for an unfocused pane", async () => {
    const h = spawn("bash", ["-c", "printf hi; sleep 10"]);
    await waitFor(h, () => h.cursorCol >= 2);

    const buf = new CellBuffer(10, 30);
    const res = renderPtyPane(buf, { x: 0, y: 0, width: 20, height: 6 }, h, {
      theme, focused: false,
    });
    expect(res.cursor).toBeNull();
  });

  it("hides the cursor when it is scrolled off-screen", async () => {
    const h = spawn("bash", ["-c", "printf hi; sleep 10"], { scrollback: 100 });
    await waitFor(h, () => h.cursorCol >= 2);

    const buf = new CellBuffer(10, 30);
    // inner height is 4; a scrollOffset of 10 pushes the cursor well past it.
    const res = renderPtyPane(buf, { x: 0, y: 0, width: 20, height: 6 }, h, {
      theme, focused: true, scrollOffset: 10,
    });
    expect(res.cursor).toBeNull();
  });

  it("inverts fg/bg (and their palette indices) for a selected cell", async () => {
    // Red foreground (SGR 31 → fgIndex 1), default background.
    const h = spawn("bash", ["-c", "printf '\\x1b[31mR\\x1b[0m'; sleep 10"]);
    await waitFor(h, () => {
      const cells = h.readCells();
      return cells.some(row => row.some(c => c.char === "R"));
    });

    const buf = new CellBuffer(6, 20);
    renderPtyPane(buf, { x: 0, y: 0, width: 10, height: 3 }, h, {
      theme, chrome: false,
      selection: { startRow: 0, startCol: 0, endRow: 0, endCol: 3, scrollOffset: 0 },
    });

    const hit = findInBuffer(buf, "R")!;
    const cell = buf.getCell(hit.row, hit.col)!;
    // The original fg index (1) is swapped into the background.
    expect(cell.bgIndex).toBe(1);
    expect(cell.fgIndex).toBeNull();
  });
});
