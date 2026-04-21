import { describe, it, expect } from "vitest";
import {
  createTabsState, handleTabsMouse, type TabDef,
} from "../src/tui/widgets/tabs.ts";
import type { MouseEvent } from "../src/tui/input.ts";

function me(x: number, y: number, action: MouseEvent["action"] = "press"): MouseEvent {
  return { kind: "mouse", action, button: "left", x, y, ctrl: false, alt: false, shift: false };
}

const tabs: TabDef[] = [
  { id: "inbox",  label: "Inbox"  },   // "[ Inbox ]" width = 5 + 4 = 9
  { id: "sent",   label: "Sent"   },   // "[ Sent ]" width = 4 + 4 = 8
  { id: "drafts", label: "Drafts" },   // width = 10
];

// Rendered: "[ Inbox ]  [ Sent ]  [ Drafts ]" = 9 + 2 + 8 + 2 + 10 = 31 cols.
const rect = { x: 0, y: 2, width: 40, height: 1 };

describe("handleTabsMouse", () => {
  it("click on the first tab selects it", () => {
    const s = { activeId: "sent" };
    const r = handleTabsMouse(s, tabs, me(3, 2), rect);
    expect(r?.activeId).toBe("inbox");
  });

  it("click on the middle tab selects it", () => {
    const s = createTabsState(tabs);
    // Middle tab starts at col 9 + 2 = 11, width 8.
    const r = handleTabsMouse(s, tabs, me(15, 2), rect);
    expect(r?.activeId).toBe("sent");
  });

  it("click in the gap between tabs is ignored", () => {
    const s = createTabsState(tabs);
    const r = handleTabsMouse(s, tabs, me(10, 2), rect);
    expect(r).toBeNull();
  });

  it("click on a different row is ignored", () => {
    const s = createTabsState(tabs);
    const r = handleTabsMouse(s, tabs, me(3, 5), rect);
    expect(r).toBeNull();
  });

  it("non-press / non-left-button events are ignored", () => {
    const s = createTabsState(tabs);
    expect(handleTabsMouse(s, tabs, me(3, 2, "release"), rect)).toBeNull();
    expect(handleTabsMouse(s, tabs, { ...me(3, 2), button: "right" }, rect)).toBeNull();
  });
});
