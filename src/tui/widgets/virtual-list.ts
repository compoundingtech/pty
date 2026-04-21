// Virtualized list — keyboard-navigable list that only renders a visible
// slice. For datasets where rendering every row would be wasteful (email
// inboxes, RSS archives, long logs). The consumer still gives us `total`
// and an item-by-index callback; we never look at the full array.
//
// State-first: the caller owns `VirtualListState`. `moveVirtualSelection`
// and arrow-key handlers return new states.

import { column, row, text } from "../builders.ts";
import type { UINode, ColumnNode, Rect } from "../nodes.ts";
import type { KeyEvent, MouseEvent } from "../input.ts";

export interface VirtualListState {
  total: number;
  selectedIndex: number;
  /** First visible index (scroll offset). */
  offset: number;
  /** How many rows the viewport shows. The caller drives this from layout. */
  viewport: number;
}

export interface VirtualWindow {
  /** Inclusive first index to render. */
  start: number;
  /** Exclusive last index to render. */
  end: number;
}

export function createVirtualListState(total: number, viewport: number): VirtualListState {
  return {
    total,
    selectedIndex: total > 0 ? 0 : -1,
    offset: 0,
    viewport: Math.max(1, viewport),
  };
}

/** Re-normalise state after total/viewport change. Keeps the selection in
 *  view and within bounds. */
export function clampVirtual(state: VirtualListState): VirtualListState {
  const total = Math.max(0, state.total);
  const viewport = Math.max(1, state.viewport);
  if (total === 0) {
    return { total: 0, selectedIndex: -1, offset: 0, viewport };
  }
  const sel = Math.max(0, Math.min(total - 1, state.selectedIndex));
  const maxOffset = Math.max(0, total - viewport);
  let offset = Math.max(0, Math.min(maxOffset, state.offset));
  if (sel < offset) offset = sel;
  if (sel >= offset + viewport) offset = sel - viewport + 1;
  return { total, selectedIndex: sel, offset, viewport };
}

/** Compute the window of indexes that should be drawn right now. */
export function virtualWindow(state: VirtualListState): VirtualWindow {
  const s = clampVirtual(state);
  return { start: s.offset, end: Math.min(s.total, s.offset + s.viewport) };
}

/** Move selection by `delta`, adjusting offset to keep the selection in
 *  the viewport. Returns the same reference when nothing changed. */
export function moveVirtualSelection(
  state: VirtualListState,
  delta: number,
): VirtualListState {
  if (state.total === 0) return state;
  const target = Math.max(0, Math.min(state.total - 1, state.selectedIndex + delta));
  if (target === state.selectedIndex) return state;
  return clampVirtual({ ...state, selectedIndex: target });
}

export function pageVirtual(state: VirtualListState, delta: number): VirtualListState {
  return moveVirtualSelection(state, delta * state.viewport);
}

export function jumpVirtualToStart(state: VirtualListState): VirtualListState {
  return clampVirtual({ ...state, selectedIndex: 0 });
}

export function jumpVirtualToEnd(state: VirtualListState): VirtualListState {
  return clampVirtual({ ...state, selectedIndex: Math.max(0, state.total - 1) });
}

export interface HandleVirtualKeyResult {
  state: VirtualListState;
  /** What the consumer might want to react to. "activate" = the user hit
   *  Enter on the selected row; the consumer should open it. */
  action: "moved" | "activate" | "none";
}

export interface HandleVirtualMouseResult {
  state: VirtualListState;
  /** "moved" = scroll or selection changed. "activate" = click picked a
   *  specific row the caller should open. "none" = event outside the list's
   *  rendered rect, or on the empty-state row. */
  action: "moved" | "activate" | "none";
}

/** Route a mouse event to the list given its rendered rect (from the
 *  layout pass). Handles click-to-select and scroll-wheel-to-scroll. */
export function handleVirtualMouse(
  state: VirtualListState,
  event: MouseEvent,
  rect: Rect,
): HandleVirtualMouseResult {
  const inside = event.x >= rect.x && event.x < rect.x + rect.width
              && event.y >= rect.y && event.y < rect.y + rect.height;
  if (!inside) return { state, action: "none" };

  if (event.action === "scrollUp") {
    return { state: moveVirtualSelection(state, -3), action: "moved" };
  }
  if (event.action === "scrollDown") {
    return { state: moveVirtualSelection(state, 3), action: "moved" };
  }
  if (event.action === "press" && event.button === "left") {
    const rowIdx = (event.y - rect.y) + state.offset;
    if (rowIdx < 0 || rowIdx >= state.total) return { state, action: "none" };
    return {
      state: clampVirtual({ ...state, selectedIndex: rowIdx }),
      action: "activate",
    };
  }
  return { state, action: "none" };
}

/** Default key bindings: up/down, pageup/pagedown, home/end, return. */
export function handleVirtualKey(state: VirtualListState, key: KeyEvent): HandleVirtualKeyResult {
  switch (key.name) {
    case "up":       return { state: moveVirtualSelection(state, -1), action: "moved" };
    case "down":     return { state: moveVirtualSelection(state, 1),  action: "moved" };
    case "pageup":   return { state: pageVirtual(state, -1), action: "moved" };
    case "pagedown": return { state: pageVirtual(state, 1),  action: "moved" };
    case "home":     return { state: jumpVirtualToStart(state), action: "moved" };
    case "end":      return { state: jumpVirtualToEnd(state), action: "moved" };
    case "return":   return { state, action: "activate" };
    default:         return { state, action: "none" };
  }
}

/** Render a column with one row per visible index. `renderItem(index, selected)`
 *  returns the UINode for that row. Never called for indexes outside the
 *  window — so this scales to arbitrary `total`. */
export function renderVirtualList(
  state: VirtualListState,
  renderItem: (index: number, selected: boolean) => UINode,
): UINode {
  const win = virtualWindow(state);
  const children: UINode[] = [];
  for (let i = win.start; i < win.end; i++) {
    children.push(renderItem(i, i === state.selectedIndex));
  }
  if (children.length === 0) {
    children.push(row(text("(empty)", "muted", { dim: true })));
  }
  const node: ColumnNode = { type: "column", children };
  return node;
}
