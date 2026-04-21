// Hit-testing for UINode trees. After layoutRoot runs, every node has a
// _rect (x, y, width, height) in screen coordinates. Given a mouse event
// with (x, y) in the same coordinate space, this walks the tree top-down
// and returns the deepest node whose rect contains the point, plus the
// chain of ancestors that also contained it.
//
// Consumers use this to route clicks to the right widget. Example:
//
//   handleMouse(e, ctx) {
//     const hit = hitTest(renderedNodes, e.x, e.y);
//     if (!hit) return false;
//     // hit.path is [root, ..., deepest] — find the nearest selectable.
//     const sel = hit.path.find(n => n.type === "selectable");
//     if (sel) /* route the click to the selectable */;
//   }

import type { UINode, Rect } from "./nodes.ts";

export interface HitResult {
  /** The deepest node containing the point. */
  node: UINode;
  /** Root-first chain of nodes containing the point. Last element === node. */
  path: UINode[];
}

function rectContains(rect: Rect, x: number, y: number): boolean {
  return x >= rect.x && x < rect.x + rect.width
      && y >= rect.y && y < rect.y + rect.height;
}

/** Children that contain other UINodes and therefore participate in
 *  hit-testing. Leaf nodes (text, spacer, etc.) have no children. */
function childrenOf(node: UINode): UINode[] {
  switch (node.type) {
    case "row":      return node.children;
    case "column":   return node.children;
    case "hstack":   return node.children as unknown as UINode[];
    case "panel":    return node.children;
    case "scrollable":
    case "selectable":
      // Items are arrays of arrays; flatten one level so walkers see every
      // rendered child row.
      return node.items.flat();
    default:         return [];
  }
}

/** Find the deepest node whose _rect contains (x, y). Returns null if no
 *  node covers the point (e.g. click outside the rendered area). */
export function hitTest(roots: UINode[], x: number, y: number): HitResult | null {
  const path: UINode[] = [];
  for (const root of roots) {
    if (!root._rect || !rectContains(root._rect, x, y)) continue;
    path.push(root);
    descend(root, x, y, path);
    return { node: path[path.length - 1], path };
  }
  return null;
}

function descend(node: UINode, x: number, y: number, path: UINode[]): void {
  const kids = childrenOf(node);
  for (const child of kids) {
    if (!child._rect || !rectContains(child._rect, x, y)) continue;
    path.push(child);
    descend(child, x, y, path);
    return; // Only one child per depth level should contain the point.
  }
}

/** Convenience: find the nearest ancestor of a given type in a hit path.
 *  Useful when you want "whichever scrollable/selectable/tabs the user
 *  clicked inside, not the deepest text node." */
export function findInPath<T extends UINode["type"]>(
  hit: HitResult,
  type: T,
): Extract<UINode, { type: T }> | null {
  for (let i = hit.path.length - 1; i >= 0; i--) {
    if (hit.path[i].type === type) return hit.path[i] as Extract<UINode, { type: T }>;
  }
  return null;
}
