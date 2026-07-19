// ptyPane — a first-class, reusable widget that renders a live pty session
// really well: a bordered/titled, focus-aware, scrollback-capable pane with
// selection highlighting and cursor-with-scroll reporting.
//
// This generalizes the single-pane render path that pty-layout grew (border
// chrome + `readCells(scrollOffset)` blit + selection + cursor tracking +
// per-pane cell cache) into one widget the framework owns. It renders
// directly into a `CellBuffer` region so the host composites it wherever it
// likes — a single interactive attach, a tiled layout, an IDE pane. Multi-
// pane tiling stays the host's job; this widget renders exactly one pane.
//
// Unlike the base `ptyView` node (renderer.ts), this widget:
//   - draws its own border/title chrome with a focus color;
//   - supports scrollback via `scrollOffset`;
//   - preserves palette indices (fgIndex/bgIndex) so the outer terminal's
//     theme resolves indexed colors — the base node flattens to RGB;
//   - highlights a text selection;
//   - reports the child cursor's on-screen position (or null when the pane
//     is unfocused or the cursor is scrolled off-screen) so the host can
//     place the real terminal cursor;
//   - caches the last cell read per handle and skips re-reading a clean pane.

import { CellBuffer } from "../buffer.ts";
import type { Cell } from "../types.ts";
import type { Rect, PtyHandle, PtyCell } from "../nodes.ts";
import type { Theme, BoxStyle } from "../colors.ts";
import { drawBox, fg as fgAnsi, reset } from "../colors.ts";

/** A text selection within a pane, captured in pane-inner cell coordinates
 *  at the `scrollOffset` in effect when the selection was made. The
 *  highlight tracks the selected *content*, so it stays anchored to the
 *  text as the pane scrolls afterward. */
export interface PtyPaneSelection {
  startRow: number;
  startCol: number;
  endRow: number;
  endCol: number;
  /** The pane scroll offset in effect when the selection coordinates were
   *  captured. Used to translate the highlight to the current scroll. */
  scrollOffset: number;
}

export interface PtyPaneOptions {
  /** Theme — supplies the default border and fill colors. */
  theme: Theme;
  /** Title drawn into the top border. Omit for an untitled box. */
  title?: string;
  /** Focused panes get the accent border color; unfocused get the muted
   *  border color. Default false. */
  focused?: boolean;
  /** Draw a border + title around the content. Default true. When false,
   *  the whole `rect` is content (no inset) and no title is drawn. */
  chrome?: boolean;
  /** Box style for the border. Default "rounded". */
  boxStyle?: BoxStyle;
  /** Lines scrolled back into history. 0 = live viewport (default). */
  scrollOffset?: number;
  /** Optional selection to highlight. */
  selection?: PtyPaneSelection | null;
  /** Override the focused border color (defaults to theme.fgAc). */
  borderColor?: [number, number, number];
  /** Override the unfocused border color (defaults to theme.border ?? theme.fgMu). */
  mutedBorderColor?: [number, number, number];
  /** Cache the last cell read per handle and skip re-reading a clean pane
   *  (same size + scroll). Default true. */
  cache?: boolean;
}

export interface PtyPaneResult {
  /** The child cursor's on-screen position, 1-based, suitable for
   *  `moveTo(row, col) + showCursor()`. `null` when the pane is unfocused
   *  or the cursor is scrolled off-screen. */
  cursor: { row: number; col: number } | null;
  /** The 0-based inner content rect the cells were blitted into. */
  inner: Rect;
}

interface PaneCacheEntry {
  cells: PtyCell[][];
  width: number;
  height: number;
  scrollOffset: number;
}

// Keyed by handle so entries are collected when the handle is GC'd — the
// widget stays stateless from the caller's point of view.
const paneCache = new WeakMap<PtyHandle, PaneCacheEntry>();

/** Drop a handle's cached cells (or all of them). Call after a change the
 *  cache can't detect — e.g. a theme swap that recolors the same cells. */
export function clearPtyPaneCache(handle?: PtyHandle): void {
  if (handle) paneCache.delete(handle);
  // A WeakMap has no clear(); without a handle there's nothing global to do.
}

/** Compute the 0-based inner content rect for a pane given its outer rect
 *  and whether chrome is drawn. */
export function ptyPaneInnerRect(rect: Rect, chrome: boolean): Rect {
  if (!chrome) return { x: rect.x, y: rect.y, width: rect.width, height: rect.height };
  return {
    x: rect.x + 1,
    y: rect.y + 1,
    width: Math.max(0, rect.width - 2),
    height: Math.max(0, rect.height - 2),
  };
}

/** The child cursor's effective 0-based row within the pane inner area
 *  given the current scroll offset, or null if it's scrolled off-screen.
 *  The cursor lives in the live viewport; scrolling back (offset > 0)
 *  pushes it down by that many rows. */
export function effectiveCursorRow(
  cursorRow: number,
  scrollOffset: number,
  innerHeight: number,
): number | null {
  const effective = cursorRow + scrollOffset;
  if (effective < 0 || effective >= innerHeight) return null;
  return effective;
}

/** Whether an inner cell (row, col) falls within a selection, translating
 *  the selection's captured coordinates to the current scroll offset so the
 *  highlight follows the content, not the screen position. */
export function isSelectedInPane(
  row: number,
  col: number,
  sel: PtyPaneSelection,
  currentScrollOffset: number,
): boolean {
  const delta = currentScrollOffset - sel.scrollOffset;
  const r = row - delta;
  let r1 = sel.startRow, c1 = sel.startCol;
  let r2 = sel.endRow, c2 = sel.endCol;
  if (r1 > r2 || (r1 === r2 && c1 > c2)) {
    [r1, c1, r2, c2] = [r2, c2, r1, c1];
  }
  if (r < r1 || r > r2) return false;
  if (r === r1 && r === r2) return col >= c1 && col <= c2;
  if (r === r1) return col >= c1;
  if (r === r2) return col <= c2;
  return true;
}

function hasDragDistance(sel: PtyPaneSelection): boolean {
  return sel.startRow !== sel.endRow || sel.startCol !== sel.endCol;
}

/**
 * Render a single live pty pane into `buf` at `rect` (0-based, in the
 * framework's `Rect` convention). Returns the child cursor's on-screen
 * position for the host to place the real terminal cursor.
 */
export function renderPtyPane(
  buf: CellBuffer,
  rect: Rect,
  handle: PtyHandle,
  opts: PtyPaneOptions,
): PtyPaneResult {
  const chrome = opts.chrome !== false;
  const boxStyle: BoxStyle = opts.boxStyle ?? "rounded";
  const scrollOffset = opts.scrollOffset ?? 0;
  const useCache = opts.cache !== false;
  const theme = opts.theme;

  const inner = ptyPaneInnerRect(rect, chrome);

  // --- Border + title chrome ---
  if (chrome && rect.width >= 2 && rect.height >= 2) {
    const border = opts.focused
      ? (opts.borderColor ?? theme.fgAc ?? [80, 200, 120])
      : (opts.mutedBorderColor ?? theme.border ?? theme.fgMu ?? [100, 100, 100]);
    // drawBox is 1-based (row/col); our Rect is 0-based.
    buf.writeAnsi(
      fgAnsi(border[0], border[1], border[2]) +
      drawBox(rect.y + 1, rect.x + 1, rect.width, rect.height, {
        style: boxStyle,
        ...(opts.title ? { title: opts.title } : {}),
      }) +
      reset(),
    );
  }

  let cursor: PtyPaneResult["cursor"] = null;

  if (inner.width > 0 && inner.height > 0) {
    // Match the child PTY to the inner content area.
    handle.resize(inner.width, inner.height);

    // Reuse the last read when nothing changed; otherwise read fresh.
    const cached = useCache ? paneCache.get(handle) : undefined;
    const canReuse =
      cached &&
      !handle.dirty &&
      cached.width === inner.width &&
      cached.height === inner.height &&
      cached.scrollOffset === scrollOffset;

    const cells: PtyCell[][] = canReuse ? cached!.cells : handle.readCells(scrollOffset);

    if (!canReuse) {
      if (useCache) {
        paneCache.set(handle, {
          cells,
          width: inner.width,
          height: inner.height,
          scrollOffset,
        });
      }
      // We consumed the pending data for this size/scroll.
      handle.dirty = false;
    }

    const showSelection =
      opts.selection != null && hasDragDistance(opts.selection);

    for (let r = 0; r < cells.length && r < inner.height; r++) {
      const rowCells = cells[r];
      if (!rowCells) continue;
      for (let c = 0; c < rowCells.length && c < inner.width; c++) {
        const cell = rowCells[c];
        if (!cell) continue;
        const y = inner.y + r;
        const x = inner.x + c;
        if (showSelection && isSelectedInPane(r, c, opts.selection!, scrollOffset)) {
          // Invert fg/bg (and their palette indices) for the highlight.
          buf.setCell(y, x, {
            ...cell,
            fg: cell.bg ?? [0, 0, 0],
            bg: cell.fg ?? [200, 200, 200],
            fgIndex: cell.bgIndex,
            bgIndex: cell.fgIndex,
          } as Cell);
        } else {
          // PtyCell is structurally a Cell — blit directly, preserving
          // palette indices so the outer terminal resolves them.
          buf.setCell(y, x, cell as Cell);
        }
      }
    }

    // Report the cursor only for a focused, on-screen cursor.
    if (opts.focused) {
      const effRow = effectiveCursorRow(handle.cursorRow, scrollOffset, inner.height);
      if (effRow !== null && handle.cursorCol >= 0 && handle.cursorCol < inner.width) {
        cursor = {
          row: inner.y + effRow + 1, // 1-based for moveTo
          col: inner.x + handle.cursorCol + 1,
        };
      }
    }
  }

  return { cursor, inner };
}
