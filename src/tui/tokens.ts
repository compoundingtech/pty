// tokens — the single source for semantic design-token resolution.
//
// Semantic color names (`accent`, `muted`, `ok`, …) are the vocabulary every
// widget styles with. This module owns the one mapping from those names to a
// theme's concrete slots, the canonical resolution the renderer delegates to,
// and a serializer that emits a theme's tokens as a framework-neutral
// name→RGB map — the form a non-terminal (web) layer consumes so the same
// palette drives terminal and web alike.
//
// It is a leaf consumer (imports only the `Theme`/`Color` shapes), so nothing
// here depends on the ANSI/render code; the web side can resolve and serialize
// tokens without pulling in terminal rendering.

import type { Theme } from "./colors.ts";
import type { Color, SemanticColor } from "./nodes.ts";

/** The one mapping from a semantic color name to the theme slot that backs
 *  it. This is the single source of truth — the renderer and any serializer
 *  resolve through it. */
export const SEMANTIC_SLOTS: Record<SemanticColor, keyof Theme> = {
  primary: "fg1",
  secondary: "fg2",
  accent: "fgAc",
  muted: "fgMu",
  ok: "ok",
  warn: "warn",
  error: "err",
  info: "info",
  border: "border",
};

export type Rgb = [number, number, number];

/** Resolve a `Color` to concrete RGB against a theme: a semantic name maps
 *  through `SEMANTIC_SLOTS`; an `[r,g,b]` triple passes through; anything
 *  else (undefined / unknown) resolves to `null` ("use the terminal
 *  default"). This is the canonical resolution `renderer.resolveColor`
 *  delegates to. */
export function resolveSemantic(color: Color | undefined, theme: Theme): Rgb | null {
  if (!color) return null;
  if (Array.isArray(color)) return color;
  const slot = SEMANTIC_SLOTS[color as SemanticColor];
  return slot ? (theme[slot] as Rgb | null) : null;
}

/** All semantic token names, in declaration order. */
export function semanticColorNames(): SemanticColor[] {
  return Object.keys(SEMANTIC_SLOTS) as SemanticColor[];
}

/** Serialize a theme's semantic tokens to a name→RGB map. The framework-
 *  neutral form a web layer consumes — e.g. emitted as CSS custom properties
 *  (`--accent: rgb(...)`) so the same palette styles terminal and web. A
 *  `null` value means the token defers to the host default. */
export function themeTokens(theme: Theme): Record<SemanticColor, Rgb | null> {
  const out = {} as Record<SemanticColor, Rgb | null>;
  for (const name of semanticColorNames()) {
    out[name] = resolveSemantic(name, theme);
  }
  return out;
}
