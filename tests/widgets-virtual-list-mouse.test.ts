import { describe, it, expect } from "vitest";
import {
  createVirtualListState, handleVirtualMouse, virtualWindow,
} from "../src/tui/widgets/virtual-list.ts";
import type { MouseEvent } from "../src/tui/input.ts";

function me(partial: Partial<MouseEvent> & { action: MouseEvent["action"] }): MouseEvent {
  return {
    kind: "mouse",
    button: "left",
    x: 0, y: 0,
    ctrl: false, alt: false, shift: false,
    ...partial,
  };
}

const rect = { x: 2, y: 3, width: 40, height: 10 };

describe("handleVirtualMouse", () => {
  it("ignores events outside the rect", () => {
    const s = createVirtualListState(100, 10);
    const r = handleVirtualMouse(s, me({ action: "press", x: 0, y: 0 }), rect);
    expect(r.action).toBe("none");
  });

  it("left click selects the row under the cursor (accounting for offset)", () => {
    const s = { ...createVirtualListState(100, 10), offset: 20, selectedIndex: 20 };
    // Click at y=5, rect.y=3 → rowWithinRect=2 → index=22.
    const r = handleVirtualMouse(s, me({ action: "press", x: 5, y: 5 }), rect);
    expect(r.action).toBe("activate");
    expect(r.state.selectedIndex).toBe(22);
  });

  it("scroll up moves selection back by 3", () => {
    const s = { ...createVirtualListState(100, 10), selectedIndex: 20, offset: 15 };
    const r = handleVirtualMouse(s, me({ action: "scrollUp", x: 5, y: 5, button: "none" }), rect);
    expect(r.action).toBe("moved");
    expect(r.state.selectedIndex).toBe(17);
  });

  it("scroll down moves selection forward by 3", () => {
    const s = { ...createVirtualListState(100, 10), selectedIndex: 20, offset: 15 };
    const r = handleVirtualMouse(s, me({ action: "scrollDown", x: 5, y: 5, button: "none" }), rect);
    expect(r.action).toBe("moved");
    expect(r.state.selectedIndex).toBe(23);
  });

  it("click outside the virtual rows (past `total`) is a no-op", () => {
    const s = { ...createVirtualListState(5, 10), offset: 0 };
    // Click at y below the actual data rows.
    const r = handleVirtualMouse(s, me({ action: "press", x: 5, y: 9 }), rect);
    expect(r.action).toBe("none");
  });
});
