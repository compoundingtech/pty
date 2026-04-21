import { describe, it, expect } from "vitest";
import {
  createTextArea, textAreaToString, applyTextAreaKey, renderTextArea,
  type TextAreaState,
} from "../src/tui/widgets/text-area.ts";
import type { KeyEvent } from "../src/tui/input.ts";

function k(name: string, opts: Partial<KeyEvent> = {}): KeyEvent {
  return { name, char: opts.char, ctrl: opts.ctrl ?? false, alt: opts.alt ?? false, shift: opts.shift ?? false };
}

function typeIn(initial: TextAreaState, ...keys: KeyEvent[]): TextAreaState {
  let s: TextAreaState | null = initial;
  for (const ke of keys) {
    const r = applyTextAreaKey(s!, ke);
    if (r) s = r;
  }
  return s!;
}

describe("text area — basics", () => {
  it("starts empty with one empty line", () => {
    const s = createTextArea();
    expect(s.lines).toEqual([""]);
    expect(s).toEqual({ lines: [""], row: 0, col: 0 });
    expect(textAreaToString(s)).toBe("");
  });

  it("initial text is split on newlines", () => {
    const s = createTextArea("hello\nworld");
    expect(s.lines).toEqual(["hello", "world"]);
  });

  it("inserts printable chars at the cursor", () => {
    const s = typeIn(
      createTextArea(),
      k("h", { char: "h" }), k("i", { char: "i" }),
    );
    expect(textAreaToString(s)).toBe("hi");
    expect(s.col).toBe(2);
  });

  it("return inserts a newline, splitting the current line", () => {
    const s0 = createTextArea("hello world");
    const s1 = typeIn(s0, k("home"), k("right"), k("right"), k("right"), k("right"), k("right"), k("return"));
    expect(s1.lines).toEqual(["hello", " world"]);
    expect(s1.row).toBe(1);
    expect(s1.col).toBe(0);
  });

  it("backspace at col 0 joins with the previous line", () => {
    const s = typeIn(createTextArea("line1\nline2"), k("down"), k("backspace"));
    expect(s.lines).toEqual(["line1line2"]);
    expect(s.row).toBe(0);
    expect(s.col).toBe(5);
  });

  it("delete at end-of-line joins with the next line", () => {
    const s0 = createTextArea("line1\nline2");
    const s1 = typeIn(s0, k("end"), k("delete"));
    expect(s1.lines).toEqual(["line1line2"]);
    expect(s1.row).toBe(0);
    expect(s1.col).toBe(5);
  });
});

describe("text area — cursor movement", () => {
  it("left at col 0 of a non-first line wraps to end of previous line", () => {
    const s = typeIn(createTextArea("ab\nxy"), k("down"), k("left"));
    expect(s.row).toBe(0);
    expect(s.col).toBe(2);
  });

  it("right at end-of-line wraps to col 0 of next line", () => {
    const s = typeIn(createTextArea("ab\nxy"), k("end"), k("right"));
    expect(s.row).toBe(1);
    expect(s.col).toBe(0);
  });

  it("up/down preserves column when lines are equal, clamps when shorter", () => {
    const s0 = typeIn(createTextArea("longer\nshrt"), k("end"));
    expect(s0.col).toBe(6);
    const s1 = typeIn(s0, k("down"));
    expect(s1.col).toBe(4); // clamped to shrt.length
    const s2 = typeIn(s1, k("up"));
    // Going back up does NOT restore the "wanted" column — it stays at 4
    // (matches the simple model; we can add a "preferred col" later if needed).
    expect(s2.col).toBe(4);
  });

  it("home / end jump within the current line", () => {
    const s = typeIn(createTextArea("hello\nworld"), k("down"), k("end"), k("home"));
    expect(s.row).toBe(1);
    expect(s.col).toBe(0);
  });
});

describe("text area — keys that return null (caller handles)", () => {
  it("tab / backtab / escape pass through", () => {
    const s0 = createTextArea("x");
    expect(applyTextAreaKey(s0, k("tab"))).toBeNull();
    expect(applyTextAreaKey(s0, k("backtab"))).toBeNull();
    expect(applyTextAreaKey(s0, k("escape"))).toBeNull();
  });

  it("ctrl+return passes through (conventional submit-from-composer)", () => {
    const s0 = createTextArea("x");
    expect(applyTextAreaKey(s0, k("return", { ctrl: true }))).toBeNull();
  });

  it("ctrl/alt-modified printable keys pass through (no leaks)", () => {
    const s0 = createTextArea("x");
    expect(applyTextAreaKey(s0, k("s", { char: "s", ctrl: true }))).toBeNull();
    expect(applyTextAreaKey(s0, k("q", { char: "q", alt: true }))).toBeNull();
  });
});

describe("text area — rendering", () => {
  it("returns a column with one row per line", () => {
    const s = createTextArea("one\ntwo\nthree");
    const node = renderTextArea(s, false);
    // Root is a column node; structural check is enough here.
    expect((node as any).type).toBe("column");
    expect((node as any).children).toHaveLength(3);
  });

  it("renders the cursor row as three text nodes (before / inverse / after)", () => {
    // New rendering model: the cursor is an inverse-styled cell painted
    // on top of the char under it, not a glyph inserted into the string.
    // The focused row becomes a row of three text nodes.
    const s = createTextArea("abc\ndef");
    const active = renderTextArea({ ...s, row: 1, col: 1 }, true);
    const firstRowChildren = (active as any).children[0].children;
    const secondRowChildren = (active as any).children[1].children;
    // First row: no cursor — one plain text node.
    expect(firstRowChildren).toHaveLength(1);
    expect(firstRowChildren[0].text).toBe("abc");
    // Second row: before ("d"), inverse ("e"), after ("f").
    expect(secondRowChildren).toHaveLength(3);
    expect(secondRowChildren[0].text).toBe("d");
    expect(secondRowChildren[1].text).toBe("e");
    expect(secondRowChildren[1].inverse).toBe(true);
    expect(secondRowChildren[2].text).toBe("f");
  });

  it("cursor at end-of-line renders a single-space inverse cell", () => {
    const s = createTextArea("abc");
    const active = renderTextArea({ ...s, row: 0, col: 3 }, true);
    const ch = (active as any).children[0].children;
    expect(ch).toHaveLength(3);
    expect(ch[0].text).toBe("abc");
    expect(ch[1].text).toBe(" ");
    expect(ch[1].inverse).toBe(true);
    expect(ch[2].text).toBe("");
  });
});
