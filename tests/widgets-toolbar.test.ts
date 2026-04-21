import { describe, it, expect } from "vitest";
import { toolbar, toolbarItemFor, type ToolbarItem } from "../src/tui/widgets/toolbar.ts";

const items: ToolbarItem[] = [
  { key: "n", label: "ew" },
  { key: "s", label: "ave", active: true },
  { key: "/", label: "Search", hint: "fuzzy" },
  { key: "q", label: "uit", disabled: true },
];

describe("toolbar — bracket format (default)", () => {
  const node = toolbar(items);

  it("is a single row", () => {
    expect((node as any).type).toBe("row");
  });

  it("wraps each key in [K]", () => {
    const flat = (node as any).children.map((c: any) => c.text ?? "").join("");
    expect(flat).toContain("[N]");
    expect(flat).toContain("[S]");
    expect(flat).toContain("[/]");
  });

  it("uppercases the bound letter between brackets", () => {
    const flat = (node as any).children.map((c: any) => c.text ?? "").join("");
    expect(flat).toContain("[N]");
    // Original key was lowercase but render uppercases.
    expect(flat).not.toContain("[n]");
  });

  it("marks active items via bold", () => {
    // Find the [S] sub-text and check bold. Text builder spreads style onto
    // the node directly (not under .style).
    const save = (node as any).children.find((c: any) => c.text === "S");
    expect(save.bold).toBe(true);
  });

  it("marks disabled items dim", () => {
    const quit = (node as any).children.find((c: any) => c.text === "uit");
    expect(quit.dim).toBe(true);
  });
});

describe("toolbar — inline format", () => {
  it("highlights the first occurrence of the key inside the label", () => {
    const node = toolbar([{ key: "n", label: "new" }], { format: "inline" });
    const texts = (node as any).children.map((c: any) => c.text);
    // Expected sequence: "" (before), "n" (the bound char), "ew"
    expect(texts).toEqual(["", "n", "ew"]);
  });
});

describe("toolbarItemFor", () => {
  it("matches by key, skipping disabled items", () => {
    expect(toolbarItemFor(items, "n")?.key).toBe("n");
    expect(toolbarItemFor(items, "Q")).toBeNull(); // disabled
    expect(toolbarItemFor(items, "x")).toBeNull(); // unknown
    expect(toolbarItemFor(items, undefined)).toBeNull();
  });
});
