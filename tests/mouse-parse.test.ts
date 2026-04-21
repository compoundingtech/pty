import { describe, it, expect } from "vitest";
import { parseInput, isMouseEvent, type MouseEvent } from "../src/tui/input.ts";

function parse(s: string) {
  return parseInput(Buffer.from(s, "utf8"));
}

function mouse(s: string): MouseEvent {
  const events = parse(s);
  expect(events).toHaveLength(1);
  expect(isMouseEvent(events[0])).toBe(true);
  return events[0] as MouseEvent;
}

describe("SGR mouse parsing", () => {
  it("left-button press at (col 10, row 5) — 0-based output", () => {
    const e = mouse("\x1b[<0;10;5M");
    expect(e.action).toBe("press");
    expect(e.button).toBe("left");
    expect(e.x).toBe(9);
    expect(e.y).toBe(4);
    expect(e.ctrl).toBe(false);
    expect(e.alt).toBe(false);
    expect(e.shift).toBe(false);
  });

  it("left-button release — lowercase `m`", () => {
    const e = mouse("\x1b[<0;10;5m");
    expect(e.action).toBe("release");
    expect(e.button).toBe("left");
  });

  it("middle and right buttons", () => {
    expect(mouse("\x1b[<1;1;1M").button).toBe("middle");
    expect(mouse("\x1b[<2;1;1M").button).toBe("right");
  });

  it("drag: motion flag + button low bits", () => {
    // button code = 0 (left) | 0x20 (motion) = 32
    const e = mouse("\x1b[<32;5;5M");
    expect(e.action).toBe("drag");
    expect(e.button).toBe("left");
  });

  it("hover: motion flag + button=none (3)", () => {
    // button code = 3 (none) | 0x20 (motion) = 35
    const e = mouse("\x1b[<35;5;5M");
    expect(e.action).toBe("move");
    expect(e.button).toBe("none");
  });

  it("scroll wheel: bit 6 = 64 (up) / 65 (down)", () => {
    const up = mouse("\x1b[<64;10;10M");
    expect(up.action).toBe("scrollUp");
    expect(up.button).toBe("none");
    const down = mouse("\x1b[<65;10;10M");
    expect(down.action).toBe("scrollDown");
  });

  it("modifiers: shift / alt / ctrl from bits 2/3/4", () => {
    // button = left (0) + shift (4) + alt (8) + ctrl (16) = 28
    const e = mouse("\x1b[<28;1;1M");
    expect(e.shift).toBe(true);
    expect(e.alt).toBe(true);
    expect(e.ctrl).toBe(true);
  });

  it("interleaves mouse with key events in a single chunk", () => {
    const events = parse("a\x1b[<0;3;4Mb");
    expect(events).toHaveLength(3);
    expect((events[0] as any).name).toBe("a");
    expect(isMouseEvent(events[1])).toBe(true);
    expect((events[2] as any).name).toBe("b");
  });

  it("parseKey (legacy) filters out mouse events", async () => {
    const { parseKey } = await import("../src/tui/input.ts");
    const keys = parseKey(Buffer.from("a\x1b[<0;3;4Mb", "utf8"));
    expect(keys.map(k => k.name)).toEqual(["a", "b"]);
  });
});
