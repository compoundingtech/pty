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

  it("kitty plain tab (no shift) -> named 'tab' (matches the legacy \\t encoding)", () => {
    // mods = 0, wire = 1. Decodes to the SAME named event as a legacy tab byte
    // so consumers keying on key.name === "tab" work under CSI-u too.
    expect(parse("\x1b[9;1u")).toEqual([
      { name: "tab", ctrl: false, alt: false, shift: false },
    ]);
  });
});

describe("parseKey: kitty CSI-u named special keys", () => {
  // Regression (Nathan): in kitty with the keyboard protocol active, Escape and
  // the other control keys arrive as CSI-u (`ESC[27u`), and the modifiers param
  // is OMITTED when no modifiers are held. Requiring the `;mods` dropped the
  // bare-Escape form entirely (fell through to "unknown CSI" skip), and even the
  // explicit-param form decoded to the raw `\x1b` CHAR instead of a named event
  // — so consumers matching key.name === "escape" never fired (two-stage esc
  // "did nothing"). Both forms must decode to the named key.
  it("ESC[27u (mods omitted) -> escape", () => {
    expect(parse("\x1b[27u")).toEqual([
      { name: "escape", ctrl: false, alt: false, shift: false },
    ]);
  });

  it("ESC[27;1u (explicit no-mods) -> escape", () => {
    expect(parse("\x1b[27;1u")).toEqual([
      { name: "escape", ctrl: false, alt: false, shift: false },
    ]);
  });

  it("ESC[13u -> return, ESC[127u -> backspace (mods omitted)", () => {
    expect(parse("\x1b[13u")).toEqual([
      { name: "return", ctrl: false, alt: false, shift: false },
    ]);
    expect(parse("\x1b[127u")).toEqual([
      { name: "backspace", ctrl: false, alt: false, shift: false },
    ]);
  });

  it("modifiers still decode on a named key: ESC[27;5u -> ctrl+escape", () => {
    // mods = 5 -> bitmask 4 = ctrl.
    expect(parse("\x1b[27;5u")).toEqual([
      { name: "escape", ctrl: true, alt: false, shift: false },
    ]);
  });

  it("a non-special codepoint still decodes to its char: ESC[97u -> 'a'", () => {
    expect(parse("\x1b[97u")).toEqual([
      { name: "a", char: "a", ctrl: false, alt: false, shift: false },
    ]);
  });
});

describe("parseKey: modified arrow keys (ESC[1;mods<letter>)", () => {
  it("option+left (mods=3 → alt) -> left with alt true", () => {
    expect(parse("\x1b[1;3D")).toEqual([
      { name: "left", ctrl: false, alt: true, shift: false },
    ]);
  });
  it("option+right -> right with alt true", () => {
    expect(parse("\x1b[1;3C")).toEqual([
      { name: "right", ctrl: false, alt: true, shift: false },
    ]);
  });
  it("shift+up (mods=2) -> up with shift true", () => {
    expect(parse("\x1b[1;2A")).toEqual([
      { name: "up", ctrl: false, alt: false, shift: true },
    ]);
  });
  it("ctrl+shift+alt+end (mods=8) -> all three modifiers on end", () => {
    expect(parse("\x1b[1;8F")).toEqual([
      { name: "end", ctrl: true, alt: true, shift: true },
    ]);
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
