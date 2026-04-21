import { describe, it, expect } from "vitest";
import {
  createTabsState, selectTab, nextTab, prevTab, handleTabsKey, renderTabs,
  type TabDef,
} from "../src/tui/widgets/tabs.ts";
import type { KeyEvent } from "../src/tui/input.ts";

function k(name: string, opts: Partial<KeyEvent> = {}): KeyEvent {
  return { name, char: opts.char, ctrl: opts.ctrl ?? false, alt: opts.alt ?? false, shift: opts.shift ?? false };
}

const tabs: TabDef[] = [
  { id: "inbox", label: "Inbox" },
  { id: "sent", label: "Sent" },
  { id: "drafts", label: "Drafts" },
];

describe("tabs — state", () => {
  it("createTabsState defaults to the first tab", () => {
    expect(createTabsState(tabs).activeId).toBe("inbox");
  });

  it("selectTab changes activeId", () => {
    const s = selectTab(createTabsState(tabs), "sent");
    expect(s.activeId).toBe("sent");
  });

  it("next / prev wrap around", () => {
    const s0 = createTabsState(tabs);
    const s1 = nextTab(s0, tabs);
    const s2 = nextTab(s1, tabs);
    const s3 = nextTab(s2, tabs);
    expect([s1, s2, s3].map(s => s.activeId)).toEqual(["sent", "drafts", "inbox"]);
    const sp = prevTab(s0, tabs);
    expect(sp.activeId).toBe("drafts");
  });
});

describe("tabs — key dispatch", () => {
  it("ctrl+tab / ctrl+backtab cycle", () => {
    const s0 = createTabsState(tabs);
    const fwd = handleTabsKey(s0, tabs, k("tab", { ctrl: true }));
    expect(fwd?.activeId).toBe("sent");
    const back = handleTabsKey(s0, tabs, k("backtab", { ctrl: true }));
    expect(back?.activeId).toBe("drafts");
  });

  it("number keys jump directly", () => {
    const s0 = createTabsState(tabs);
    const r = handleTabsKey(s0, tabs, k("3", { char: "3" }));
    expect(r?.activeId).toBe("drafts");
  });

  it("returns null for unrelated keys", () => {
    const s0 = createTabsState(tabs);
    expect(handleTabsKey(s0, tabs, k("x"))).toBeNull();
    // Non-ctrl tab is owned by forms, not tabs — let it bubble.
    expect(handleTabsKey(s0, tabs, k("tab"))).toBeNull();
  });
});

describe("tabs — rendering", () => {
  it("active tab is bracketed, inactive tabs padded", () => {
    const s = createTabsState(tabs);
    const node = renderTabs(s, tabs);
    // Children alternate tab nodes and separators.
    const children = (node as any).children;
    const active = children.find((c: any) => c.text?.includes("[ Inbox ]"));
    const inactive = children.find((c: any) => c.text?.includes("Sent"));
    expect(active).toBeDefined();
    expect(inactive).toBeDefined();
  });
});
