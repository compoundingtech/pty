import { describe, it, expect } from "vitest";
import {
  SEMANTIC_SLOTS, resolveSemantic, semanticColorNames, themeTokens,
  resolveColor, themes, type SemanticColor,
} from "../src/tui/index.ts";

const ALL_THEMES = Object.values(themes);
const NAMES = semanticColorNames();

describe("semantic token resolution", () => {
  it("maps each semantic name to its theme slot", () => {
    const theme = themes.coolBlue!;
    for (const name of NAMES) {
      expect(resolveSemantic(name, theme)).toEqual(theme[SEMANTIC_SLOTS[name]]);
    }
  });

  it("passes [r,g,b] triples through unchanged", () => {
    expect(resolveSemantic([12, 34, 56], themes.coolBlue!)).toEqual([12, 34, 56]);
  });

  it("resolves undefined / unknown names to null (host default)", () => {
    expect(resolveSemantic(undefined, themes.coolBlue!)).toBeNull();
    expect(resolveSemantic("nope" as SemanticColor, themes.coolBlue!)).toBeNull();
  });

  it("covers exactly the 9 semantic colors", () => {
    expect(NAMES.sort()).toEqual(
      ["accent", "border", "error", "info", "muted", "ok", "primary", "secondary", "warn"],
    );
  });
});

describe("renderer.resolveColor delegates to resolveSemantic (no behavior change)", () => {
  it("returns identical results for every semantic name across every theme", () => {
    for (const theme of ALL_THEMES) {
      for (const name of NAMES) {
        expect(resolveColor(name, theme)).toEqual(resolveSemantic(name, theme));
      }
    }
  });

  it("still honors the historical slot mapping (spot checks)", () => {
    const t = themes.coolBlue!;
    expect(resolveColor("accent", t)).toEqual(t.fgAc);
    expect(resolveColor("error", t)).toEqual(t.err); // "error" name → err slot
    expect(resolveColor("muted", t)).toEqual(t.fgMu);
    expect(resolveColor("primary", t)).toEqual(t.fg1);
  });
});

describe("themeTokens serialization", () => {
  it("emits a name→RGB map for all semantic tokens", () => {
    const t = themes.coolBlue!;
    const tok = themeTokens(t);
    expect(Object.keys(tok).sort()).toEqual([...NAMES].sort());
    expect(tok.accent).toEqual(t.fgAc);
    expect(tok.border).toEqual(t.border);
  });

  it("serializes the null-passthrough 'terminal' theme to all-null tokens", () => {
    const tok = themeTokens(themes.terminal!);
    expect(Object.values(tok).every((v) => v === null)).toBe(true);
  });
});
