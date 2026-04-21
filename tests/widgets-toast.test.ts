import { describe, it, expect } from "vitest";
import {
  createToastQueue, pushToast, pruneExpired, dismissToast, renderToasts,
} from "../src/tui/widgets/toast.ts";

describe("toast queue", () => {
  it("push adds a toast with an expiresAt derived from now + duration", () => {
    const t0 = 1000;
    const q = pushToast(createToastQueue(), "saved", { now: t0, durationMs: 2000 });
    expect(q.toasts).toHaveLength(1);
    expect(q.toasts[0].text).toBe("saved");
    expect(q.toasts[0].expiresAt).toBe(3000);
  });

  it("pruneExpired drops toasts whose expiresAt passed", () => {
    const q0 = pushToast(pushToast(createToastQueue(),
      "a", { now: 1000, durationMs: 500 }),
      "b", { now: 1000, durationMs: 2000 });
    const q1 = pruneExpired(q0, 1600);
    expect(q1.toasts.map(t => t.text)).toEqual(["b"]);
  });

  it("returns the same reference when nothing pruned (stable for signals)", () => {
    const q0 = pushToast(createToastQueue(), "x", { now: 1000, durationMs: 5000 });
    const q1 = pruneExpired(q0, 1100);
    expect(q1).toBe(q0);
  });

  it("dismissToast removes by id", () => {
    const q0 = pushToast(createToastQueue(), "a", { now: 0 });
    const id = q0.toasts[0].id;
    const q1 = dismissToast(q0, id);
    expect(q1.toasts).toHaveLength(0);
  });

  it("kind colours the glyph", () => {
    const q = pushToast(createToastQueue(), "boom", { kind: "error", now: 0 });
    const node = renderToasts(q);
    const firstRow = (node as any).children[0];
    expect(firstRow.children[0].color).toBe("error");
  });

  it("renders one row per toast", () => {
    const q0 = createToastQueue();
    const q1 = pushToast(q0, "one", { now: 0 });
    const q2 = pushToast(q1, "two", { now: 0 });
    const node = renderToasts(q2);
    expect((node as any).children).toHaveLength(2);
  });
});
