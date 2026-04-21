import { describe, it, expect } from "vitest";
import {
  applyTextKey, renderFieldText, renderFieldNodes,
  prevWordBoundary, nextWordBoundary,
  createFormState, handleFormKey, focusField, setFieldText,
  type TextFieldState, type FormState,
} from "../src/tui/widgets/form.ts";
import type { KeyEvent } from "../src/tui/input.ts";

function k(name: string, opts: Partial<KeyEvent> = {}): KeyEvent {
  return { name, char: opts.char, ctrl: opts.ctrl ?? false, alt: opts.alt ?? false, shift: opts.shift ?? false };
}

describe("applyTextKey", () => {
  const empty: TextFieldState = { text: "", cursor: 0 };
  const hello: TextFieldState = { text: "hello", cursor: 5 };

  it("inserts a printable character at the cursor", () => {
    const r = applyTextKey(hello, k("!", { char: "!" }));
    expect(r).toEqual({ text: "hello!", cursor: 6 });
  });

  it("backspace deletes the preceding character", () => {
    const r = applyTextKey(hello, k("backspace"));
    expect(r).toEqual({ text: "hell", cursor: 4 });
  });

  it("delete removes the character under the cursor", () => {
    const mid: TextFieldState = { text: "hello", cursor: 2 };
    const r = applyTextKey(mid, k("delete"));
    expect(r).toEqual({ text: "helo", cursor: 2 });
  });

  it("arrow keys move the cursor within bounds", () => {
    expect(applyTextKey(hello, k("left"))).toEqual({ text: "hello", cursor: 4 });
    expect(applyTextKey(empty, k("left"))).toEqual(empty);
    expect(applyTextKey(empty, k("right"))).toEqual(empty);
  });

  it("home/end jump to the extremes", () => {
    expect(applyTextKey(hello, k("home"))).toEqual({ text: "hello", cursor: 0 });
    expect(applyTextKey({ text: "hello", cursor: 0 }, k("end"))).toEqual({ text: "hello", cursor: 5 });
  });

  it("ctrl+u clears from start to cursor", () => {
    const mid: TextFieldState = { text: "hello", cursor: 3 };
    expect(applyTextKey(mid, k("u", { ctrl: true }))).toEqual({ text: "lo", cursor: 0 });
  });

  it("ignores ctrl/alt-modified printable keys so shortcuts don't leak", () => {
    expect(applyTextKey(hello, k("s", { char: "s", ctrl: true }))).toBeNull();
    expect(applyTextKey(hello, k("q", { char: "q", alt: true }))).toBeNull();
  });
});

describe("renderFieldText (legacy string API)", () => {
  it("shows a block cursor only when active", () => {
    expect(renderFieldText("foo", 1, false)).toBe("foo");
    expect(renderFieldText("foo", 1, true)).toBe("f\u2588oo");
    expect(renderFieldText("foo", 3, true)).toBe("foo\u2588");
  });
});

describe("renderFieldNodes (inverse-cursor API)", () => {
  it("inactive: one text node, no cursor", () => {
    const nodes = renderFieldNodes("hello", 2, false);
    expect(nodes).toHaveLength(1);
    expect((nodes[0] as any).text).toBe("hello");
  });

  it("active mid-string: three nodes with inverse cell on the char under the cursor", () => {
    const nodes = renderFieldNodes("hello", 2, true);
    expect(nodes).toHaveLength(3);
    expect((nodes[0] as any).text).toBe("he");
    expect((nodes[1] as any).text).toBe("l");
    expect((nodes[1] as any).inverse).toBe(true);
    expect((nodes[2] as any).text).toBe("lo");
  });

  it("active at end: inverse cell is a single space", () => {
    const nodes = renderFieldNodes("hi", 2, true);
    expect((nodes[1] as any).text).toBe(" ");
    expect((nodes[1] as any).inverse).toBe(true);
  });
});

describe("word boundaries", () => {
  it("prevWordBoundary jumps back over whitespace then word", () => {
    // "hello  world|" — cursor at 13. Expect jump to "world" start (7).
    expect(prevWordBoundary("hello  world", 12)).toBe(7);
    // Already at a boundary — jump further.
    expect(prevWordBoundary("hello world", 6)).toBe(0);
  });

  it("nextWordBoundary jumps forward over non-word then word", () => {
    // Cursor at the start of "hello" — jumps to end of "hello".
    expect(nextWordBoundary("hello world", 0)).toBe(5);
    // Cursor at the space — skip the space, jump to end of "world".
    expect(nextWordBoundary("hello world", 5)).toBe(11);
  });

  it("handles leading/trailing whitespace", () => {
    expect(prevWordBoundary("   abc", 6)).toBe(3);
    expect(nextWordBoundary("abc   ", 0)).toBe(3);
    expect(nextWordBoundary("abc   ", 3)).toBe(6);
  });
});

describe("applyTextKey — word motion via alt+arrows / alt+b / alt+f", () => {
  const state: TextFieldState = { text: "hello there friend", cursor: 11 };
  it("alt+left jumps back one word", () => {
    const r = applyTextKey(state, k("left", { alt: true }));
    expect(r?.cursor).toBe(6); // start of "there"
  });
  it("alt+right jumps forward one word", () => {
    const r = applyTextKey(state, k("right", { alt: true }));
    expect(r?.cursor).toBe(18); // end of "friend"
  });
  it("alt+b == alt+left", () => {
    const r = applyTextKey(state, k("b", { char: "b", alt: true }));
    expect(r?.cursor).toBe(6);
  });
  it("alt+f == alt+right", () => {
    const r = applyTextKey(state, k("f", { char: "f", alt: true }));
    expect(r?.cursor).toBe(18);
  });
});

describe("form focus ring with tab/backtab", () => {
  type F = "title" | "notes" | "due";
  const order = ["title", "notes", "due"] as const satisfies readonly F[];

  it("tab walks forward, backtab walks backward (wrapping)", () => {
    const s0 = createFormState(order, { title: "", notes: "", due: "" });
    const r1 = handleFormKey(s0, k("tab"));
    expect(r1.state.focused).toBe("notes");
    expect(r1.action).toBe("moved");
    const r2 = handleFormKey(r1.state, k("tab"));
    expect(r2.state.focused).toBe("due");
    const r3 = handleFormKey(r2.state, k("tab"));
    expect(r3.state.focused).toBe("title"); // wraps
    const r4 = handleFormKey(r3.state, k("backtab"));
    expect(r4.state.focused).toBe("due"); // wraps backward
  });

  it("enter in a non-last field is 'activate', in the last field is 'submit'", () => {
    const s0 = createFormState(order, { title: "", notes: "", due: "" });
    expect(handleFormKey(s0, k("return")).action).toBe("activate");
    const focusedLast = { ...s0, focused: "due" as const };
    expect(handleFormKey(focusedLast, k("return")).action).toBe("submit");
  });

  it("escape is 'cancel'", () => {
    const s0 = createFormState(order, { title: "", notes: "", due: "" });
    expect(handleFormKey(s0, k("escape")).action).toBe("cancel");
  });

  it("edits the focused field when a printable key arrives", () => {
    const s0 = createFormState(order, { title: "", notes: "", due: "" });
    const r = handleFormKey(s0, k("a", { char: "a" }));
    expect(r.action).toBe("edited");
    expect(r.state.values.title.text).toBe("a");
  });

  it("setFieldText resets cursor to end and works without focus change", () => {
    const s0 = createFormState(order, { title: "old", notes: "", due: "" });
    const s1 = setFieldText(s0, "notes", "hello");
    expect(s1.values.notes).toEqual({ text: "hello", cursor: 5 });
    expect(s1.focused).toBe("title");
  });
});
