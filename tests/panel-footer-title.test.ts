import { describe, it, expect } from "vitest";
import { panel } from "../src/tui/builders.ts";

describe("panel footerTitle option", () => {
  it("defaults to undefined when opts omitted", () => {
    const p = panel("T", []);
    expect(p.footerTitle).toBeUndefined();
  });

  it("passes through a string opts (back-compat: style shorthand)", () => {
    const p = panel("T", [], "double");
    expect(p.style).toBe("double");
    expect(p.footerTitle).toBeUndefined();
  });

  it("reads footerTitle from the opts object", () => {
    const p = panel("T", [], { footerTitle: "4/17 layout" });
    expect(p.footerTitle).toBe("4/17 layout");
    expect(p.style).toBeUndefined();
  });

  it("supports style + footerTitle together", () => {
    const p = panel("T", [], { style: "double", footerTitle: "x" });
    expect(p.style).toBe("double");
    expect(p.footerTitle).toBe("x");
  });
});
