import { describe, it, expect } from "vitest";
import { accordion, text, screen, CellBuffer, themes, type ColumnNode, type RowNode, type TextNode } from "../src/tui/index.ts";

const theme = themes.coolBlue!;

describe("accordion — node contract", () => {
  it("collapsed shows only the header with a ▸ glyph", () => {
    const a = accordion("Group", false, [text("child")]) as ColumnNode;
    expect(a.type).toBe("column");
    expect(a.children).toHaveLength(1); // header only
    const header = a.children[0] as RowNode;
    expect((header.children[0] as TextNode).text).toBe("▸ ");
    expect((header.children[1] as TextNode).text).toBe("Group");
  });

  it("expanded shows the header with ▾ plus the indented content", () => {
    const a = accordion("Group", true, [text("child")]) as ColumnNode;
    expect(a.children).toHaveLength(2);
    const header = a.children[0] as RowNode;
    expect((header.children[0] as TextNode).text).toBe("▾ ");
    const contentRow = a.children[1] as RowNode;
    expect((contentRow.children[0] as TextNode).text).toBe("  "); // 2-col indent
  });

  it("expanded with no children collapses to just the header", () => {
    const a = accordion("Empty", true, []) as ColumnNode;
    expect(a.children).toHaveLength(1);
  });

  it("focused highlights the header (accent + bold)", () => {
    const a = accordion("Group", false, [], { focused: true }) as ColumnNode;
    const header = a.children[0] as RowNode;
    expect((header.children[1] as TextNode).color).toBe("accent");
    expect((header.children[1] as TextNode).bold).toBe(true);
  });

  it("custom disclosure glyphs are honored", () => {
    const a = accordion("G", true, [text("x")], { expandedIcon: "-", collapsedIcon: "+" }) as ColumnNode;
    expect(((a.children[0] as RowNode).children[0] as TextNode).text).toBe("- ");
  });
});

describe("accordion — rendered through the real buffer pipeline", () => {
  it("renders the header and indents expanded content", () => {
    const s = screen({ id: "acc", render: () => [accordion("Group", true, [text("child")])] });
    const ctx = { rows: 6, cols: 30, theme, boxStyle: "rounded" as const } as any;
    const buf: CellBuffer = s.renderToBuffer(ctx);

    expect(buf.cells[0].map((c) => c.char).join("")).toContain("▾ Group");
    // Content "child" is on row 1, indented (does not start at column 0).
    const row1 = buf.cells[1].map((c) => c.char).join("");
    expect(row1).toContain("child");
    expect(row1.startsWith("child")).toBe(false); // indented
    expect(buf.cells[1].findIndex((c) => c.char === "c")).toBeGreaterThanOrEqual(2);
  });
});
