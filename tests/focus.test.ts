import { describe, it, expect } from "vitest";
import { createFocusManager } from "../src/tui/focus.ts";
import type { KeyEvent, MouseEvent, ScreenContext } from "../src/tui/index.ts";

function k(name: string, extra: Partial<KeyEvent> = {}): KeyEvent {
  return { kind: "key", name, ctrl: false, alt: false, shift: false, ...extra };
}
function me(action: MouseEvent["action"] = "press"): MouseEvent {
  return { kind: "mouse", action, button: "left", x: 0, y: 0, ctrl: false, alt: false, shift: false };
}
function mkCtx(): ScreenContext {
  return {
    rows: 40, cols: 100, theme: {} as any, boxStyle: "rounded",
    navigate: () => {}, back: () => {}, openOverlay: () => {}, closeOverlay: () => {},
    isTextInputActive: () => false, setTextInputActive: () => {},
    quit: () => {},
    focus: {} as any, // not used in these tests
  };
}

describe("createFocusManager — stack semantics", () => {
  it("starts empty", () => {
    const f = createFocusManager();
    expect(f.current()).toBeNull();
    expect(f.stack()).toEqual([]);
  });

  it("push returns a disposer that removes the scope", () => {
    const f = createFocusManager();
    const d = f.push({ id: "a" });
    expect(f.stack().map(s => s.id)).toEqual(["a"]);
    d();
    expect(f.stack()).toEqual([]);
  });

  it("disposer is idempotent", () => {
    const f = createFocusManager();
    const d = f.push({ id: "a" });
    d(); d(); d();
    expect(f.stack()).toEqual([]);
  });

  it("current() returns the innermost-active scope", () => {
    const f = createFocusManager();
    f.push({ id: "a" });
    f.push({ id: "b" });
    f.push({ id: "c" });
    expect(f.current()?.id).toBe("c");
  });
});

describe("dispatchKey — bubbling", () => {
  it("innermost wins", () => {
    const f = createFocusManager();
    const calls: string[] = [];
    f.push({ id: "outer", onKey: () => { calls.push("outer"); return true; } });
    f.push({ id: "inner", onKey: () => { calls.push("inner"); return true; } });
    f.dispatchKey(k("a"), mkCtx());
    expect(calls).toEqual(["inner"]);
  });

  it("returning false bubbles to the next-outer scope", () => {
    const f = createFocusManager();
    const calls: string[] = [];
    f.push({ id: "outer", onKey: () => { calls.push("outer"); return true; } });
    f.push({ id: "inner", onKey: () => { calls.push("inner"); return false; } });
    f.dispatchKey(k("a"), mkCtx());
    expect(calls).toEqual(["inner", "outer"]);
  });

  it("returns true when any scope consumed, false when none did", () => {
    const f = createFocusManager();
    f.push({ id: "a", onKey: () => false });
    f.push({ id: "b", onKey: () => false });
    expect(f.dispatchKey(k("x"), mkCtx())).toBe(false);

    f.push({ id: "c", onKey: () => true });
    expect(f.dispatchKey(k("x"), mkCtx())).toBe(true);
  });

  it("scopes without onKey are silently skipped in the bubble", () => {
    const f = createFocusManager();
    const calls: string[] = [];
    f.push({ id: "outer", onKey: () => { calls.push("outer"); return true; } });
    f.push({ id: "middle" });  // no onKey
    f.push({ id: "inner",  onKey: () => { calls.push("inner"); return false; } });
    f.dispatchKey(k("a"), mkCtx());
    expect(calls).toEqual(["inner", "outer"]);
  });
});

describe("active() predicate — sibling scopes", () => {
  it("skips inactive scopes", () => {
    const f = createFocusManager();
    let paneIsA = true;
    const calls: string[] = [];
    f.push({
      id: "A",
      active: () => paneIsA,
      onKey: () => { calls.push("A"); return true; },
    });
    f.push({
      id: "B",
      active: () => !paneIsA,
      onKey: () => { calls.push("B"); return true; },
    });
    f.dispatchKey(k("x"), mkCtx());
    expect(calls).toEqual(["A"]);
    calls.length = 0;
    paneIsA = false;
    f.dispatchKey(k("x"), mkCtx());
    expect(calls).toEqual(["B"]);
  });

  it("inactive scopes still appear in stack() but not current()", () => {
    const f = createFocusManager();
    f.push({ id: "A", active: () => false });
    f.push({ id: "B", active: () => true });
    expect(f.stack().map(s => s.id)).toEqual(["A", "B"]);
    expect(f.current()?.id).toBe("B");
  });
});

describe("nested scopes — classic app shape", () => {
  it("global + pane + modal: modal wins, then pane, then global", () => {
    const f = createFocusManager();
    const calls: string[] = [];
    f.push({
      id: "global",
      onKey: (key) => {
        calls.push("global");
        if (key.name === "c" && key.ctrl) return true;
        return false;
      },
    });
    let paneActive = true;
    f.push({
      id: "pane",
      active: () => paneActive,
      onKey: (key) => {
        calls.push("pane");
        if (key.char === "n") return true;
        return false;
      },
    });
    const disposeModal = f.push({
      id: "modal",
      onKey: (key) => {
        calls.push("modal");
        if (key.name === "escape") return true;
        return false;
      },
    });

    // esc: modal consumes it.
    f.dispatchKey(k("escape"), mkCtx());
    expect(calls).toEqual(["modal"]);

    // n: modal doesn't consume, pane does.
    calls.length = 0;
    f.dispatchKey(k("n", { char: "n" }), mkCtx());
    expect(calls).toEqual(["modal", "pane"]);

    // ctrl+c: bubbles all the way up.
    calls.length = 0;
    f.dispatchKey(k("c", { ctrl: true }), mkCtx());
    expect(calls).toEqual(["modal", "pane", "global"]);

    // Close the modal, rerun esc: pane doesn't consume, bubbles to global.
    disposeModal();
    calls.length = 0;
    f.dispatchKey(k("escape"), mkCtx());
    expect(calls).toEqual(["pane", "global"]);
  });

  it("handlers that pop scopes mid-dispatch don't break the pass", () => {
    const f = createFocusManager();
    let disposeInner: (() => void) | null = null;
    const outerCalls: string[] = [];
    f.push({
      id: "outer",
      onKey: () => { outerCalls.push("outer"); return true; },
    });
    disposeInner = f.push({
      id: "inner",
      onKey: () => { disposeInner!(); return false; }, // pop ourselves, bubble up
    });
    f.dispatchKey(k("x"), mkCtx());
    expect(outerCalls).toEqual(["outer"]);
    expect(f.stack().map(s => s.id)).toEqual(["outer"]);
  });
});

describe("dispatchMouse — same semantics for mouse", () => {
  it("bubbles with active predicate", () => {
    const f = createFocusManager();
    const calls: string[] = [];
    f.push({ id: "o", onMouse: () => { calls.push("o"); return false; } });
    f.push({ id: "i", onMouse: () => { calls.push("i"); return false; } });
    f.dispatchMouse(me(), mkCtx());
    expect(calls).toEqual(["i", "o"]);
  });

  it("scopes without onMouse don't intercept key events (and vice versa)", () => {
    const f = createFocusManager();
    let keyCount = 0, mouseCount = 0;
    f.push({ id: "a", onKey: () => { keyCount++; return true; } });
    f.push({ id: "b", onMouse: () => { mouseCount++; return true; } });
    f.dispatchKey(k("x"), mkCtx());
    f.dispatchMouse(me(), mkCtx());
    expect(keyCount).toBe(1);
    expect(mouseCount).toBe(1);
  });
});
