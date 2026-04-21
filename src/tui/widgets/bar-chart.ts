// Bar chart — vertical-bar histogram rendered with unicode block characters.
// Good for "CPU per core", power-draw over time, any small-cardinality
// histogram. For a high-cardinality scrolling chart use `sparkline` instead.
//
// Each bar is `barWidth` columns wide (default 2) with a one-column gap
// between bars (default). The bar height is encoded with the same 8-step
// block scale as sparkline, but here we use `height` rows per bar instead
// of a single row.

import { canvas } from "../builders.ts";
import type { UINode, Color } from "../nodes.ts";

const BLOCKS = [" ", "\u2581", "\u2582", "\u2583", "\u2584",
                "\u2585", "\u2586", "\u2587", "\u2588"];

export interface BarChartItem {
  label?: string;
  value: number;
  /** Per-bar color override. Falls back to `opts.color`. */
  color?: Color;
}

export interface BarChartOptions {
  /** Total canvas height in rows (excluding label row). Default 6. */
  height?: number;
  /** Width of each bar in columns. Default 2. */
  barWidth?: number;
  /** Gap between bars in columns. Default 1. */
  gap?: number;
  /** Lower bound of the value axis. Default min(values). */
  min?: number;
  /** Upper bound of the value axis. Default max(values). */
  max?: number;
  /** Default bar color. Default "accent". */
  color?: Color;
  /** Render short labels under each bar (first char of `item.label`). */
  showLabels?: boolean;
  /** Color for labels. Default "muted". */
  labelColor?: Color;
}

/** Render a bar chart as a Canvas node. The caller places it inside a
 *  row/column/panel like any other UINode. */
export function barChart(items: readonly BarChartItem[], opts: BarChartOptions = {}): UINode {
  const height = Math.max(2, Math.floor(opts.height ?? 6));
  const barWidth = Math.max(1, Math.floor(opts.barWidth ?? 2));
  const gap = Math.max(0, Math.floor(opts.gap ?? 1));
  const showLabels = opts.showLabels ?? false;
  const labelColor = opts.labelColor ?? "muted";
  const baseColor = opts.color ?? "accent";

  const values = items.map(it => it.value);
  let lo = opts.min;
  let hi = opts.max;
  if (lo == null) {
    lo = values.length ? Math.min(...values.filter(Number.isFinite)) : 0;
    if (!Number.isFinite(lo)) lo = 0;
  }
  if (hi == null) {
    hi = values.length ? Math.max(...values.filter(Number.isFinite)) : 1;
    if (!Number.isFinite(hi)) hi = 1;
  }
  const range = hi - lo;

  const rowsForBars = height;
  const totalRows = showLabels ? rowsForBars + 1 : rowsForBars;

  return canvas((ctx) => {
    // Draw each bar column.
    items.forEach((item, barIdx) => {
      const v = Number.isFinite(item.value) ? item.value : lo!;
      const fracTotal = range <= 0 ? 0.5 : (v - lo!) / range;
      const clamped = Math.max(0, Math.min(1, fracTotal));
      // Encode the total height in 1/8ths per row. Rows fill from the
      // bottom up, then the top row uses a partial block.
      const totalEighths = Math.round(clamped * rowsForBars * 8);
      const fullRows = Math.floor(totalEighths / 8);
      const topRemainder = totalEighths % 8;

      const barColor = item.color ?? baseColor;
      const leftCol = barIdx * (barWidth + gap);

      // Full rows (from the bottom).
      for (let r = 0; r < fullRows; r++) {
        const y = rowsForBars - 1 - r;
        for (let c = 0; c < barWidth; c++) {
          ctx.write(leftCol + c, y, BLOCKS[8], barColor);
        }
      }
      // Partial top row.
      if (topRemainder > 0) {
        const y = rowsForBars - 1 - fullRows;
        if (y >= 0) {
          for (let c = 0; c < barWidth; c++) {
            ctx.write(leftCol + c, y, BLOCKS[topRemainder], barColor);
          }
        }
      }

      // Label row.
      if (showLabels) {
        const labelStr = (item.label ?? "").slice(0, barWidth);
        if (labelStr.length > 0) {
          ctx.write(leftCol, rowsForBars, labelStr, labelColor);
        }
      }
    });
  }, { height: totalRows });
}
