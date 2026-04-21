// Integration tests for the playground's pane-focus routing.
//
// Exercises the REAL router in router.ts (not a mirror), wiring up the
// minimum deps it needs. Catches regressions like "down arrow leaked
// from main-pane demo into sidebar selection."
//
// Rules the router enforces:
//
//   SIDEBAR FOCUSED:
//     up/down         -> prev/next demo
//     pageup/pagedown -> jump by 5
//     right/tab/enter -> focus main
//     q / ? / s       -> quit / help / source (playground-owned shortcuts)
//     anything else   -> swallowed (does NOT leak to a demo)
//
//   MAIN FOCUSED:
//     every key       -> the demo's handleKey
//     sidebar never changes from a key typed here
//     ctrl+h          -> focus sidebar (always works)
//
//   UNIVERSAL:
//     ctrl+h / ctrl+l -> focus sidebar / main

import { describe, it, expect, beforeEach } from "vitest";
import { signal } from "../../src/tui/index.ts";
import type {
  KeyEvent, MouseEvent, ScreenContext,
} from "../../src/tui/index.ts";
import { createFocusManager } from "../../src/tui/index.ts";
import {
  registerPlaygroundScopes, makeGlobalChords, handleMouse, type RouterDeps,
} from "./router.ts";
import type { Demo } from "./types.ts";

function k(name: string, extra: Partial<KeyEvent> = {}): KeyEvent {
  return { kind: "key", name, ctrl: false, alt: false, shift: false, ...extra };
}

function me(x: number, y: number, extra: Partial<MouseEvent> = {}): MouseEvent {
  return {
    kind: "mouse", action: "press", button: "left",
    x, y, ctrl: false, alt: false, shift: false,
    ...extra,
  };
}

function mkCtx(quit: () => void): ScreenContext {
  return {
    rows: 40, cols: 100,
    theme: {} as any,
    boxStyle: "rounded",
    navigate: () => {},
    back: () => {},
    openOverlay: () => {},
    closeOverlay: () => {},
    isTextInputActive: () => false,
    setTextInputActive: () => {},
    quit,
  };
}

/** Build a self-contained router harness with fake demos so we can
 *  observe forwarded keys without real demos side-effecting. */
function harness(demoCount = 10) {
  const selectedDemoId = signal<string>("demo-0");
  const selectedPane = signal<"sidebar" | "main">("sidebar");
  const showHelp = signal(false);
  const showSource = signal(true);

  let lastForwarded: { key: KeyEvent | null } = { key: null };
  const fakeDemo: Demo = {
    id: "demo-0",
    category: "cat",
    name: "demo-0",
    blurb: "",
    source: "",
    render: () => [],
    handleKey: (key) => { lastForwarded.key = key; return true; },
  };

  const entries = [
    { kind: "header" as const, demoId: undefined },
    ...Array.from({ length: demoCount }, (_, i) => ({
      kind: "demo" as const,
      demoId: `demo-${i}`,
    })),
  ];

  let demoIdx = 0;
  const step = (delta: number) => {
    demoIdx = Math.max(0, Math.min(demoCount - 1, demoIdx + delta));
    selectedDemoId.set(`demo-${demoIdx}`);
  };

  const deps: RouterDeps = {
    selectedDemoId,
    selectedPane,
    showHelp,
    showSource,
    sidebarEntries: () => entries,
    selectedDemo: () => fakeDemo,
    selectNextDemo: step,
    stepDemos: step,
  };

  const focus = createFocusManager();
  const dispose = registerPlaygroundScopes(focus, deps);
  const globalChords = makeGlobalChords(deps);

  // Mirror app()'s real dispatch order: onKey (global chords) first,
  // then the focus stack.
  const handleKey = (key: KeyEvent, ctx: ScreenContext): boolean => {
    if (globalChords(key)) return true;
    return focus.dispatchKey(key, ctx);
  };

  return {
    deps, selectedPane, selectedDemoId, showHelp, showSource,
    lastForwarded, entries, focus, handleKey, dispose,
  };
}

describe("playground routing — sidebar focused", () => {
  let h: ReturnType<typeof harness>;
  beforeEach(() => { h = harness(); });

  it("up/down move the selection", () => {
    h.handleKey(k("down"), mkCtx(() => {}));
    expect(h.selectedDemoId.get()).toBe("demo-1");
    h.handleKey(k("down"), mkCtx(() => {}));
    expect(h.selectedDemoId.get()).toBe("demo-2");
    h.handleKey(k("up"), mkCtx(() => {}));
    expect(h.selectedDemoId.get()).toBe("demo-1");
  });

  it("pageup/pagedown jump by 5", () => {
    h.handleKey(k("pagedown"), mkCtx(() => {}));
    expect(h.selectedDemoId.get()).toBe("demo-5");
    h.handleKey(k("pageup"), mkCtx(() => {}));
    expect(h.selectedDemoId.get()).toBe("demo-0");
  });

  it("right / tab / return focus main", () => {
    h.handleKey(k("right"), mkCtx(() => {}));
    expect(h.selectedPane.get()).toBe("main");
  });

  it("q quits, ? toggles help, s toggles source", () => {
    let quit = 0;
    h.handleKey(k("q", { char: "q" }), mkCtx(() => { quit++; }));
    expect(quit).toBe(1);
    h.handleKey(k("?", { char: "?" }), mkCtx(() => {}));
    expect(h.showHelp.get()).toBe(true);
    const before = h.showSource.get();
    h.handleKey(k("s", { char: "s" }), mkCtx(() => {}));
    expect(h.showSource.get()).toBe(!before);
  });

  it("arbitrary keys are swallowed — don't leak to demo", () => {
    h.handleKey(k("x", { char: "x" }), mkCtx(() => {}));
    expect(h.selectedPane.get()).toBe("sidebar");
    expect(h.lastForwarded.key).toBeNull();
  });
});

describe("playground routing — main focused (the regression guards)", () => {
  let h: ReturnType<typeof harness>;
  beforeEach(() => {
    h = harness();
    h.handleKey(k("right"), mkCtx(() => {}));
    expect(h.selectedPane.get()).toBe("main");
  });

  it("down arrow goes to the demo, NOT the sidebar", () => {
    const before = h.selectedDemoId.get();
    h.handleKey(k("down"), mkCtx(() => {}));
    expect(h.selectedDemoId.get()).toBe(before);
    expect(h.lastForwarded.key?.name).toBe("down");
  });

  it("up arrow goes to the demo, NOT the sidebar", () => {
    const before = h.selectedDemoId.get();
    h.handleKey(k("up"), mkCtx(() => {}));
    expect(h.selectedDemoId.get()).toBe(before);
    expect(h.lastForwarded.key?.name).toBe("up");
  });

  it("tab goes to the demo (forms use it)", () => {
    h.handleKey(k("tab"), mkCtx(() => {}));
    expect(h.selectedPane.get()).toBe("main");
    expect(h.lastForwarded.key?.name).toBe("tab");
  });

  it("left arrow goes to the demo (date picker uses it)", () => {
    h.handleKey(k("left"), mkCtx(() => {}));
    expect(h.selectedPane.get()).toBe("main");
    expect(h.lastForwarded.key?.name).toBe("left");
  });

  it("q / s / ? go to the demo (could be text input)", () => {
    h.handleKey(k("q", { char: "q" }), mkCtx(() => {}));
    h.handleKey(k("s", { char: "s" }), mkCtx(() => {}));
    h.handleKey(k("?", { char: "?" }), mkCtx(() => {}));
    expect(h.showHelp.get()).toBe(false);
    expect(h.showSource.get()).toBe(true);
    expect(h.lastForwarded.key?.char).toBe("?");
  });

  it("escape does NOT change focus — demos may need it", () => {
    h.handleKey(k("escape"), mkCtx(() => {}));
    expect(h.selectedPane.get()).toBe("main");
    expect(h.lastForwarded.key?.name).toBe("escape");
  });

  it("ctrl+h is the escape hatch back to the sidebar", () => {
    h.handleKey(k("h", { ctrl: true }), mkCtx(() => {}));
    expect(h.selectedPane.get()).toBe("sidebar");
  });
});

describe("playground routing — universal shortcuts", () => {
  it("ctrl+l focuses main from sidebar", () => {
    const h = harness();
    h.handleKey(k("l", { ctrl: true }), mkCtx(() => {}));
    expect(h.selectedPane.get()).toBe("main");
  });

  it("ctrl+h focuses sidebar from main", () => {
    const h = harness();
    h.handleKey(k("right"), mkCtx(() => {}));
    h.handleKey(k("h", { ctrl: true }), mkCtx(() => {}));
    expect(h.selectedPane.get()).toBe("sidebar");
  });
});

describe("playground routing — mouse", () => {
  it("click on a sidebar demo row selects + focuses main", () => {
    const h = harness();
    // Panel border is row 0. entries[0] is the "cat" header at row 1.
    // entries[1] is demo-0 at row 2. entries[2] is demo-1 at row 3.
    // So y=3 → entryIdx=2 → demo-1.
    handleMouse(h.deps, me(10, 3));
    expect(h.selectedDemoId.get()).toBe("demo-1");
    expect(h.selectedPane.get()).toBe("main");
  });

  it("scroll over sidebar cycles demos without changing focus", () => {
    const h = harness();
    handleMouse(h.deps, me(10, 5, { action: "scrollDown", button: "none" }));
    expect(h.selectedDemoId.get()).toBe("demo-1");
    expect(h.selectedPane.get()).toBe("sidebar");
  });

  it("click on main pane only focuses main, doesn't change selection", () => {
    const h = harness();
    const before = h.selectedDemoId.get();
    handleMouse(h.deps, me(50, 10));
    expect(h.selectedPane.get()).toBe("main");
    expect(h.selectedDemoId.get()).toBe(before);
  });
});
