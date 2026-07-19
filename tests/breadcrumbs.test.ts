import { describe, it, expect } from "vitest";
import { breadCrumbs, screen, CellBuffer, themes, type RowNode, type TextNode } from "../src/tui/index.ts";

const theme = themes.coolBlue!;

describe("breadCrumbs — node contract", () => {
  it("joins items with the ❯ separator", () => {
    const r = breadCrumbs(["net", "host", "agent"]) as RowNode;
    expect(r.type).toBe("row");
    const texts = (r.children as TextNode[]).map((c) => c.text);
    expect(texts).toEqual(["net", " ❯ ", "host", " ❯ ", "agent"]);
  });

  it("emphasizes the last crumb (accent + bold), muted separators", () => {
    const r = breadCrumbs(["net", "host", "agent"]) as RowNode;
    const kids = r.children as TextNode[];
    expect(kids[0].color).toBe("secondary"); // ancestor
    expect(kids[1].color).toBe("muted");     // separator
    expect(kids[4].color).toBe("accent");    // current
    expect(kids[4].bold).toBe(true);
  });

  it("emphasizeLast:false leaves the last crumb un-emphasized", () => {
    const r = breadCrumbs(["a", "b"], { emphasizeLast: false }) as RowNode;
    const last = (r.children as TextNode[]).at(-1)!;
    expect(last.color).toBe("secondary");
    expect(last.bold).toBeUndefined();
  });

  it("accepts { label } items and bare strings interchangeably", () => {
    const r = breadCrumbs([{ label: "x" }, "y"]) as RowNode;
    const texts = (r.children as TextNode[]).map((c) => c.text);
    expect(texts).toEqual(["x", " ❯ ", "y"]);
  });

  it("chips option pads each crumb and gives it a border fill", () => {
    const r = breadCrumbs(["a"], { chips: true }) as RowNode;
    const crumb = (r.children as TextNode[])[0];
    expect(crumb.text).toBe(" a ");
    expect(crumb.background).toBe("border");
  });

  it("a custom separator is honored", () => {
    const r = breadCrumbs(["a", "b"], { separator: " / " }) as RowNode;
    expect((r.children as TextNode[])[1].text).toBe(" / ");
  });

  it("a single crumb has no separator", () => {
    const r = breadCrumbs(["only"]) as RowNode;
    expect(r.children).toHaveLength(1);
  });
});

describe("breadCrumbs — rendered through the real buffer pipeline", () => {
  it("packs the trail onto one row with per-crumb colors", () => {
    const s = screen({ id: "bc", render: () => [breadCrumbs(["net", "host", "agent"])] });
    const ctx = { rows: 4, cols: 40, theme, boxStyle: "rounded" as const } as any;
    const buf: CellBuffer = s.renderToBuffer(ctx);

    const row0 = buf.cells[0].map((c) => c.char).join("");
    expect(row0).toContain("net ❯ host ❯ agent");

    // 'n' of "net" is an ancestor → secondary (fg2).
    const nCol = buf.cells[0].findIndex((c) => c.char === "n");
    expect(buf.cells[0][nCol]!.fg).toEqual(theme.fg2 ? [...theme.fg2] : null);

    // The last "agent" is the current crumb → accent (fgAc). Check its 't'
    // (the final char of the row's text), which is unambiguously in "agent".
    const tCols = buf.cells[0].flatMap((c, i) => (c.char === "t" ? [i] : []));
    const lastT = tCols.at(-1)!;
    expect(buf.cells[0][lastT]!.fg).toEqual(theme.fgAc ? [...theme.fgAc] : null);
  });
});
