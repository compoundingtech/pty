import { describe, it, expect } from "vitest";
import { parseKey } from "../src/tui/input.ts";

function parse(s: string) {
  return parseKey(Buffer.from(s, "utf8"));
}

describe("parseKey basics", () => {
  it("parses a plain printable character", () => {
    expect(parse("a")).toEqual([
      { name: "a", char: "a", ctrl: false, alt: false, shift: false },
    ]);
  });

  it("parses return / tab / backspace as named keys", () => {
    expect(parse("\r")).toEqual([{ name: "return", ctrl: false, alt: false, shift: false }]);
    expect(parse("\t")).toEqual([{ name: "tab", ctrl: false, alt: false, shift: false }]);
    expect(parse("\x7f")).toEqual([{ name: "backspace", ctrl: false, alt: false, shift: false }]);
  });

  it("parses a bare ESC as escape", () => {
    expect(parse("\x1b")).toEqual([{ name: "escape", ctrl: false, alt: false, shift: false }]);
  });

  it("parses arrow keys", () => {
    expect(parse("\x1b[A")).toEqual([{ name: "up", ctrl: false, alt: false, shift: false }]);
    expect(parse("\x1b[B")).toEqual([{ name: "down", ctrl: false, alt: false, shift: false }]);
    expect(parse("\x1b[C")).toEqual([{ name: "right", ctrl: false, alt: false, shift: false }]);
    expect(parse("\x1b[D")).toEqual([{ name: "left", ctrl: false, alt: false, shift: false }]);
  });

  it("parses Ctrl+letter", () => {
    // Ctrl+A = 0x01
    expect(parse("\x01")).toEqual([{ name: "a", ctrl: true, alt: false, shift: false }]);
  });

  it("parses Alt+letter", () => {
    expect(parse("\x1ba")).toEqual([{ name: "a", char: "a", ctrl: false, alt: true, shift: false }]);
  });
});

describe("parseKey: shift+tab (backtab)", () => {
  // Two wire encodings matter in the wild:
  //   - Legacy xterm: ESC [ Z (CSI-Z). Most terminals emit this by default.
  //   - Kitty keyboard protocol: ESC [ 9 ; 2 u (code 9 = tab, modifiers 2 = shift+1).
  // Both must produce the canonical name "backtab" so consumers can key on
  // key.name === "backtab" without caring which terminal the client is in.

  it("ESC[Z -> backtab", () => {
    expect(parse("\x1b[Z")).toEqual([
      { name: "backtab", ctrl: false, alt: false, shift: true },
    ]);
  });

  it("kitty encoding ESC[9;2u -> backtab", () => {
    expect(parse("\x1b[9;2u")).toEqual([
      { name: "backtab", ctrl: false, alt: false, shift: true },
    ]);
  });

  it("kitty shift+ctrl+tab: backtab with ctrl modifier", () => {
    // mods = ctrl(4) + shift(1) = 5, wire = 5 + 1 = 6
    expect(parse("\x1b[9;6u")).toEqual([
      { name: "backtab", ctrl: true, alt: false, shift: true },
    ]);
  });

  it("kitty plain tab (no shift) stays as tab char via existing path", () => {
    // mods = 0, wire = 1
    const events = parse("\x1b[9;1u");
    expect(events).toHaveLength(1);
    expect(events[0].char).toBe("\t");
    expect(events[0].shift).toBe(false);
  });
});

describe("parseKey: kitty protocol modifier extraction", () => {
  // Regression guard: before this change, shift was never extracted from
  // the kitty modifier bitmask. Now it is — keep it that way.
  it("extracts shift modifier from kitty sequences", () => {
    // 'a' with shift, modifiers wire = shift(1) + 1 = 2
    expect(parse("\x1b[97;2u")).toEqual([
      { name: "a", char: "a", ctrl: false, alt: false, shift: true },
    ]);
  });

  it("extracts all three modifiers (ctrl+alt+shift) from one keystroke", () => {
    // mods = ctrl(4) + alt(2) + shift(1) = 7, wire = 8
    expect(parse("\x1b[97;8u")).toEqual([
      { name: "a", char: "a", ctrl: true, alt: true, shift: true },
    ]);
  });
});
