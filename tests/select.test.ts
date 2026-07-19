import { describe, it, expect } from "vitest";
import {
  createSelectState, renderSelect, handleSelectKey,
  screen, CellBuffer, themes,
  type ColumnNode, type RowNode, type TextNode,
} from "../src/tui/index.ts";

const theme = themes.coolBlue!;
const key = (name: string) => ({ name, ctrl: false, alt: false, shift: false } as any);
const OPTS = ["alpha", "beta", "gamma"];

describe("select — key reducer", () => {
  it("starts closed", () => {
    expect(createSelectState(1)).toEqual({ open: false, index: 1 });
  });

  it("Enter or Down opens a closed select", () => {
    expect(handleSelectKey(createSelectState(1), 3, key("return")).state.open).toBe(true);
    expect(handleSelectKey(createSelectState(0), 3, key("down")).state.open).toBe(true);
  });

  it("Up/Down move the highlight while open, clamped to bounds", () => {
    let s: any = { open: true, index: 0 };
    s = handleSelectKey(s, 3, key("down")).state; expect(s.index).toBe(1);
    s = handleSelectKey(s, 3, key("down")).state; expect(s.index).toBe(2);
    s = handleSelectKey(s, 3, key("down")).state; expect(s.index).toBe(2); // clamp at last
    s = handleSelectKey(s, 3, key("up")).state;   expect(s.index).toBe(1);
  });

  it("Enter while open commits the highlighted index and closes", () => {
    const r = handleSelectKey({ open: true, index: 2 }, 3, key("return"));
    expect(r.selectedIndex).toBe(2);
    expect(r.state.open).toBe(false);
  });

  it("Escape closes without committing", () => {
    const r = handleSelectKey({ open: true, index: 2 }, 3, key("escape"));
    expect(r.selectedIndex).toBeUndefined();
    expect(r.state.open).toBe(false);
  });
});

describe("select — render", () => {
  it("closed shows the caret + selected value only", () => {
    const node = renderSelect(OPTS, 1, createSelectState(1)) as ColumnNode;
    expect(node.children).toHaveLength(1); // button only
    const button = node.children[0] as RowNode;
    expect((button.children[0] as TextNode).text).toBe("▸ ");
    expect((button.children[1] as TextNode).text).toBe("beta");
  });

  it("placeholder shows when the selected index is out of range", () => {
    const button = (renderSelect(OPTS, -1, createSelectState(0), { placeholder: "pick…" }) as ColumnNode)
      .children[0] as RowNode;
    expect((button.children[1] as TextNode).text).toBe("pick…");
  });

  it("open lists the options with the highlight marked", () => {
    const node = renderSelect(OPTS, 0, { open: true, index: 1 }) as ColumnNode;
    expect(node.children).toHaveLength(1 + OPTS.length);
    const highlighted = (node.children[2] as RowNode).children[1] as TextNode; // beta row
    expect(highlighted.text).toBe("› beta");
    expect(highlighted.color).toBe("accent");
  });
});

describe("select — rendered through the real buffer pipeline", () => {
  it("renders the open dropdown into cells", () => {
    const s = screen({ id: "sel", render: () => [renderSelect(OPTS, 0, { open: true, index: 1 })] });
    const ctx = { rows: 8, cols: 24, theme, boxStyle: "rounded" as const } as any;
    const buf: CellBuffer = s.renderToBuffer(ctx);
    expect(buf.cells[0].map((c) => c.char).join("")).toContain("▾ alpha");
    // The highlighted option "beta" is marked with the › caret.
    const rowsText = buf.cells.map((r) => r.map((c) => c.char).join(""));
    expect(rowsText.some((t) => t.includes("› beta"))).toBe(true);
  });
});
