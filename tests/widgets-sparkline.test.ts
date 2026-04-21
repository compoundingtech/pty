import { describe, it, expect } from "vitest";
import { sparklineString } from "../src/tui/widgets/sparkline.ts";

describe("sparklineString", () => {
  it("renders one character per sample", () => {
    expect(sparklineString([0, 1, 2, 3]).length).toBe(4);
  });

  it("left-pads empties when width > series length", () => {
    const s = sparklineString([8], { width: 4, min: 0, max: 8 });
    expect(s.length).toBe(4);
    // Last char is full block; first three are empties.
    expect(s[3]).toBe("\u2588");
    expect(s[0]).toBe(" ");
  });

  it("tail-slices when series is longer than width", () => {
    const samples = [1, 2, 3, 4, 5, 6, 7, 8];
    const s = sparklineString(samples, { width: 3, min: 1, max: 8 });
    expect(s.length).toBe(3);
    // Tail is [6, 7, 8] — last char is the full-block (max).
    expect(s[2]).toBe("\u2588");
  });

  it("uses explicit min/max for stable scale across frames", () => {
    const a = sparklineString([50], { width: 1, min: 0, max: 100 });
    const b = sparklineString([50], { width: 1, min: 0, max: 100 });
    expect(a).toBe(b);
  });

  it("NaN / Infinity samples render as empties", () => {
    const s = sparklineString([NaN, Infinity, 1], { min: 0, max: 1 });
    expect(s[0]).toBe(" ");
    expect(s[1]).toBe(" ");
    expect(s[2]).toBe("\u2588");
  });

  it("all-equal series picks the middle block (not empty)", () => {
    const s = sparklineString([5, 5, 5], { min: 5, max: 5 });
    expect(s).toBe("\u2584\u2584\u2584");
  });

  it("empty input renders empty string", () => {
    expect(sparklineString([])).toBe("");
    expect(sparklineString([1, 2, 3], { width: 0 })).toBe("");
  });
});
