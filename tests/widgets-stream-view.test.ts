import { describe, it, expect } from "vitest";
import {
  createStreamView, isPinned, streamPin,
  streamScrollUp, streamScrollDown, streamWindow,
  handleStreamKey, renderStreamView,
} from "../src/tui/widgets/stream-view.ts";
import type { KeyEvent } from "../src/tui/input.ts";

function k(name: string): KeyEvent {
  return { name, ctrl: false, alt: false, shift: false };
}

describe("stream-view — pinning and scrollback", () => {
  it("starts pinned at the bottom", () => {
    const s = createStreamView();
    expect(isPinned(s)).toBe(true);
  });

  it("window with total=10 viewport=3 pinned shows items 7-9", () => {
    const s = createStreamView();
    expect(streamWindow(s, 10, 3)).toEqual({ start: 7, end: 10 });
  });

  it("scrollUp increases scrollback, clamped by total - viewport", () => {
    const s0 = createStreamView();
    const s1 = streamScrollUp(s0, 1, 10, 3);
    expect(isPinned(s1)).toBe(false);
    expect(streamWindow(s1, 10, 3)).toEqual({ start: 6, end: 9 });
    const s2 = streamScrollUp(s1, 1000, 10, 3);
    // Clamped at total - viewport = 7
    expect(s2.scrollback).toBe(7);
    expect(streamWindow(s2, 10, 3)).toEqual({ start: 0, end: 3 });
  });

  it("scrollDown decreases scrollback, clamped at 0", () => {
    const s0 = streamScrollUp(createStreamView(), 5, 10, 3);
    const s1 = streamScrollDown(s0, 2);
    expect(s1.scrollback).toBe(3);
    const s2 = streamScrollDown(s1, 999);
    expect(s2.scrollback).toBe(0);
    expect(isPinned(s2)).toBe(true);
  });

  it("end key re-pins to bottom", () => {
    const s0 = streamScrollUp(createStreamView(), 3, 20, 5);
    const s1 = handleStreamKey(s0, k("end"), 20, 5);
    expect(s1).not.toBeNull();
    expect(isPinned(s1!)).toBe(true);
  });

  it("rendering includes an 'N more below' indicator when scrolled up", () => {
    const items = Array.from({ length: 10 }, (_, i) => `item-${i}`);
    const s = streamScrollUp(createStreamView(), 3, 10, 3);
    const node = renderStreamView(items, s, 3, (it) => ({ type: "row", children: [{ type: "text", text: it, color: "primary", style: {} }] } as any));
    const children = (node as any).children;
    const last = children[children.length - 1];
    const lastText = last.children[0].text as string;
    expect(lastText).toMatch(/more below/);
  });

  it("rendering has no indicator when pinned", () => {
    const items = Array.from({ length: 10 }, (_, i) => `item-${i}`);
    const s = createStreamView();
    const node = renderStreamView(items, s, 3, (it) => ({ type: "row", children: [{ type: "text", text: it, color: "primary", style: {} }] } as any));
    const children = (node as any).children;
    // Last child is the last visible item, not an indicator.
    expect((children[children.length - 1] as any).children[0].text).toBe("item-9");
  });
});
