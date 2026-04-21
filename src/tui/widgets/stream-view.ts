// Stream view — "chat window" layout. Items flow in; by default the view
// is pinned to the newest (bottom). Scrolling up unpins and preserves the
// user's position even as new items arrive. Scrolling back to the bottom
// or explicitly re-pinning restores auto-follow.
//
// Used by chat apps, log tails, REPL output, streaming AI replies.

import { column, row, text } from "../builders.ts";
import type { UINode, ColumnNode, Rect } from "../nodes.ts";
import type { KeyEvent, MouseEvent } from "../input.ts";

export interface StreamViewState {
  /** Number of items to scroll back from the most recent. 0 = pinned. */
  scrollback: number;
}

export function createStreamView(): StreamViewState {
  return { scrollback: 0 };
}

export function isPinned(state: StreamViewState): boolean {
  return state.scrollback === 0;
}

export function streamPin(state: StreamViewState): StreamViewState {
  return state.scrollback === 0 ? state : { ...state, scrollback: 0 };
}

export function streamScrollUp(
  state: StreamViewState,
  delta: number,
  total: number,
  viewport: number,
): StreamViewState {
  const maxScrollback = Math.max(0, total - viewport);
  const next = Math.min(maxScrollback, state.scrollback + delta);
  if (next === state.scrollback) return state;
  return { ...state, scrollback: next };
}

export function streamScrollDown(state: StreamViewState, delta: number): StreamViewState {
  const next = Math.max(0, state.scrollback - delta);
  if (next === state.scrollback) return state;
  return { ...state, scrollback: next };
}

/** Compute the window of item indexes to draw. `total` and `viewport` are
 *  provided by the caller from layout each tick. */
export function streamWindow(
  state: StreamViewState,
  total: number,
  viewport: number,
): { start: number; end: number } {
  if (total <= 0 || viewport <= 0) return { start: 0, end: 0 };
  // end (exclusive) = total - scrollback; start = end - viewport, clamped.
  const end = Math.max(0, total - state.scrollback);
  const start = Math.max(0, end - viewport);
  return { start, end };
}

/** Route a mouse event. Scroll-wheel adjusts scrollback; no click handling
 *  since items aren't individually selectable in a plain stream view. */
export function handleStreamMouse(
  state: StreamViewState,
  event: MouseEvent,
  rect: Rect,
  total: number,
  viewport: number,
): StreamViewState | null {
  const inside = event.x >= rect.x && event.x < rect.x + rect.width
              && event.y >= rect.y && event.y < rect.y + rect.height;
  if (!inside) return null;
  if (event.action === "scrollUp")   return streamScrollUp(state, 3, total, viewport);
  if (event.action === "scrollDown") return streamScrollDown(state, 3);
  return null;
}

/** Default key handler — up/down for scrollback, pageup/pagedown for bigger
 *  jumps, end to re-pin. `total` and `viewport` are required for clamping. */
export function handleStreamKey(
  state: StreamViewState,
  key: KeyEvent,
  total: number,
  viewport: number,
): StreamViewState | null {
  switch (key.name) {
    case "up":       return streamScrollUp(state, 1, total, viewport);
    case "down":     return streamScrollDown(state, 1);
    case "pageup":   return streamScrollUp(state, viewport, total, viewport);
    case "pagedown": return streamScrollDown(state, viewport);
    case "end":      return streamPin(state);
    case "home":     return streamScrollUp(state, total, total, viewport);
  }
  return null;
}

/** Render visible items using a per-item renderer. Appends an "N unread
 *  below" indicator row when scrolled-back and new items have arrived. */
export function renderStreamView<T>(
  items: T[],
  state: StreamViewState,
  viewport: number,
  renderItem: (item: T, index: number) => UINode,
): UINode {
  const total = items.length;
  const { start, end } = streamWindow(state, total, viewport);
  const children: UINode[] = [];
  for (let i = start; i < end; i++) {
    children.push(renderItem(items[i], i));
  }
  if (!isPinned(state) && state.scrollback > 0) {
    // Bottom hint row — unread count between the window and the tail.
    const behind = total - end;
    if (behind > 0) {
      children.push(row(
        text(`— ${behind} more below (end to jump) —`, "accent", { dim: true }),
      ));
    }
  }
  const node: ColumnNode = { type: "column", children };
  return node;
}
