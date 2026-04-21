import { describe, it, expect } from "vitest";
import {
  createConfirm, handleConfirmKey, confirmPanel,
} from "../src/tui/widgets/confirm.ts";
import type { KeyEvent } from "../src/tui/input.ts";

function k(name: string, opts: Partial<KeyEvent> = {}): KeyEvent {
  return { name, char: opts.char, ctrl: opts.ctrl ?? false, alt: opts.alt ?? false, shift: opts.shift ?? false };
}

describe("confirm — state", () => {
  it("defaults focus to 'no' for safety", () => {
    const s = createConfirm({ title: "Delete?", message: "really?" });
    expect(s.focused).toBe("no");
  });

  it("respects defaultFocus option", () => {
    const s = createConfirm({ title: "", message: "", defaultFocus: "yes" });
    expect(s.focused).toBe("yes");
  });
});

describe("confirm — keys", () => {
  const s0 = createConfirm({ title: "x", message: "y" });

  it("arrows / tab toggle focus but don't commit", () => {
    const r1 = handleConfirmKey(s0, k("right"));
    expect(r1.action).toBe("pending");
    expect(r1.state.focused).toBe("yes");
    const r2 = handleConfirmKey(r1.state, k("tab"));
    expect(r2.state.focused).toBe("no");
    expect(r2.action).toBe("pending");
  });

  it("return commits the focused button", () => {
    const r1 = handleConfirmKey(s0, k("return"));
    expect(r1.action).toBe("no");
    const focusedYes = { ...s0, focused: "yes" as const };
    const r2 = handleConfirmKey(focusedYes, k("return"));
    expect(r2.action).toBe("yes");
  });

  it("escape is always 'no'", () => {
    const r = handleConfirmKey({ ...s0, focused: "yes" }, k("escape"));
    expect(r.action).toBe("no");
  });

  it("y / n shortcuts commit directly", () => {
    expect(handleConfirmKey(s0, k("y", { char: "y" })).action).toBe("yes");
    expect(handleConfirmKey(s0, k("Y", { char: "Y" })).action).toBe("yes");
    expect(handleConfirmKey(s0, k("n", { char: "n" })).action).toBe("no");
  });
});

describe("confirm — rendering", () => {
  it("renders a panel titled from state", () => {
    const s = createConfirm({ title: "Drop the bomb?", message: "boom" });
    const node = confirmPanel(s);
    expect((node as any).type).toBe("panel");
    expect((node as any).title).toBe("Drop the bomb?");
  });
});
