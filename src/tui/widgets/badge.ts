// badge — a small SRCL-style status chip.
//
// SRCL's Badge is an uppercase, monospace, padded label on a filled
// background (`background: var(--theme-border)`, `padding: 0 1ch`,
// `text-transform: uppercase`). This mirrors that look in the terminal and
// generalizes it with semantic status variants (the inventory calls out
// "available / dead / dnd / host" style status).
//
// It's a pure builder that returns a `TextNode` — plain UI data, styled
// only with semantic color tokens (no raw ANSI, no terminal-only escape) —
// so a non-terminal (web/React) backend can render the same description
// from the same call. This is the "B-ready" authoring pattern: components
// are functions over the shared node/token model, not terminal-specific
// draw code.

import { text } from "../builders.ts";
import type { TextNode, Color } from "../nodes.ts";

export type BadgeVariant = "neutral" | "ok" | "warn" | "error" | "accent" | "info";

export interface BadgeOptions {
  /** Status variant. `neutral` (default) is the plain SRCL chip; the others
   *  color the label (or the fill, with `solid`). */
  variant?: BadgeVariant;
  /** Fill the chip with the variant color and use primary text on top,
   *  instead of coloring the label on a muted fill. Default false.
   *  Ignored for `neutral` (there is no distinct neutral fill color). */
  solid?: boolean;
  /** Uppercase the label like SRCL. Default true. */
  uppercase?: boolean;
  /** Bold text. Default false. */
  bold?: boolean;
}

/** Variant → semantic color. `neutral` uses the primary text color so a
 *  neutral chip reads as plain text on the muted fill. */
const VARIANT_COLOR: Record<BadgeVariant, Color> = {
  neutral: "primary",
  ok: "ok",
  warn: "warn",
  error: "error",
  accent: "accent",
  info: "info",
};

/** A status chip: `badge("live", { variant: "ok" })` → an uppercase " LIVE "
 *  label. Returns a `TextNode`, styled with semantic tokens only. */
export function badge(label: string, opts: BadgeOptions = {}): TextNode {
  const variant = opts.variant ?? "neutral";
  const shown = (opts.uppercase ?? true) ? label.toUpperCase() : label;
  const padded = ` ${shown} `; // SRCL pads 1ch on each side
  const bold = opts.bold ?? false;
  const variantColor = VARIANT_COLOR[variant];

  if (opts.solid && variant !== "neutral") {
    // Filled chip: variant color as the background, primary text on top.
    return text(padded, { fg: "primary", background: variantColor, bold });
  }
  // Subtle chip: variant colors the label on a muted, border-toned fill.
  return text(padded, { fg: variantColor, background: "border", bold });
}
