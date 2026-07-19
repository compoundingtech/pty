import { describe, it, expect } from "vitest";
import { actionListItem, screen, CellBuffer, themes, type RowNode, type TextNode } from "../src/tui/index.ts";

const theme = themes.coolBlue!;

describe("actionListItem — node contract", () => {
  it("renders a 3-cell icon chip then the label", () => {
    const r = actionListItem("Deploy", { icon: "▶" }) as RowNode;
    expect(r.type).toBe("row");
    const chip = r.children[0] as TextNode;
    expect(chip.text).toBe(" ▶ ");
    expect(chip.background).toBe("border");
    expect((r.children[1] as TextNode).text).toBe(" Deploy");
  });

  it("blank icon still yields a stable 3-cell chip", () => {
    expect(((actionListItem("x") as RowNode).children[0] as TextNode).text).toBe("   ");
  });

  it("focused highlights the icon chip (accent) and bolds the label", () => {
    const r = actionListItem("Deploy", { icon: "▶", focused: true }) as RowNode;
    expect((r.children[0] as TextNode).background).toBe("accent");
    expect((r.children[1] as TextNode).bold).toBe(true);
  });

  it("right text is pushed to the end with a flex spacer", () => {
    const r = actionListItem("Session", { right: "3m" }) as RowNode;
    const kinds = r.children.map((c) => c.type);
    expect(kinds).toEqual(["text", "text", "spacer", "text"]);
    expect((r.children[3] as TextNode).text).toBe("3m");
  });
});

describe("actionListItem — rendered through the real buffer pipeline", () => {
  it("paints the icon chip background (accent when focused)", () => {
    const s = screen({ id: "ali", render: () => [actionListItem("Deploy", { icon: "▶", focused: true })] });
    const ctx = { rows: 4, cols: 30, theme, boxStyle: "rounded" as const } as any;
    const buf: CellBuffer = s.renderToBuffer(ctx);

    // The icon glyph is at column 1 (chip is " ▶ "), on the accent background.
    const chipCell = buf.cells[0][1]!;
    expect(chipCell.char).toBe("▶");
    expect(chipCell.bg).toEqual(theme.fgAc ? [...theme.fgAc] : null); // accent bg
    expect(buf.cells[0].map((c) => c.char).join("")).toContain("Deploy");
  });
});
