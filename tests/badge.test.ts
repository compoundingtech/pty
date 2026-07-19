import { describe, it, expect } from "vitest";
import { badge, screen, CellBuffer, themes, type TextNode } from "../src/tui/index.ts";

const theme = themes.coolBlue!;

describe("badge — node contract", () => {
  it("uppercases and pads the label (SRCL chip shape)", () => {
    const b = badge("live") as TextNode;
    expect(b.type).toBe("text");
    expect(b.text).toBe(" LIVE ");
  });

  it("neutral is primary text on a border-toned fill", () => {
    const b = badge("host") as TextNode;
    expect(b.color).toBe("primary");
    expect(b.background).toBe("border");
  });

  it("a variant colors the label on the muted fill", () => {
    const b = badge("available", { variant: "ok" }) as TextNode;
    expect(b.color).toBe("ok");
    expect(b.background).toBe("border");
    expect(b.text).toBe(" AVAILABLE ");
  });

  it("solid fills the chip with the variant color and uses primary text", () => {
    const b = badge("dead", { variant: "error", solid: true }) as TextNode;
    expect(b.color).toBe("primary");
    expect(b.background).toBe("error");
  });

  it("solid is ignored for neutral (no distinct neutral fill)", () => {
    const b = badge("x", { variant: "neutral", solid: true }) as TextNode;
    expect(b.color).toBe("primary");
    expect(b.background).toBe("border");
  });

  it("uppercase:false preserves the label case", () => {
    const b = badge("Host", { uppercase: false }) as TextNode;
    expect(b.text).toBe(" Host ");
  });

  it("bold is threaded through", () => {
    expect((badge("x", { bold: true }) as TextNode).bold).toBe(true);
  });
});

describe("badge — rendered through the real buffer pipeline", () => {
  it("paints the chip fill and colored label into cells", () => {
    const s = screen({ id: "badge-render", render: () => [badge("ok", { variant: "ok" })] });
    const ctx = { rows: 6, cols: 20, theme, boxStyle: "rounded" as const } as any;
    const buf: CellBuffer = s.renderToBuffer(ctx);

    // The chip renders on row 0 as " OK ".
    const row0 = buf.cells[0].map(c => c.char).join("");
    expect(row0).toContain(" OK ");

    // Find the "O" of the label and assert its fg = theme.ok, bg = theme.border.
    const oCol = buf.cells[0].findIndex(c => c.char === "O");
    expect(oCol).toBeGreaterThanOrEqual(0);
    const oCell = buf.cells[0][oCol]!;
    expect(oCell.fg).toEqual(theme.ok ? [...theme.ok] : null);
    expect(oCell.bg).toEqual(theme.border ? [...theme.border] : null);

    // The leading pad cell also carries the chip fill (distinct from the
    // screen's bg1 fill), so the chip reads as a filled block.
    const padCell = buf.cells[0][oCol - 1]!;
    expect(padCell.bg).toEqual(theme.border ? [...theme.border] : null);
  });
});
