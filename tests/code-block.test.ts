import { describe, it, expect } from "vitest";
import { codeBlock, screen, CellBuffer, themes, type ColumnNode, type RowNode, type TextNode } from "../src/tui/index.ts";

const theme = themes.coolBlue!;

describe("codeBlock — node contract", () => {
  it("renders one row per line with a right-aligned gutter", () => {
    const c = codeBlock("a\nb\nc") as ColumnNode;
    expect(c.type).toBe("column");
    expect(c.children).toHaveLength(3);
    const row0 = c.children[0] as RowNode;
    expect((row0.children[0] as TextNode).text).toBe("  1 "); // min-3 gutter, right-aligned
    expect((row0.children[1] as TextNode).text).toBe("a");
  });

  it("honors startLine", () => {
    const c = codeBlock("x\ny", { startLine: 41 }) as ColumnNode;
    expect(((c.children[0] as RowNode).children[0] as TextNode).text).toBe(" 41 ");
    expect(((c.children[1] as RowNode).children[0] as TextNode).text).toBe(" 42 ");
  });

  it("showLineNumbers:false drops the gutter (bare content nodes)", () => {
    const c = codeBlock("a\nb", { showLineNumbers: false }) as ColumnNode;
    expect((c.children[0] as TextNode).type).toBe("text");
    expect((c.children[0] as TextNode).text).toBe("a");
  });

  it("a per-line highlight callback is wired onto the content node", () => {
    const c = codeBlock("kw x", { highlight: (line) => [{ start: 0, end: 2, color: "accent" }] }) as ColumnNode;
    const content = (c.children[0] as RowNode).children[1] as TextNode;
    expect(typeof content.highlight).toBe("function");
    expect(content.highlight!("kw x")).toEqual([{ start: 0, end: 2, color: "accent" }]);
  });
});

describe("codeBlock — rendered through the real buffer pipeline", () => {
  it("paints the gutter and content", () => {
    const s = screen({ id: "cb", render: () => [codeBlock("hello\nworld")] });
    const ctx = { rows: 6, cols: 30, theme, boxStyle: "rounded" as const } as any;
    const buf: CellBuffer = s.renderToBuffer(ctx);
    expect(buf.cells[0].map((c) => c.char).join("")).toContain("  1 hello");
    expect(buf.cells[1].map((c) => c.char).join("")).toContain("  2 world");
  });
});
