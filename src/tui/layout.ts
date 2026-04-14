// Two-pass layout engine: measure (bottom-up) then position (top-down)

import type {
  UINode, Rect, RowNode, ColumnNode, HStackNode, PanelNode,
  ScrollableNode, SelectableNode,
} from "./nodes.ts";
import { charWidth, wrapText } from "./colors.ts";

// --- Clipping helper ---

/** Clip a child rect to stay within a parent boundary. Returns the clipped rect. */
function clipRect(child: Rect, parent: Rect): Rect {
  const right = Math.min(child.x + child.width, parent.x + parent.width);
  const bottom = Math.min(child.y + child.height, parent.y + parent.height);
  const x = Math.max(child.x, parent.x);
  const y = Math.max(child.y, parent.y);
  return {
    x,
    y,
    width: Math.max(0, right - x),
    height: Math.max(0, bottom - y),
  };
}

// --- Text width measurement (no ANSI, plain chars only) ---

export function textWidth(str: string): number {
  let w = 0;
  for (const ch of str) {
    w += charWidth(ch);
  }
  return w;
}

// --- Height measurement ---
// Returns intrinsic height or "flex" for nodes that expand to fill space.

export function measureHeight(node: UINode, maxWidth: number): number | "flex" {
  switch (node.type) {
    case "text": {
      if (node.wrap && maxWidth > 0) {
        const tw = textWidth(node.text);
        if (tw <= maxWidth) return 1;
        return wrapText(node.text, maxWidth).lines.length;
      }
      return 1;
    }
    case "spacer": return 0;
    case "gap": return node.size === "center" ? "flex" : node.size;
    case "separator": return 1;
    case "indent": return 0;
    case "dot": return 1;
    case "checkbox": return 1;
    case "progressBar": return 1;
    case "spinner": return 1;
    case "icon": return 1;
    case "row": return 1;
    case "column": {
      const cw = node.width ?? maxWidth;
      let hasFlexChild = false;
      let fixedHeight = 0;
      for (const child of node.children) {
        const h = measureHeight(child, cw);
        if (h === "flex") hasFlexChild = true;
        else fixedHeight += h;
      }
      return hasFlexChild ? "flex" : fixedHeight;
    }
    case "hstack": {
      let hasFlexCol = false;
      let maxH = 0;
      for (const col of node.children) {
        const h = measureHeight(col, maxWidth);
        if (h === "flex") hasFlexCol = true;
        else maxH = Math.max(maxH, h);
      }
      return hasFlexCol ? "flex" : maxH;
    }
    case "panel": {
      const contentWidth = maxWidth - 4;
      let hasFlexChild = false;
      let fixedHeight = 0;
      for (const child of node.children) {
        const h = measureHeight(child, contentWidth);
        if (h === "flex") hasFlexChild = true;
        else fixedHeight += h;
      }
      return hasFlexChild ? "flex" : fixedHeight + 2;
    }
    case "scrollable":
    case "selectable":
      return "flex";
    case "statusBar": return 1;
    case "footer": return 1;
    case "askBar": return 3;
    case "textInput": return 1;
    case "fpsCounter": return 1;
    case "canvas": return node.height ?? "flex";
    case "ptyView": return "flex";
  }
}

// --- Width measurement ---
// Returns intrinsic width or "flex" for nodes that expand horizontally.

export function measureWidth(node: UINode): number | "flex" {
  switch (node.type) {
    case "text": return (node.truncate || node.wrap) ? "flex" : textWidth(node.text);
    case "spacer": return "flex";
    case "gap": return 0;
    case "separator": return "flex";
    case "indent": return node.depth * 2;
    case "dot": return 1;
    case "checkbox": return 1;
    case "progressBar": return node.width ?? "flex";
    case "spinner": return 1;
    case "icon": return charWidth(node.char);
    case "textInput": return "flex";
    case "fpsCounter": return 8;
    case "canvas": return node.widthHint ?? "flex";
    case "ptyView": return "flex";
    case "row": return "flex";
    case "column": return "flex";
    case "hstack": return "flex";
    case "panel": return "flex";
    case "scrollable": return "flex";
    case "selectable": return "flex";
    default: return 0;
  }
}

// --- Layout entry point ---

export function layoutRoot(nodes: UINode[], viewport: Rect): void {
  // Extract statusBar and footer from the top-level list
  let topY = viewport.y;
  let bottomY = viewport.y + viewport.height;

  for (const node of nodes) {
    if (node.type === "statusBar") {
      node._rect = { x: viewport.x, y: topY, width: viewport.width, height: 1 };
      topY++;
    }
  }

  // Footer anchors to bottom (process in reverse to stack correctly)
  for (let i = nodes.length - 1; i >= 0; i--) {
    if (nodes[i].type === "footer") {
      bottomY--;
      nodes[i]._rect = { x: viewport.x, y: bottomY, width: viewport.width, height: 1 };
    }
  }

  // Layout remaining children vertically in the middle area
  const middle = nodes.filter(n => n.type !== "statusBar" && n.type !== "footer");
  const middleRect = { x: viewport.x, y: topY, width: viewport.width, height: bottomY - topY };
  layoutVertical(middle, middleRect);
}

// --- Vertical flow layout ---

export function layoutVertical(nodes: UINode[], rect: Rect): void {
  if (nodes.length === 0) return;

  // First pass: measure all heights
  const heights: (number | "flex")[] = nodes.map(n => measureHeight(n, rect.width));

  // Calculate flex distribution
  let fixedTotal = 0;
  let flexCount = 0;
  for (const h of heights) {
    if (h === "flex") flexCount++;
    else fixedTotal += h;
  }

  const remaining = Math.max(0, rect.height - fixedTotal);
  const flexSize = flexCount > 0 ? Math.floor(remaining / flexCount) : 0;
  const flexRemainder = flexCount > 0 ? remaining - flexSize * flexCount : 0;

  // Second pass: assign rects
  let y = rect.y;
  let flexIndex = 0;
  for (let i = 0; i < nodes.length; i++) {
    const h = heights[i] === "flex"
      ? flexSize + (flexIndex++ < flexRemainder ? 1 : 0)
      : heights[i] as number;

    const nodeRect = clipRect({ x: rect.x, y, width: rect.width, height: h }, rect);
    nodes[i]._rect = nodeRect;
    if (nodeRect.width > 0 && nodeRect.height > 0) {
      layoutChildren(nodes[i], nodeRect);
    }
    y += h;
  }
}

// --- Horizontal flow layout (for rows) ---

export function layoutRow(children: UINode[], rect: Rect): void {
  if (children.length === 0) return;

  // First pass: measure widths
  const widths: (number | "flex")[] = children.map(n => measureWidth(n));

  // Calculate flex distribution
  let fixedTotal = 0;
  let flexCount = 0;
  for (const w of widths) {
    if (w === "flex") flexCount++;
    else fixedTotal += w;
  }

  const remaining = Math.max(0, rect.width - fixedTotal);
  const flexSize = flexCount > 0 ? Math.floor(remaining / flexCount) : 0;
  const flexRemainder = flexCount > 0 ? remaining - flexSize * flexCount : 0;

  // Second pass: assign positions
  let x = rect.x;
  let flexIndex = 0;
  for (let i = 0; i < children.length; i++) {
    const w = widths[i] === "flex"
      ? flexSize + (flexIndex++ < flexRemainder ? 1 : 0)
      : widths[i] as number;

    const childRect = clipRect({ x, y: rect.y, width: w, height: rect.height }, rect);
    children[i]._rect = childRect;
    if (childRect.width > 0 && childRect.height > 0) {
      layoutChildren(children[i], childRect);
    }
    x += w;
  }
}

// --- Container-specific layout ---

function layoutChildren(node: UINode, rect: Rect): void {
  if (rect.width <= 0 || rect.height <= 0) return;
  switch (node.type) {
    case "row":
      layoutRow(node.children, rect);
      break;
    case "column":
      layoutVertical(node.children, rect);
      break;
    case "hstack":
      layoutHStack(node, rect);
      break;
    case "panel":
      layoutPanel(node, rect);
      break;
    case "scrollable":
      layoutScrollable(node, rect);
      break;
    case "selectable":
      layoutScrollable(node, rect);
      break;
  }
}

function layoutHStack(node: HStackNode, rect: Rect): void {
  const columns = node.children;
  const gapSize = node.gap ?? 0;
  const totalGap = gapSize * Math.max(0, columns.length - 1);

  // Measure fixed-width columns
  let fixedWidth = 0;
  let flexCount = 0;
  for (const col of columns) {
    if (col.width !== undefined) {
      fixedWidth += col.width;
    } else {
      flexCount++;
    }
  }

  const remaining = Math.max(0, rect.width - fixedWidth - totalGap);
  const flexWidth = flexCount > 0 ? Math.floor(remaining / flexCount) : 0;
  const flexRemainder = flexCount > 0 ? remaining - flexWidth * flexCount : 0;

  let x = rect.x;
  let flexIndex = 0;
  for (const col of columns) {
    let w: number;
    if (col.width !== undefined) {
      w = col.width;
    } else {
      w = flexWidth + (flexIndex < flexRemainder ? 1 : 0);
      flexIndex++;
    }

    col._rect = clipRect({ x, y: rect.y, width: w, height: rect.height }, rect);
    if (col._rect.width > 0 && col._rect.height > 0) {
      layoutVertical(col.children, col._rect);
    }
    x += w + gapSize;
  }
}

export function layoutPanel(node: PanelNode, rect: Rect): void {
  // Content area: inset 2 cols from sides (border + padding), 1 row top/bottom
  const contentX = rect.x + 2;
  const contentY = rect.y + 1;
  const contentW = Math.max(0, rect.width - 4);
  const contentH = Math.max(0, rect.height - 2);
  const contentRect: Rect = { x: contentX, y: contentY, width: contentW, height: contentH };

  // Measure children heights
  const heights: (number | "flex")[] = node.children.map(child =>
    measureHeight(child, contentW),
  );

  let fixedTotal = 0;
  let flexCount = 0;
  for (const h of heights) {
    if (h === "flex") flexCount++;
    else fixedTotal += h;
  }

  const remaining = Math.max(0, contentH - fixedTotal);
  const flexSize = flexCount > 0 ? Math.floor(remaining / flexCount) : 0;
  const flexRemainder = flexCount > 0 ? remaining - flexSize * flexCount : 0;

  let y = contentY;
  let flexIndex = 0;
  for (let i = 0; i < node.children.length; i++) {
    const child = node.children[i];
    const h = heights[i] === "flex"
      ? flexSize + (flexIndex++ < flexRemainder ? 1 : 0)
      : heights[i] as number;

    if (child.type === "separator") {
      // Separator spans the full panel width (connects to borders)
      const sepRect = clipRect({ x: rect.x, y, width: rect.width, height: 1 }, { x: rect.x, y: contentY, width: rect.width, height: contentH });
      child._rect = sepRect;
    } else {
      const childRect = clipRect({ x: contentX, y, width: contentW, height: h }, contentRect);
      child._rect = childRect;
      if (childRect.width > 0 && childRect.height > 0) {
        layoutChildren(child, child._rect);
      }
    }
    y += h;
  }
}

function layoutScrollable(
  node: ScrollableNode | SelectableNode,
  rect: Rect,
): void {
  const offset = node.offset;
  const visibleCount = Math.min(node.items.length - offset, rect.height);

  for (let i = 0; i < visibleCount; i++) {
    const itemIndex = offset + i;
    const itemNodes = node.items[itemIndex];
    if (!itemNodes) continue;

    const rowRect = clipRect({ x: rect.x, y: rect.y + i, width: rect.width, height: 1 }, rect);
    if (rowRect.width > 0 && rowRect.height > 0) {
      // Layout each item's nodes as a horizontal row
      layoutRow(itemNodes, rowRect);
    }
  }
}
