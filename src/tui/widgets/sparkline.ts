// Sparkline — a compact inline chart drawn with unicode block characters.
// One character per sample; height encoded via eight steps from " " (empty)
// through "\u2581" (lower eighth) up to "\u2588" (full block).
//
// Pure: given a numeric series and a width, returns the string. Wrap it in
// text(...) / canvas(...) to place it anywhere.

import { text } from "../builders.ts";
import type { UINode, Color } from "../nodes.ts";

/** 0..8 block glyph levels used to pack a sample into a single cell. */
const BLOCKS = [" ", "\u2581", "\u2582", "\u2583", "\u2584",
                "\u2585", "\u2586", "\u2587", "\u2588"];

export interface SparklineOptions {
  /** Explicit render width. If omitted, renders `series.length` cells. */
  width?: number;
  /** Explicit min/max — useful when the bounds should be stable across
   *  frames (e.g. 0..100 for a CPU %) rather than per-sample. */
  min?: number;
  max?: number;
}

/** Render `series` as a unicode sparkline string. NaN / Infinity samples
 *  render as the empty cell. */
export function sparklineString(series: readonly number[], opts: SparklineOptions = {}): string {
  const width = Math.max(0, Math.floor(opts.width ?? series.length));
  if (width === 0 || series.length === 0) return "";

  // If the caller wants a fixed width, sample the tail of the series.
  // This matches the "scrolling window" UX of metric sparklines.
  const slice = series.length <= width
    ? series
    : series.slice(series.length - width);

  let lo = opts.min;
  let hi = opts.max;
  if (lo == null || hi == null) {
    let ilo = Infinity, ihi = -Infinity;
    for (const v of slice) {
      if (!Number.isFinite(v)) continue;
      if (v < ilo) ilo = v;
      if (v > ihi) ihi = v;
    }
    if (!Number.isFinite(ilo)) ilo = 0;
    if (!Number.isFinite(ihi)) ihi = 1;
    if (lo == null) lo = ilo;
    if (hi == null) hi = ihi;
  }

  const range = hi! - lo!;
  const out: string[] = [];
  // Left-pad if the series is shorter than width.
  const pad = width - slice.length;
  for (let i = 0; i < pad; i++) out.push(BLOCKS[0]);

  for (const v of slice) {
    if (!Number.isFinite(v)) { out.push(BLOCKS[0]); continue; }
    if (range <= 0) {
      // All samples equal — pick the middle block so there's *something*.
      out.push(BLOCKS[4]);
      continue;
    }
    const frac = (v - lo!) / range;
    const idx = Math.max(0, Math.min(8, Math.round(frac * 8)));
    out.push(BLOCKS[idx]);
  }
  return out.join("");
}

/** Convenience: render the sparkline as a text node with a given color. */
export function sparkline(series: readonly number[], opts: SparklineOptions & { color?: Color } = {}): UINode {
  return text(sparklineString(series, opts), opts.color ?? "accent");
}
