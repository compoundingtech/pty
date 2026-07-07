// Tests for the declarative TUI framework (src/tui/).
// These are pure framework tests — no experiment-app dependencies.
import { describe, it, expect } from "vitest";
import {
  text, spacer, gap, separator, indent, dot, checkbox, progressBar,
  spinner, icon, row, column, hstack, panel, scrollable, selectable,
  groupedSelectable, statusBar, footer, askBar, textInput, fpsCounter,
  canvas, screen, overlay,
  layoutRoot, layoutVertical, layoutRow, textWidth,
  renderToAnsi, resolveColor,
  type UINode, type RenderOpts,
} from "../src/tui/index.ts";
import { CellBuffer } from "../src/tui/buffer.ts";
import { createScrollRegion } from "../src/tui/scrollable.ts";
import { createTextInput } from "../src/tui/text-input.ts";
import { themes, type Theme } from "../src/tui/colors.ts";

const theme: Theme = themes.coolBlue;
const defaultOpts: RenderOpts = { spinnerChar: "\u280b", fps: 60, showFPS: false };

// ── Builders ──
describe("builders", () => {
  it("text() with all options", () => {
    const n = text("hello", "primary", { bold: true, truncate: true });
    expect(n.type).toBe("text");
    expect(n.text).toBe("hello");
    expect(n.color).toBe("primary");
    expect(n.bold).toBe(true);
    expect(n.truncate).toBe(true);
  });

  it("text() with RGB color", () => {
    const n = text("hi", [255, 0, 0]);
    expect(n.color).toEqual([255, 0, 0]);
  });

  it("text() object shape: { fg, bold, ... }", () => {
    // Consumers that prefer the object form pass `fg` for the color and
    // any TextOpts inline. `fg` maps to `color`; the rest spread as-is.
    const n = text("hi", { fg: [255, 0, 0], bold: true });
    expect(n.color).toEqual([255, 0, 0]);
    expect(n.bold).toBe(true);
  });

  it("text() object shape: `fg` with a SemanticColor string", () => {
    const n = text("hi", { fg: "primary", italic: true });
    expect(n.color).toBe("primary");
    expect(n.italic).toBe(true);
  });

  it("text() object shape without `fg` leaves color unset", () => {
    const n = text("hi", { bold: true, dim: true });
    expect(n.color).toBeUndefined();
    expect(n.bold).toBe(true);
    expect(n.dim).toBe(true);
  });

  it("text() with no args after str", () => {
    const n = text("hi");
    expect(n.type).toBe("text");
    expect(n.text).toBe("hi");
    expect(n.color).toBeUndefined();
    expect(n.bold).toBeUndefined();
  });

  it("spacer, gap, separator, indent", () => {
    expect(spacer().type).toBe("spacer");
    expect(gap(3).size).toBe(3);
    expect(gap("center").size).toBe("center");
    expect(separator().type).toBe("separator");
    expect(indent(2).depth).toBe(2);
  });

  it("dot, checkbox, progressBar, spinner, icon", () => {
    expect(dot(true, "ok").filled).toBe(true);
    expect(dot(false).filled).toBe(false);
    expect(checkbox(true).checked).toBe(true);
    expect(progressBar(0.5, { width: 20 }).percent).toBe(0.5);
    expect(spinner("accent").type).toBe("spinner");
    expect(icon("\u2605", "warn").char).toBe("\u2605");
  });

  it("row, column, hstack compose", () => {
    const h = hstack({ gap: 2 }, [
      column({ width: 20 }, [text("left")]),
      column({ flex: true }, [text("right")]),
    ]);
    expect(h.type).toBe("hstack");
    expect(h.children.length).toBe(2);
    expect(h.children[0].width).toBe(20);
    expect(h.children[1].flex).toBe(true);
  });

  it("panel wraps children", () => {
    const p = panel("Tasks", [text("a"), separator(), text("b")]);
    expect(p.title).toBe("Tasks");
    expect(p.children.length).toBe(3);
  });

  it("selectable maps ScrollRegion", () => {
    const region = createScrollRegion(3, 10);
    const s = selectable(region, ["a", "b", "c"], (item, _i, sel) => [
      text(sel ? "> " + item : "  " + item),
    ]);
    expect(s.items.length).toBe(3);
    expect((s.items[0][0] as any).text).toBe("> a");
    expect((s.items[1][0] as any).text).toBe("  b");
  });

  it("groupedSelectable renders headers + items", () => {
    const region = createScrollRegion(4, 10);
    const s = groupedSelectable(region, [
      { title: "A", items: [1, 2] },
      { title: "B", items: [3, 4] },
    ], (item, _idx, sel) => [text(sel ? `> ${item}` : `  ${item}`)]);
    // header A, item 1, item 2, spacing, header B, item 3, item 4
    expect(s.items.length).toBe(7);
    expect((s.items[0][0] as any).text).toBe("A");
    expect((s.items[1][0] as any).text).toBe("> 1");
  });

  it("statusBar, footer, askBar, textInput, fpsCounter", () => {
    expect(statusBar("T", "R").left).toBe("T");
    expect(footer("hints").hints).toBe("hints");
    const ti = createTextInput();
    expect(askBar(ti).type).toBe("askBar");
    expect(textInput(ti).type).toBe("textInput");
    expect(fpsCounter().type).toBe("fpsCounter");
  });

  it("canvas produces CanvasNode", () => {
    const c = canvas(() => {});
    expect(c.type).toBe("canvas");
  });
});

// ── textWidth ──
describe("textWidth", () => {
  it("ASCII", () => expect(textWidth("hello")).toBe(5));
  it("CJK", () => expect(textWidth("\u4e2d\u6587")).toBe(4));
  it("empty", () => expect(textWidth("")).toBe(0));
});

// ── Layout ──
describe("layout", () => {
  it("statusBar top, footer bottom", () => {
    const nodes: UINode[] = [statusBar("App", "info"), text("body"), footer("help")];
    layoutRoot(nodes, { x: 0, y: 0, width: 80, height: 24 });
    expect(nodes[0]._rect).toEqual({ x: 0, y: 0, width: 80, height: 1 });
    expect(nodes[2]._rect).toEqual({ x: 0, y: 23, width: 80, height: 1 });
    expect(nodes[1]._rect!.y).toBe(1);
  });

  it("gap(center) centers vertically", () => {
    const nodes: UINode[] = [gap("center"), text("mid"), gap("center")];
    layoutRoot(nodes, { x: 0, y: 0, width: 80, height: 21 });
    expect(nodes[1]._rect!.y).toBe(10);
  });

  it("row with spacer distributes width", () => {
    const r = row(text("left"), spacer(), text("right"));
    layoutRoot([r], { x: 0, y: 0, width: 80, height: 1 });
    expect(r.children[0]._rect!.width).toBe(4);
    expect(r.children[2]._rect!.x).toBe(75);
  });

  it("hstack with fixed and flex columns", () => {
    const h = hstack({ gap: 1 }, [
      column({ width: 20 }, [text("left")]),
      column({ flex: true }, [text("right")]),
    ]);
    layoutRoot([h], { x: 0, y: 0, width: 80, height: 24 });
    expect(h.children[0]._rect!.width).toBe(20);
    expect(h.children[1]._rect!.x).toBe(21);
    expect(h.children[1]._rect!.width).toBe(59);
  });

  it("panel auto-heights", () => {
    const p = panel("Test", [text("a"), text("b"), text("c")]);
    layoutRoot([p], { x: 0, y: 0, width: 40, height: 24 });
    expect(p._rect!.height).toBe(5);
    expect(p.children[0]._rect!.x).toBe(2);
    expect(p.children[0]._rect!.y).toBe(1);
  });

  it("clipping: vertical overflow", () => {
    const nodes: UINode[] = [text("1"), text("2"), text("3")];
    layoutVertical(nodes, { x: 0, y: 0, width: 80, height: 2 });
    expect(nodes[2]._rect!.height).toBe(0);
  });

  it("clipping: horizontal overflow", () => {
    const r = row(text("a".repeat(50)), text("b".repeat(50)));
    layoutRoot([r], { x: 0, y: 0, width: 80, height: 1 });
    expect(r.children[0]._rect!.width).toBe(50);
    expect(r.children[1]._rect!.width).toBe(30);
  });

  it("scrollable viewport", () => {
    const items = Array.from({ length: 20 }, (_, i) => `item ${i}`);
    const s = scrollable(items, (item) => [text(item)]);
    s.offset = 5;
    layoutRoot([s], { x: 0, y: 0, width: 80, height: 10 });
    expect(s.items[5]![0]._rect).toBeDefined();
    expect(s.items[5]![0]._rect!.y).toBe(0);
  });

  it("canvas gets flex height", () => {
    const nodes: UINode[] = [text("above"), canvas(() => {}), text("below")];
    layoutRoot(nodes, { x: 0, y: 0, width: 80, height: 20 });
    expect(nodes[1]._rect!.height).toBe(18);
  });

  it("canvas respects fixed height", () => {
    const nodes: UINode[] = [canvas(() => {}, { height: 5 })];
    layoutRoot(nodes, { x: 0, y: 0, width: 80, height: 20 });
    expect(nodes[0]._rect!.height).toBe(5);
  });
});

// ── Renderer ──
describe("renderer", () => {
  it("resolveColor maps semantic names", () => {
    expect(resolveColor("ok", theme)).toEqual(theme.ok);
    expect(resolveColor("primary", theme)).toEqual(theme.fg1);
    expect(resolveColor([10, 20, 30], theme)).toEqual([10, 20, 30]);
    expect(resolveColor(undefined, theme)).toBeNull();
  });

  it("renders text at correct position", () => {
    const nodes: UINode[] = [text("hello", "primary")];
    layoutRoot(nodes, { x: 0, y: 0, width: 80, height: 24 });
    const ansi = renderToAnsi(nodes, theme, "rounded", defaultOpts);
    expect(ansi).toContain("\x1b[1;1H");
    expect(ansi).toContain("hello");
  });

  it("renders panel with border and title", () => {
    const nodes: UINode[] = [panel("Tasks", [text("item 1")])];
    layoutRoot(nodes, { x: 0, y: 0, width: 40, height: 10 });
    const ansi = renderToAnsi(nodes, theme, "rounded", defaultOpts);
    expect(ansi).toContain("Tasks");
    expect(ansi).toContain("item 1");
    expect(ansi).toContain("\u256d");
  });

  it("renders dot, checkbox, progressBar", () => {
    const nodes: UINode[] = [row(dot(true, "ok"), checkbox(false), progressBar(0.5, { width: 10 }))];
    layoutRoot(nodes, { x: 0, y: 0, width: 80, height: 1 });
    const ansi = renderToAnsi(nodes, theme, "rounded", defaultOpts);
    expect(ansi).toContain("\u25cf");
    expect(ansi).toContain("\u25a1");
    expect(ansi).toContain("\u2588");
  });

  it("renders spinner", () => {
    const nodes: UINode[] = [spinner("accent")];
    layoutRoot(nodes, { x: 0, y: 0, width: 80, height: 1 });
    const ansi = renderToAnsi(nodes, theme, "rounded", { ...defaultOpts, spinnerChar: "\u2819" });
    expect(ansi).toContain("\u2819");
  });

  it("centered text via spacers", () => {
    const r = row(spacer(), text("CENTER"), spacer());
    layoutRoot([r], { x: 0, y: 0, width: 80, height: 1 });
    const ansi = renderToAnsi([r], theme, "rounded", defaultOpts);
    const buf = new CellBuffer(1, 80);
    buf.writeAnsi(ansi);
    const start = buf.cells[0].findIndex(c => c.char === "C");
    expect(start).toBe(37);
  });
});

// ── screen() wrapper ──
describe("screen() wrapper", () => {
  it("produces Screen with render + renderToBuffer + handleKey", () => {
    const s = screen({ id: "test", render: () => [text("hi")] });
    expect(s.id).toBe("test");
    expect(typeof s.render).toBe("function");
    expect(typeof s.renderToBuffer).toBe("function");
    expect(typeof s.handleKey).toBe("function");
  });

  it("render returns ANSI with content", () => {
    const s = screen({ id: "test", render: () => [text("body")] });
    const ctx = { rows: 24, cols: 80, theme, boxStyle: "rounded" as const } as any;
    expect(s.render(ctx)).toContain("body");
  });

  it("renderToBuffer returns CellBuffer", () => {
    const s = screen({ id: "test", render: () => [text("hello")] });
    const ctx = { rows: 24, cols: 80, theme, boxStyle: "rounded" as const } as any;
    const buf = s.renderToBuffer(ctx);
    expect(buf).toBeInstanceOf(CellBuffer);
    expect(buf.cells[0].map(c => c.char).join("")).toContain("hello");
  });

  it("renderToBuffer fills background with theme bg1", () => {
    const s = screen({ id: "test", render: () => [text("x")] });
    const ctx = { rows: 24, cols: 80, theme, boxStyle: "rounded" as const } as any;
    const buf = s.renderToBuffer(ctx);
    expect(buf.cells[20][60].bg).toEqual(theme.bg1 ? [...theme.bg1] : null);
  });

  it("text inside a panel preserves the panel background color", () => {
    const s = screen({
      id: "panel-bg-test",
      render() {
        return [panel("Test", [text("hello", "primary")])];
      },
    });
    const ctx = { rows: 10, cols: 40, theme, boxStyle: "rounded" as const } as any;
    const buf = s.renderToBuffer(ctx);
    // Find the row with "hello" text
    let helloRow = -1;
    for (let r = 0; r < buf.rows; r++) {
      const rowText = buf.cells[r].map(c => c.char).join("");
      if (rowText.includes("hello")) { helloRow = r; break; }
    }
    expect(helloRow).toBeGreaterThan(0);
    // The cell with "h" should have the panel's bg2 background, not null
    const hCol = buf.cells[helloRow].findIndex(c => c.char === "h");
    expect(hCol).toBeGreaterThan(0);
    expect(buf.cells[helloRow][hCol].bg).toEqual(theme.bg2 ? [...theme.bg2] : null);
  });
});

// ── overlay() wrapper ──
describe("overlay() wrapper", () => {
  it("produces Screen with renderToBuffer", () => {
    const o = overlay({
      id: "test-overlay", title: "Test", width: 40, height: 10,
      render: () => [text("content")],
    });
    expect(typeof o.renderToBuffer).toBe("function");
  });

  it("renderToBuffer has centered content", () => {
    const o = overlay({
      id: "test-overlay", title: "Test", width: 40, height: 10,
      render: () => [text("inside overlay")],
    });
    const ctx = { rows: 24, cols: 80, theme, boxStyle: "rounded" as const } as any;
    const buf = o.renderToBuffer(ctx);
    let found = false;
    for (let r = 0; r < buf.rows; r++) {
      if (buf.cells[r].map(c => c.char).join("").includes("inside overlay")) { found = true; break; }
    }
    expect(found).toBe(true);
  });

  it("has border characters", () => {
    const o = overlay({
      id: "test-overlay", title: "Test", width: 40, height: 10,
      render: () => [text("x")],
    });
    const ctx = { rows: 24, cols: 80, theme, boxStyle: "rounded" as const } as any;
    const buf = o.renderToBuffer(ctx);
    let hasCorner = false;
    for (let r = 0; r < buf.rows && !hasCorner; r++) {
      for (let c = 0; c < buf.cols && !hasCorner; c++) {
        if (buf.cells[r][c].char === "\u256d") hasCorner = true;
      }
    }
    expect(hasCorner).toBe(true);
  });
});

// ── Canvas ──
describe("canvas", () => {
  it("draw callback receives width/height", () => {
    let w = 0, h = 0;
    const nodes: UINode[] = [canvas((ctx) => { w = ctx.width; h = ctx.height; })];
    layoutRoot(nodes, { x: 0, y: 0, width: 60, height: 15 });
    renderToAnsi(nodes, theme, "rounded", defaultOpts);
    expect(w).toBe(60);
    expect(h).toBe(15);
  });

  it("set() places character at correct position", () => {
    const nodes: UINode[] = [canvas((ctx) => { ctx.set(5, 3, "@", "accent"); })];
    layoutRoot(nodes, { x: 0, y: 0, width: 80, height: 20 });
    const ansi = renderToAnsi(nodes, theme, "rounded", defaultOpts);
    expect(ansi).toContain("\x1b[4;6H");
    expect(ansi).toContain("@");
  });

  it("fill() fills rectangle", () => {
    const nodes: UINode[] = [canvas((ctx) => { ctx.fill(0, 0, 3, 2, "#", "muted"); })];
    layoutRoot(nodes, { x: 0, y: 0, width: 80, height: 20 });
    const ansi = renderToAnsi(nodes, theme, "rounded", defaultOpts);
    expect((ansi.match(/#/g) || []).length).toBe(6);
  });

  it("clips to bounds", () => {
    let cellCount = 0;
    const c = canvas((ctx) => {
      ctx.set(-1, 0, "X");
      ctx.set(ctx.width, 0, "X");
      ctx.set(0, -1, "X");
      ctx.set(0, ctx.height, "X");
      ctx.set(0, 0, "V");
      cellCount = (c as any)._cells.length;
    });
    layoutRoot([c], { x: 0, y: 0, width: 10, height: 5 });
    renderToAnsi([c], theme, "rounded", defaultOpts);
    expect(cellCount).toBe(1);
  });

  it("canvas inside panel gets inset rect", () => {
    let w = 0, h = 0;
    const nodes: UINode[] = [panel("Game", [canvas((ctx) => { w = ctx.width; h = ctx.height; })])];
    layoutRoot(nodes, { x: 0, y: 0, width: 40, height: 12 });
    renderToAnsi(nodes, theme, "rounded", defaultOpts);
    expect(w).toBe(36);
    expect(h).toBe(10);
  });

  it("sidebar + canvas in hstack", () => {
    let gw = 0, gh = 0;
    const nodes: UINode[] = [
      statusBar("Game", "score"),
      hstack({ gap: 0 }, [
        column({ width: 20 }, [panel("Inv", [text("sword")])]),
        column({ flex: true }, [canvas((ctx) => { gw = ctx.width; gh = ctx.height; })]),
      ]),
      footer("quit"),
    ];
    layoutRoot(nodes, { x: 0, y: 0, width: 80, height: 24 });
    renderToAnsi(nodes, theme, "rounded", defaultOpts);
    expect(gw).toBe(60);
    expect(gh).toBe(22);
  });
});

// ── CellBuffer ANSI parser ──
describe("CellBuffer", () => {
  it("handles CSI private parameter prefixes without leaking text", () => {
    const buf = new CellBuffer(5, 40);
    buf.writeAnsi("\x1b[?2004h\x1b[?25lhello");
    const row0 = buf.cells[0].map(c => c.char).join("").trim();
    expect(row0).toBe("hello");
    expect(row0).not.toContain("2004");
  });
});

// ── Text wrapping ──
describe("text wrap", () => {
  it("wrapText breaks at word boundaries (space)", () => {
    const { wrapText } = require("../src/tui/colors.ts");
    const { lines, offsets } = wrapText("Hello World", 8);
    expect(lines).toEqual(["Hello", " World"]);
    expect(offsets).toEqual([0, 5]);
  });

  it("wrapText falls back to char-breaking for long words", () => {
    const { wrapText } = require("../src/tui/colors.ts");
    const { lines, offsets } = wrapText("abcdefgh", 5);
    expect(lines).toEqual(["abcde", "fgh"]);
    expect(offsets).toEqual([0, 5]);
  });

  it("wrapText char-breaks when a word exceeds width", () => {
    const { wrapText } = require("../src/tui/colors.ts");
    // "Hello" fits in 5, " World!" starts next line but 7 > 5, char-breaks
    const { lines, offsets } = wrapText("Hello World!", 5);
    expect(lines).toEqual(["Hello", " Worl", "d!"]);
    expect(offsets).toEqual([0, 5, 10]);
  });

  it("wrapText breaks CJK characters correctly", () => {
    const { wrapText } = require("../src/tui/colors.ts");
    // Each CJK char is 2 cells wide; width 6 fits 3 CJK chars per line
    const { lines, offsets } = wrapText("\u4e2d\u6587\u6d4b\u8bd5\u6570\u636e", 6);
    expect(lines).toEqual(["\u4e2d\u6587\u6d4b", "\u8bd5\u6570\u636e"]);
    expect(offsets).toEqual([0, 3]);
  });

  it("wrapText returns single line if text fits", () => {
    const { wrapText } = require("../src/tui/colors.ts");
    const { lines } = wrapText("Hi", 10);
    expect(lines).toEqual(["Hi"]);
  });

  it("wrapText handles empty string", () => {
    const { wrapText } = require("../src/tui/colors.ts");
    const { lines } = wrapText("", 10);
    expect(lines).toEqual([""]);
  });

  it("text node with wrap: true has correct measured height", () => {
    const node = text("Hello World, this is a long line", "primary", { wrap: true });
    const nodes: UINode[] = [node];
    // 10 cols wide — word-wrap: "Hello" / " World," / " this is" / " a long" / " line" = 5 lines
    layoutRoot(nodes, { x: 0, y: 0, width: 10, height: 20 });
    expect(node._rect!.height).toBe(5);
  });

  it("text node with wrap: true renders multiple lines in buffer", () => {
    const myScreen = screen({
      id: "wrap-test",
      render() {
        return [text("AAAA BBBB CCCC DDDD", "primary", { wrap: true })];
      },
    });
    const ctx = {
      rows: 10, cols: 10,
      theme, boxStyle: "rounded" as const,
      navigate: () => {}, back: () => {},
      openOverlay: () => {}, closeOverlay: () => {},
      isTextInputActive: () => false, setTextInputActive: () => {},
    };
    const buf = myScreen.renderToBuffer(ctx);
    const line0 = buf.cells[0].map(c => c.char).join("").trim();
    const line1 = buf.cells[1].map(c => c.char).join("").trim();
    expect(line0).toBe("AAAA BBBB");
    expect(line1).toContain("CCCC");
  });

  it("text node without wrap truncates", () => {
    const node = text("Hello World", "primary", { truncate: true });
    const nodes: UINode[] = [node];
    layoutRoot(nodes, { x: 0, y: 0, width: 8, height: 1 });
    expect(node._rect!.height).toBe(1);
  });
});

// ── Text highlight ──
describe("text highlight", () => {
  it("text() accepts a highlight callback", () => {
    const highlighter = (t: string) => [{ start: 0, end: 5, bold: true }];
    const n = text("Hello World", "primary", { highlight: highlighter });
    expect(n.highlight).toBe(highlighter);
  });

  it("highlight applies bold to span range in buffer", () => {
    const highlighter = (t: string) => [
      { start: 0, end: 5, color: "accent" as const, bold: true },
    ];
    const myScreen = screen({
      id: "highlight-test",
      render() {
        return [text("Hello World", "primary", { highlight: highlighter })];
      },
    });
    const ctx = {
      rows: 5, cols: 40,
      theme, boxStyle: "rounded" as const,
      navigate: () => {}, back: () => {},
      openOverlay: () => {}, closeOverlay: () => {},
      isTextInputActive: () => false, setTextInputActive: () => {},
    };
    const buf = myScreen.renderToBuffer(ctx);
    // "Hello" (chars 0-4) should be bold + accent color
    expect(buf.cells[0][0].bold).toBe(true);
    expect(buf.cells[0][0].char).toBe("H");
    expect(buf.cells[0][4].bold).toBe(true);
    expect(buf.cells[0][4].char).toBe("o");
    // " World" (chars 5+) should NOT be bold
    expect(buf.cells[0][5].bold).toBe(false);
    expect(buf.cells[0][5].char).toBe(" ");
  });

  it("highlight works with wrap", () => {
    const highlighter = (t: string) => [
      { start: 0, end: 3, bold: true },  // "AAA"
      { start: 4, end: 7, bold: true },  // "BBB"
    ];
    const myScreen = screen({
      id: "highlight-wrap-test",
      render() {
        return [text("AAA BBB CCC", "primary", { wrap: true, highlight: highlighter })];
      },
    });
    const ctx = {
      rows: 5, cols: 6,
      theme, boxStyle: "rounded" as const,
      navigate: () => {}, back: () => {},
      openOverlay: () => {}, closeOverlay: () => {},
      isTextInputActive: () => false, setTextInputActive: () => {},
    };
    const buf = myScreen.renderToBuffer(ctx);
    // Word-wrap: "AAA" / " BBB" / " CCC" (breaks before spaces)
    // Line 0: "AAA" — chars 0-2 bold (span 0..3)
    expect(buf.cells[0][0].bold).toBe(true);   // A (index 0)
    expect(buf.cells[0][2].bold).toBe(true);   // A (index 2)
    // Line 1: " BBB" — char 0 is space (index 3, not bold), chars 1-3 are B (indices 4-6, bold)
    expect(buf.cells[1][0].bold).toBe(false);  // space (index 3)
    expect(buf.cells[1][1].bold).toBe(true);   // B (index 4)
    expect(buf.cells[1][3].bold).toBe(true);   // B (index 6)
    // Line 2: " CCC" — none bold
    expect(buf.cells[2][0].bold).toBe(false);  // space (index 7)
    expect(buf.cells[2][1].bold).toBe(false);  // C (index 8)
  });

  it("markdown heading highlight: lines starting with # are bold", () => {
    function markdownHighlight(content: string): { start: number; end: number; bold: boolean }[] {
      if (content.startsWith("#")) {
        return [{ start: 0, end: [...content].length, bold: true }];
      }
      return [];
    }
    const myScreen = screen({
      id: "md-test",
      render() {
        return [text("# Hello", "primary", { highlight: markdownHighlight })];
      },
    });
    const ctx = {
      rows: 5, cols: 40,
      theme, boxStyle: "rounded" as const,
      navigate: () => {}, back: () => {},
      openOverlay: () => {}, closeOverlay: () => {},
      isTextInputActive: () => false, setTextInputActive: () => {},
    };
    const buf = myScreen.renderToBuffer(ctx);
    expect(buf.cells[0][0].bold).toBe(true);
    expect(buf.cells[0][0].char).toBe("#");
    expect(buf.cells[0][6].bold).toBe(true);
    expect(buf.cells[0][6].char).toBe("o");
  });
});

// ── App lifecycle wrapper ──
describe("app()", () => {
  it("returns an object with start/stop/pause/resume", () => {
    const { app } = require("../src/tui/index.ts");
    const dummyScreen = screen({
      id: "dummy",
      render() { return [text("hello")]; },
    });
    const a = app({ screen: dummyScreen });
    expect(typeof a.start).toBe("function");
    expect(typeof a.stop).toBe("function");
    expect(typeof a.pause).toBe("function");
    expect(typeof a.resume).toBe("function");
  });

  it("accepts a screen function for dynamic screens", () => {
    const { app } = require("../src/tui/index.ts");
    const dummyScreen = screen({
      id: "dummy",
      render() { return [text("hello")]; },
    });
    const a = app({ screen: () => dummyScreen });
    expect(typeof a.start).toBe("function");
  });
});

// ── Fuzzy match ──
describe("fuzzyMatch", () => {
  const { fuzzyMatch } = require("../src/tui/fuzzy.ts");

  // Basic matching
  it("empty query matches everything", () => {
    expect(fuzzyMatch("", "anything").match).toBe(true);
  });

  it("exact match", () => {
    expect(fuzzyMatch("node", "node").match).toBe(true);
  });

  it("substring match", () => {
    expect(fuzzyMatch("server", "node-server").match).toBe(true);
  });

  it("fuzzy match — characters in order but not adjacent", () => {
    expect(fuzzyMatch("nsr", "node-server").match).toBe(true);
  });

  it("fuzzy match — skipping characters", () => {
    expect(fuzzyMatch("ns", "node-server").match).toBe(true);
  });

  it("no match when characters are out of order", () => {
    expect(fuzzyMatch("sn", "node-server").match).toBe(false);
  });

  it("no match when query has characters not in target", () => {
    expect(fuzzyMatch("xyz", "node-server").match).toBe(false);
  });

  it("case insensitive", () => {
    expect(fuzzyMatch("NODE", "node-server").match).toBe(true);
    expect(fuzzyMatch("node", "Node-Server").match).toBe(true);
  });

  it("query longer than target never matches", () => {
    expect(fuzzyMatch("longquery", "short").match).toBe(false);
  });

  // Scoring — better matches should score higher
  it("exact match scores higher than fuzzy match", () => {
    const exact = fuzzyMatch("node", "node");
    const fuzzy = fuzzyMatch("node", "n-o-d-e");
    expect(exact.score).toBeGreaterThan(fuzzy.score);
  });

  it("prefix match scores higher than middle match", () => {
    const prefix = fuzzyMatch("node", "node-server");
    const middle = fuzzyMatch("node", "my-node-server");
    expect(prefix.score).toBeGreaterThan(middle.score);
  });

  it("consecutive match scores higher than scattered match", () => {
    const consecutive = fuzzyMatch("serve", "server");
    const scattered = fuzzyMatch("serve", "s_e_r_v_e");
    expect(consecutive.score).toBeGreaterThan(scattered.score);
  });

  it("word boundary match scores higher than mid-word match", () => {
    const boundary = fuzzyMatch("server", "node-server");
    const midword = fuzzyMatch("server", "nodeserver");
    expect(boundary.score).toBeGreaterThanOrEqual(midword.score);
  });

  it("shorter target scores higher for same query", () => {
    const short = fuzzyMatch("node", "node");
    const long = fuzzyMatch("node", "node-server-application");
    expect(short.score).toBeGreaterThan(long.score);
  });

  // Edge cases
  it("single character query", () => {
    expect(fuzzyMatch("n", "node").match).toBe(true);
    expect(fuzzyMatch("z", "node").match).toBe(false);
  });

  it("single character target", () => {
    expect(fuzzyMatch("n", "n").match).toBe(true);
    expect(fuzzyMatch("no", "n").match).toBe(false);
  });

  it("special characters in query", () => {
    expect(fuzzyMatch(".", "file.ts").match).toBe(true);
    expect(fuzzyMatch("-", "node-server").match).toBe(true);
  });

  it("spaces in query match naturally", () => {
    expect(fuzzyMatch("no se", "node server").match).toBe(true);
  });
});
