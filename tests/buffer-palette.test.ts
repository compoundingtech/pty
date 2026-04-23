// Palette-index round-trip through the CellBuffer pipeline:
//   writeAnsi(indexed SGR) -> cell.fgIndex set -> fullRender emits
//   the same indexed SGR (not truecolor).
//
// Exists so consumers like pty-layout — which re-emit cells to a real
// terminal — can keep the outer terminal's theme instead of getting a
// flattened VGA RGB. See builders.ts PtyCell and buffer.ts emitFg/emitBg.

import { describe, it, expect } from "vitest";
import { CellBuffer, fullRender, diff } from "../src/tui/buffer.ts";
import { cellsEqual, emptyCell } from "../src/tui/types.ts";

function findCellByChar(buf: CellBuffer, ch: string) {
  for (const row of buf.cells) {
    for (const cell of row) {
      if (cell.char === ch) return cell;
    }
  }
  return null;
}

describe("writeAnsi preserves palette index", () => {
  it("SGR 30-37: low 8 colors → fgIndex 0-7, flattened fg still populated", () => {
    const buf = new CellBuffer(1, 20);
    buf.writeAnsi("\x1b[34mB\x1b[0m");
    const cell = findCellByChar(buf, "B")!;
    expect(cell.fgIndex).toBe(4);
    // fg stays populated for consumers that don't know about fgIndex.
    expect(cell.fg).not.toBeNull();
  });

  it("SGR 90-97: bright 8 colors → fgIndex 8-15", () => {
    const buf = new CellBuffer(1, 20);
    buf.writeAnsi("\x1b[94mX\x1b[0m");
    expect(findCellByChar(buf, "X")!.fgIndex).toBe(12);
  });

  it("SGR 38;5;N: 256-palette → fgIndex=N", () => {
    const buf = new CellBuffer(1, 20);
    buf.writeAnsi("\x1b[38;5;17mY\x1b[0m");
    expect(findCellByChar(buf, "Y")!.fgIndex).toBe(17);
  });

  it("SGR 40-47: bg low → bgIndex 0-7", () => {
    const buf = new CellBuffer(1, 20);
    buf.writeAnsi("\x1b[42mg\x1b[0m");
    expect(findCellByChar(buf, "g")!.bgIndex).toBe(2);
  });

  it("SGR 48;5;N: 256-palette bg → bgIndex=N", () => {
    const buf = new CellBuffer(1, 20);
    buf.writeAnsi("\x1b[48;5;124mR\x1b[0m");
    expect(findCellByChar(buf, "R")!.bgIndex).toBe(124);
  });

  it("SGR 38;2;r;g;b: truecolor → fgIndex=null, fg=RGB", () => {
    const buf = new CellBuffer(1, 20);
    buf.writeAnsi("\x1b[38;2;10;20;30mT\x1b[0m");
    const cell = findCellByChar(buf, "T")!;
    expect(cell.fgIndex).toBeNull();
    expect(cell.fg).toEqual([10, 20, 30]);
  });

  it("SGR 0 clears both color and index", () => {
    const buf = new CellBuffer(1, 20);
    buf.writeAnsi("\x1b[34mA\x1b[0mB");
    const a = findCellByChar(buf, "A")!;
    const b = findCellByChar(buf, "B")!;
    expect(a.fgIndex).toBe(4);
    expect(b.fgIndex).toBeNull();
    expect(b.fg).toBeNull();
  });

  it("SGR 39 clears fg index (but not bg)", () => {
    const buf = new CellBuffer(1, 20);
    buf.writeAnsi("\x1b[34;42mA\x1b[39mB");
    const b = findCellByChar(buf, "B")!;
    expect(b.fgIndex).toBeNull();
    expect(b.fg).toBeNull();
    expect(b.bgIndex).toBe(2);
  });

  it("SGR 49 clears bg index (but not fg)", () => {
    const buf = new CellBuffer(1, 20);
    buf.writeAnsi("\x1b[34;42mA\x1b[49mB");
    const b = findCellByChar(buf, "B")!;
    expect(b.bgIndex).toBeNull();
    expect(b.bg).toBeNull();
    expect(b.fgIndex).toBe(4);
  });

  it("truecolor after indexed clears the index", () => {
    const buf = new CellBuffer(1, 20);
    buf.writeAnsi("\x1b[34mA\x1b[38;2;1;2;3mB");
    const b = findCellByChar(buf, "B")!;
    expect(b.fgIndex).toBeNull();
    expect(b.fg).toEqual([1, 2, 3]);
  });
});

describe("fullRender emits indexed SGR (not truecolor) for palette cells", () => {
  it("SGR 34 round-trips as SGR 34, not 38;2", () => {
    const buf = new CellBuffer(1, 10);
    buf.writeAnsi("\x1b[34mhi");
    const out = fullRender(buf);
    expect(out).toContain("\x1b[34m");
    expect(out).not.toContain("\x1b[38;2;");
  });

  it("SGR 94 (bright blue) round-trips as SGR 94", () => {
    const buf = new CellBuffer(1, 10);
    buf.writeAnsi("\x1b[94mhi");
    const out = fullRender(buf);
    expect(out).toContain("\x1b[94m");
    expect(out).not.toContain("\x1b[38;2;");
  });

  it("SGR 38;5;17 round-trips as SGR 38;5;17", () => {
    const buf = new CellBuffer(1, 10);
    buf.writeAnsi("\x1b[38;5;17mhi");
    const out = fullRender(buf);
    expect(out).toContain("\x1b[38;5;17m");
    expect(out).not.toContain("\x1b[38;2;");
  });

  it("truecolor stays truecolor", () => {
    const buf = new CellBuffer(1, 10);
    buf.writeAnsi("\x1b[38;2;10;20;30mhi");
    const out = fullRender(buf);
    expect(out).toContain("\x1b[38;2;10;20;30m");
  });

  it("bg round-trips for low / bright / 256", () => {
    const low = new CellBuffer(1, 4);
    low.writeAnsi("\x1b[42mh");
    expect(fullRender(low)).toContain("\x1b[42m");

    const bright = new CellBuffer(1, 4);
    bright.writeAnsi("\x1b[102mh");
    expect(fullRender(bright)).toContain("\x1b[102m");

    const ext = new CellBuffer(1, 4);
    ext.writeAnsi("\x1b[48;5;124mh");
    expect(fullRender(ext)).toContain("\x1b[48;5;124m");
  });

  it("transition from indexed to truecolor emits the truecolor code", () => {
    const buf = new CellBuffer(1, 10);
    buf.writeAnsi("\x1b[34mA\x1b[38;2;10;20;30mB");
    const out = fullRender(buf);
    expect(out).toContain("\x1b[34m");
    expect(out).toContain("\x1b[38;2;10;20;30m");
  });

  it("transition from truecolor to indexed emits the indexed code", () => {
    const buf = new CellBuffer(1, 10);
    buf.writeAnsi("\x1b[38;2;10;20;30mA\x1b[34mB");
    const out = fullRender(buf);
    expect(out).toContain("\x1b[38;2;10;20;30m");
    expect(out).toContain("\x1b[34m");
  });
});

describe("cellsEqual compares palette index", () => {
  it("cells differing only by fgIndex are unequal", () => {
    const a = emptyCell();
    a.fgIndex = 4; a.fg = [0, 0, 204];
    const b = emptyCell();
    b.fgIndex = 12; b.fg = [0, 0, 204]; // same flattened RGB, different index
    expect(cellsEqual(a, b)).toBe(false);
  });

  it("cells with same fg but one indexed and one truecolor are unequal", () => {
    const a = emptyCell();
    a.fgIndex = 4; a.fg = [0, 0, 204];
    const b = emptyCell();
    b.fgIndex = null; b.fg = [0, 0, 204]; // same RGB, no index (truecolor)
    expect(cellsEqual(a, b)).toBe(false);
  });

  it("cells with identical fgIndex and fg are equal", () => {
    const a = emptyCell();
    a.fgIndex = 4; a.fg = [0, 0, 204];
    const b = emptyCell();
    b.fgIndex = 4; b.fg = [0, 0, 204];
    expect(cellsEqual(a, b)).toBe(true);
  });
});

describe("diff respects index transitions", () => {
  it("re-emits SGR when only the index changes (same flattened RGB)", () => {
    // Craft two buffers where cell.fg is equal but cell.fgIndex differs.
    // This can't come from writeAnsi alone (same index → same RGB), so
    // forge the cells directly to exercise the diff logic.
    const prev = new CellBuffer(1, 1);
    prev.cells[0][0] = { ...emptyCell(), char: "A", fg: [0, 0, 204], fgIndex: 4 };
    const next = new CellBuffer(1, 1);
    next.cells[0][0] = { ...emptyCell(), char: "A", fg: [0, 0, 204], fgIndex: 12 };

    const out = diff(prev, next);
    expect(out).toContain("\x1b[94m"); // bright-blue SGR for index 12
  });
});
