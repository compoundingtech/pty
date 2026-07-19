// barProgress / barLoader — SRCL-style progress bars.
//
// SRCL ships two: BarProgress (a light `░` texture fill proportional to
// progress, on a subtle track) and BarLoader (a solid bar whose width is the
// progress). This mirrors both as pure text builders.
//
// These are deliberately separate from the framework's existing node-based
// `progressBar()` (a fixed `█`/`░` ProgressBarNode). Each renders a bar
// *string* — the fill glyph for the filled cells and spaces for the track —
// as a single `TextNode` colored with the fill color, on a subtle `background`
// fill (SRCL's `--theme-border-subdued`). The track is spaces, so only the
// background shows there; that keeps it a single plain node whose background
// paints correctly (a highlighted node drops its background on the highlighted
// cells in the current renderer). Styled with semantic tokens only (B-ready).

import { text } from "../builders.ts";
import type { TextNode, Color } from "../nodes.ts";

export interface BarOptions {
  /** Total width of the bar in cells. Default 20. */
  width?: number;
  /** Color of the filled portion. Default "accent". */
  color?: Color;
  /** Track background fill (SRCL's `--theme-border-subdued`). Default "border";
   *  pass `null` for no fill. */
  background?: Color | null;
}

function clampPercent(p: number): number {
  return Math.min(100, Math.max(0, p));
}

/** Shared bar builder: `fillChar` for the filled cells, spaces for the track. */
function bar(percent: number, opts: BarOptions, fillChar: string): TextNode {
  const width = opts.width ?? 20;
  const filled = Math.round((clampPercent(percent) / 100) * width);
  const str = fillChar.repeat(filled) + " ".repeat(Math.max(0, width - filled));
  const color = opts.color ?? "accent";
  const background = opts.background === undefined ? "border" : opts.background;
  return text(str, { fg: color, ...(background != null ? { background } : {}) });
}

/** SRCL BarProgress: a light `░` texture fill proportional to `percent`
 *  (0–100) on a subtle track. */
export function barProgress(percent: number, opts: BarOptions = {}): TextNode {
  return bar(percent, opts, "░"); // ░
}

/** SRCL BarLoader: a solid `█` bar whose filled length is `percent` (0–100). */
export function barLoader(percent: number, opts: BarOptions = {}): TextNode {
  return bar(percent, opts, "█"); // █
}
