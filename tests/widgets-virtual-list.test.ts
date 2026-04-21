import { describe, it, expect } from "vitest";
import {
  createVirtualListState, clampVirtual, virtualWindow,
  moveVirtualSelection, pageVirtual,
  jumpVirtualToStart, jumpVirtualToEnd, handleVirtualKey,
  renderVirtualList,
} from "../src/tui/widgets/virtual-list.ts";
import { row, text } from "../src/tui/builders.ts";
import type { KeyEvent } from "../src/tui/input.ts";

function k(name: string): KeyEvent {
  return { name, ctrl: false, alt: false, shift: false };
}

describe("virtual list — windowing", () => {
  it("window matches viewport at the top", () => {
    const s = createVirtualListState(100, 10);
    expect(virtualWindow(s)).toEqual({ start: 0, end: 10 });
  });

  it("scrolls the window to keep selection in view (downward)", () => {
    const s0 = createVirtualListState(100, 10);
    const s1 = moveVirtualSelection(s0, 15);
    expect(s1.selectedIndex).toBe(15);
    expect(virtualWindow(s1)).toEqual({ start: 6, end: 16 });
  });

  it("clamps offset at the bottom", () => {
    const s = createVirtualListState(100, 10);
    const s1 = jumpVirtualToEnd(s);
    expect(s1.selectedIndex).toBe(99);
    expect(virtualWindow(s1)).toEqual({ start: 90, end: 100 });
  });

  it("empty list has no selection and an empty window", () => {
    const s = createVirtualListState(0, 10);
    expect(s.selectedIndex).toBe(-1);
    expect(virtualWindow(s)).toEqual({ start: 0, end: 0 });
  });

  it("clampVirtual re-normalises after total/viewport shrink", () => {
    const s0 = createVirtualListState(100, 10);
    const s1 = jumpVirtualToEnd(s0); // selected = 99
    const s2 = clampVirtual({ ...s1, total: 20 });
    expect(s2.selectedIndex).toBe(19);
    expect(virtualWindow(s2)).toEqual({ start: 10, end: 20 });
  });
});

describe("virtual list — navigation", () => {
  it("pageVirtual jumps by a viewport height", () => {
    const s = createVirtualListState(100, 10);
    const s1 = pageVirtual(s, 1);
    expect(s1.selectedIndex).toBe(10);
    const s2 = pageVirtual(s1, -1);
    expect(s2.selectedIndex).toBe(0);
  });

  it("home/end jump to edges", () => {
    const s = createVirtualListState(100, 10);
    const sDown = handleVirtualKey(s, k("end")).state;
    expect(sDown.selectedIndex).toBe(99);
    const sUp = handleVirtualKey(sDown, k("home")).state;
    expect(sUp.selectedIndex).toBe(0);
  });

  it("return emits action=activate without changing state", () => {
    const s = createVirtualListState(100, 10);
    const r = handleVirtualKey(s, k("return"));
    expect(r.action).toBe("activate");
    expect(r.state).toBe(s);
  });
});

describe("virtual list — rendering", () => {
  it("calls renderItem only for visible indexes", () => {
    const s = moveVirtualSelection(createVirtualListState(1000, 5), 50);
    const touched: number[] = [];
    renderVirtualList(s, (i, sel) => {
      touched.push(i);
      return row(text(`row ${i}${sel ? " *" : ""}`));
    });
    // Window is around index 50 — 5 rows. Exactly 5 calls to renderItem.
    expect(touched.length).toBe(5);
    expect(touched).toContain(50);
    expect(Math.min(...touched)).toBeGreaterThanOrEqual(46);
    expect(Math.max(...touched)).toBeLessThanOrEqual(54);
  });
});
