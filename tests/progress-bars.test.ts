import { describe, it, expect } from "vitest";
import { barProgress, barLoader, screen, CellBuffer, themes, type TextNode } from "../src/tui/index.ts";

const theme = themes.coolBlue!;

describe("barProgress / barLoader — node contract", () => {
  it("barProgress fills with ░ proportional to percent", () => {
    const b = barProgress(50, { width: 10 }) as TextNode;
    expect(b.type).toBe("text");
    expect(b.text).toBe("░░░░░     "); // 5 filled + 5 track spaces
  });

  it("barLoader fills with a solid █", () => {
    expect((barLoader(30, { width: 10 }) as TextNode).text).toBe("███       ");
  });

  it("colors the bar with the fill color on a subtle track background", () => {
    const b = barProgress(50, { width: 10 }) as TextNode;
    expect(b.color).toBe("accent");
    expect(b.background).toBe("border");
  });

  it("clamps percent to [0, 100]", () => {
    expect((barProgress(150, { width: 4 }) as TextNode).text).toBe("░░░░");
    expect((barProgress(-10, { width: 4 }) as TextNode).text).toBe("    ");
  });

  it("honors custom color and width", () => {
    const b = barProgress(25, { width: 8, color: "ok" }) as TextNode;
    expect(b.color).toBe("ok");
    expect(b.text).toBe("██      ".replace(/█/g, "░")); // 2 filled ░ + 6 spaces
  });

  it("background:null disables the track fill", () => {
    expect((barProgress(50, { background: null }) as TextNode).background).toBeUndefined();
  });
});

describe("barProgress — rendered through the real buffer pipeline", () => {
  it("paints the filled prefix in the fill color, both on the track fill", () => {
    const s = screen({ id: "bar", render: () => [barProgress(50, { width: 10 })] });
    const ctx = { rows: 4, cols: 20, theme, boxStyle: "rounded" as const } as any;
    const buf: CellBuffer = s.renderToBuffer(ctx);

    // First cell is a filled ░ in the accent color, on the border fill.
    const c0 = buf.cells[0][0]!;
    expect(c0.char).toBe("░");
    expect(c0.fg).toEqual(theme.fgAc ? [...theme.fgAc] : null);
    expect(c0.bg).toEqual(theme.border ? [...theme.border] : null);

    // Cell 6 is in the track (space) — still on the border fill.
    const track = buf.cells[0][6]!;
    expect(track.char).toBe(" ");
    expect(track.bg).toEqual(theme.border ? [...theme.border] : null);
  });
});
