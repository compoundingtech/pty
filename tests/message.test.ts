import { describe, it, expect } from "vitest";
import { message, screen, CellBuffer, themes, type ColumnNode, type RowNode, type TextNode } from "../src/tui/index.ts";

const theme = themes.coolBlue!;

describe("message — node contract", () => {
  it("incoming: a padded bubble on the muted fill, left-aligned", () => {
    const m = message("hi there") as ColumnNode;
    expect(m.type).toBe("column");
    const bubble = m.children[0] as TextNode;
    expect(bubble.type).toBe("text"); // no spacer wrapper when incoming
    expect(bubble.text).toBe(" hi there ");
    expect(bubble.background).toBe("border");
  });

  it("outgoing: accent fill, right-aligned with a flex spacer (MessageViewer)", () => {
    const m = message("hi", { outgoing: true }) as ColumnNode;
    const wrapper = m.children[0] as RowNode;
    expect(wrapper.type).toBe("row");
    expect(wrapper.children[0].type).toBe("spacer");
    expect((wrapper.children[1] as TextNode).background).toBe("accent");
  });

  it("a from label is rendered above the bubble", () => {
    const m = message("hi", { from: "cos" }) as ColumnNode;
    expect((m.children[0] as TextNode).text).toBe("cos");
    expect((m.children[0] as TextNode).bold).toBe(true);
    expect((m.children[1] as TextNode).text).toBe(" hi ");
  });

  it("multi-line content becomes one bubble row per line", () => {
    const m = message("a\nb\nc") as ColumnNode;
    expect(m.children).toHaveLength(3);
    expect((m.children[2] as TextNode).text).toBe(" c ");
  });
});

describe("message — rendered through the real buffer pipeline", () => {
  it("paints the incoming bubble on the border fill", () => {
    const s = screen({ id: "msg", render: () => [message("yo")] });
    const ctx = { rows: 4, cols: 30, theme, boxStyle: "rounded" as const } as any;
    const buf: CellBuffer = s.renderToBuffer(ctx);
    // The 'y' of the bubble sits on the border background.
    const yCol = buf.cells[0].findIndex((c) => c.char === "y");
    expect(yCol).toBeGreaterThanOrEqual(0);
    expect(buf.cells[0][yCol]!.bg).toEqual(theme.border ? [...theme.border] : null);
  });
});
