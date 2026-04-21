// Tabs — a horizontal strip of labels with one active. Lets apps organise
// multiple top-level views (like a mail client with Inbox/Sent/Drafts).
//
// Navigation: tab / ctrl+tab cycle forward, backtab / ctrl+shift+tab cycle
// backward. Or bind numbers 1-9 to jump directly. Activation is cosmetic
// only — the consumer decides what "active" means for their app.

import { row, text } from "../builders.ts";
import type { UINode, Rect } from "../nodes.ts";
import type { KeyEvent, MouseEvent } from "../input.ts";

export interface TabDef<T = unknown> {
  id: string;
  label: string;
  /** Optional data bag for the consumer — tabs don't care what this is. */
  data?: T;
}

export interface TabsState {
  activeId: string | null;
}

export function createTabsState<T>(tabs: readonly TabDef<T>[], initial?: string): TabsState {
  return { activeId: initial ?? tabs[0]?.id ?? null };
}

export function selectTab(state: TabsState, id: string): TabsState {
  if (state.activeId === id) return state;
  return { activeId: id };
}

function stepTab<T>(state: TabsState, tabs: readonly TabDef<T>[], delta: 1 | -1): TabsState {
  if (tabs.length === 0 || !state.activeId) return state;
  const idx = tabs.findIndex(t => t.id === state.activeId);
  if (idx < 0) return state;
  const next = (idx + delta + tabs.length) % tabs.length;
  return { activeId: tabs[next].id };
}

export function nextTab<T>(state: TabsState, tabs: readonly TabDef<T>[]): TabsState {
  return stepTab(state, tabs, 1);
}

export function prevTab<T>(state: TabsState, tabs: readonly TabDef<T>[]): TabsState {
  return stepTab(state, tabs, -1);
}

/** Default key dispatch — ctrl+tab / ctrl+shift+tab to cycle. Returns
 *  `null` when the key wasn't consumed so the caller can handle focus. */
export function handleTabsKey<T>(
  state: TabsState,
  tabs: readonly TabDef<T>[],
  key: KeyEvent,
): TabsState | null {
  if (key.name === "tab" && key.ctrl) return nextTab(state, tabs);
  if (key.name === "backtab" && key.ctrl) return prevTab(state, tabs);
  // Numeric shortcuts: 1..9 select tabs 1..9.
  if (key.char && /^[1-9]$/.test(key.char) && !key.ctrl && !key.alt) {
    const idx = parseInt(key.char, 10) - 1;
    const t = tabs[idx];
    if (t) return selectTab(state, t.id);
  }
  return null;
}

/** Route a left-click within the tab bar's rect to whichever tab was
 *  clicked. Returns the new state (or the same one if the click was
 *  outside the bar or on a gap). Computes tab widths on the fly from the
 *  same rendering rule renderTabs uses — consumers pass the row's rect
 *  (from layout) and the tabs array. */
export function handleTabsMouse<T>(
  state: TabsState,
  tabs: readonly TabDef<T>[],
  event: MouseEvent,
  rect: Rect,
): TabsState | null {
  if (event.action !== "press" || event.button !== "left") return null;
  if (event.y !== rect.y) return null;
  // renderTabs layout: each tab is "[ Label ]" (active) or "  Label  "
  // (inactive), joined by "  " separators. Widths: "[ L ]" = L.length + 4.
  let cursor = rect.x;
  for (let i = 0; i < tabs.length; i++) {
    if (i > 0) cursor += 2; // "  " separator
    const w = tabs[i].label.length + 4;
    if (event.x >= cursor && event.x < cursor + w) {
      return selectTab(state, tabs[i].id);
    }
    cursor += w;
  }
  return null;
}

/** Render a tabs row: `[Active] inactive1 inactive2`. Active tab is bold
 *  with brackets; inactive tabs are dim. */
export function renderTabs<T>(state: TabsState, tabs: readonly TabDef<T>[]): UINode {
  const parts: UINode[] = [];
  tabs.forEach((t, i) => {
    if (i > 0) parts.push(text("  ", "muted"));
    if (t.id === state.activeId) {
      parts.push(text(`[ ${t.label} ]`, "accent", { bold: true }));
    } else {
      parts.push(text(`  ${t.label}  `, "muted", { dim: true }));
    }
  });
  return row(...parts);
}
