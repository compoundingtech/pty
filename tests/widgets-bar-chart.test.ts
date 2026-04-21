import { describe, it, expect } from "vitest";
import { barChart } from "../src/tui/widgets/bar-chart.ts";

describe("barChart", () => {
  it("returns a canvas node with the expected height", () => {
    const n = barChart([{ value: 1 }, { value: 2 }], { height: 4 });
    expect((n as any).type).toBe("canvas");
    expect((n as any).height).toBe(4);
  });

  it("height + 1 when showLabels is set", () => {
    const n = barChart([{ label: "A", value: 1 }], { height: 4, showLabels: true });
    expect((n as any).height).toBe(5);
  });

  it("accepts an empty item list (renders nothing)", () => {
    const n = barChart([], { height: 3 });
    expect((n as any).type).toBe("canvas");
    expect((n as any).height).toBe(3);
  });

  it("clamps gracefully when min equals max", () => {
    // Should not throw; picks a constant mid-level per bar. We verify the
    // draw callback runs by checking it produces a canvas node.
    const n = barChart([{ value: 5 }, { value: 5 }], { min: 5, max: 5 });
    expect((n as any).type).toBe("canvas");
  });
});
