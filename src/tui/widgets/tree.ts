// Tree view widget — keyboard-navigable, expand/collapse, depth-aware.
//
// Design: state-first. `flattenTree` takes the tree + an expanded-set and
// returns a flat list of visible rows (each carrying a depth). The consumer
// renders those rows however it likes. Selection and expansion state are
// just signals you own.
//
// Promoted from local helpers in demos/reminders and demos/file-browser.
// Both relied on a flat visible-rows shape; this is the shared version.

import type { KeyEvent } from "../input.ts";

export interface TreeNode<T> {
  /** Stable, unique id within the tree. Used for selection + expansion keys. */
  id: string;
  label: string;
  data: T;
  children?: TreeNode<T>[];
}

export interface TreeRow<T> {
  node: TreeNode<T>;
  depth: number;
  hasChildren: boolean;
  expanded: boolean;
}

export interface TreeState {
  /** Ids of nodes whose children are shown. */
  expanded: Set<string>;
  /** Currently highlighted node, or null when the tree is empty. */
  selectedId: string | null;
}

export function createTreeState(): TreeState {
  return { expanded: new Set(), selectedId: null };
}

/** Depth-first walk of the tree, skipping the children of any node that is
 *  not in `expanded`. The resulting list is what the renderer draws. */
export function flattenTree<T>(roots: TreeNode<T>[], expanded: Set<string>): TreeRow<T>[] {
  const out: TreeRow<T>[] = [];
  const walk = (nodes: TreeNode<T>[], depth: number) => {
    for (const node of nodes) {
      const hasChildren = !!node.children && node.children.length > 0;
      const isExpanded = hasChildren && expanded.has(node.id);
      out.push({ node, depth, hasChildren, expanded: isExpanded });
      if (isExpanded && node.children) walk(node.children, depth + 1);
    }
  };
  walk(roots, 0);
  return out;
}

export function toggleExpanded(state: TreeState, id: string): TreeState {
  const next = new Set(state.expanded);
  if (next.has(id)) next.delete(id);
  else next.add(id);
  return { ...state, expanded: next };
}

export function selectById<T>(state: TreeState, id: string | null): TreeState {
  return { ...state, selectedId: id };
}

/** Move selection to the row `delta` away from the currently selected row.
 *  Clamps at the ends. Returns the same state reference if nothing changes. */
export function moveSelection<T>(
  state: TreeState,
  rows: TreeRow<T>[],
  delta: number,
): TreeState {
  if (rows.length === 0) return state;
  const idx = state.selectedId
    ? rows.findIndex(r => r.node.id === state.selectedId)
    : -1;
  // Nothing selected yet — any arrow key lands on the first row, rather than
  // skipping past it. Deliberate: it's what users expect the first press to do.
  if (idx === -1) return { ...state, selectedId: rows[0].node.id };
  const next = Math.max(0, Math.min(rows.length - 1, idx + delta));
  if (next === idx) return state;
  return { ...state, selectedId: rows[next].node.id };
}

/** Opinionated default key bindings — up/down moves, left/right collapses or
 *  expands, enter toggles expansion on folders. Returns the new state (or the
 *  same reference if the key was not consumed). The caller receives an extra
 *  hint about what happened via the returned `action` field. */
export interface HandleKeyResult<T> {
  state: TreeState;
  /** What happened — useful for the consumer to wire side effects (e.g.,
   *  "activate" when the user hits enter on a leaf). */
  action: "moved" | "expanded" | "collapsed" | "activated" | "none";
  /** The row the action applied to, if any. */
  row: TreeRow<T> | null;
}

export function handleTreeKey<T>(
  state: TreeState,
  rows: TreeRow<T>[],
  key: KeyEvent,
): HandleKeyResult<T> {
  const selectedRow =
    state.selectedId != null
      ? rows.find(r => r.node.id === state.selectedId) ?? null
      : null;

  if (key.name === "up") {
    return { state: moveSelection(state, rows, -1), action: "moved", row: null };
  }
  if (key.name === "down") {
    return { state: moveSelection(state, rows, 1), action: "moved", row: null };
  }
  if (!selectedRow) {
    return { state, action: "none", row: null };
  }
  if (key.name === "right") {
    if (selectedRow.hasChildren && !selectedRow.expanded) {
      return { state: toggleExpanded(state, selectedRow.node.id), action: "expanded", row: selectedRow };
    }
    return { state, action: "none", row: selectedRow };
  }
  if (key.name === "left") {
    if (selectedRow.hasChildren && selectedRow.expanded) {
      return { state: toggleExpanded(state, selectedRow.node.id), action: "collapsed", row: selectedRow };
    }
    return { state, action: "none", row: selectedRow };
  }
  if (key.name === "return") {
    if (selectedRow.hasChildren) {
      return { state: toggleExpanded(state, selectedRow.node.id),
               action: selectedRow.expanded ? "collapsed" : "expanded",
               row: selectedRow };
    }
    return { state, action: "activated", row: selectedRow };
  }
  return { state, action: "none", row: null };
}

/** Glyph for the expand/collapse indicator in front of a row.
 *  ▸ collapsed folder, ▾ expanded folder, empty string for leaves. */
export function treeGlyph<T>(row: TreeRow<T>): string {
  if (!row.hasChildren) return "  ";
  return row.expanded ? "\u25be " : "\u25b8 ";
}
