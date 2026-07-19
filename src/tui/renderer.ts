// Renders positioned UI nodes to ANSI strings

import type {
  UINode, Color, Rect, TextNode, DotNode, CheckboxNode,
  ProgressBarNode, SpinnerNode, IconNode, RowNode, ColumnNode,
  HStackNode, PanelNode, ScrollableNode, SelectableNode,
  StatusBarNode, FooterNode, AskBarNode, TextInputNode,
  CanvasNode, CanvasCell, DrawContext, PtyViewNode,
} from "./nodes.ts";
import type { Theme, BoxStyle } from "./colors.ts";
import {
  moveTo, fg, bg, reset, bold as boldFn, dim as dimFn, italic as italicFn,
  BOLD, DIM, RESET, fillRect, fillLine, writeAt, drawBox, hSep,
  visibleLength, charWidth, wrapText, progressBar as renderProgressBarStr,
  boxChars,
} from "./colors.ts";
import { textWidth } from "./layout.ts";
import { resolveSemantic } from "./tokens.ts";

// --- Render options passed from screen wrapper ---

export interface RenderOpts {
  spinnerChar: string;
  fps: number;
  showFPS: boolean;
}

// --- Semantic color resolution ---

export function resolveColor(color: Color | undefined, theme: Theme): [number, number, number] | null {
  // Delegates to the canonical semantic-token resolution in tokens.ts so
  // the name→slot mapping has a single source of truth.
  return resolveSemantic(color, theme);
}

function fgColor(rgb: [number, number, number] | null): string {
  return rgb ? fg(rgb[0], rgb[1], rgb[2]) : "";
}

function bgColor(rgb: [number, number, number] | null): string {
  return rgb ? bg(rgb[0], rgb[1], rgb[2]) : "";
}

// --- Truncate plain text to fit width ---

function truncateText(str: string, maxWidth: number): string {
  if (maxWidth <= 1) return str.slice(0, maxWidth);
  let w = 0;
  let result = "";
  for (const ch of str) {
    const cw = charWidth(ch);
    if (w + cw + 1 > maxWidth) break;
    result += ch;
    w += cw;
  }
  return result + "\u2026";
}

// --- Main render entry point ---

export function renderToAnsi(
  nodes: UINode[],
  theme: Theme,
  boxStyle: BoxStyle,
  opts: RenderOpts,
  clip?: Rect,
): string {
  let out = "";
  for (const node of nodes) {
    out += renderNode(node, theme, boxStyle, opts, clip);
  }
  return out;
}

// --- Node dispatch ---

function renderNode(
  node: UINode,
  theme: Theme,
  boxStyle: BoxStyle,
  opts: RenderOpts,
  clip?: Rect,
): string {
  if (!node._rect) return "";
  const rect = node._rect;

  if (clip) {
    if (rect.y >= clip.y + clip.height || rect.y + rect.height <= clip.y ||
        rect.x >= clip.x + clip.width || rect.x + rect.width <= clip.x) {
      return "";
    }
  }

  switch (node.type) {
    case "text": return renderText(node, rect, theme, clip);
    case "spacer": return "";
    case "gap": return "";
    case "separator": return renderSeparator(rect, theme, boxStyle);
    case "indent": return "";
    case "dot": return renderDot(node, rect, theme);
    case "checkbox": return renderCheckbox(node, rect, theme);
    case "progressBar": return renderProgressBar(node, rect, theme);
    case "spinner": return renderSpinner(node, rect, theme, opts);
    case "icon": return renderIcon(node, rect, theme);
    case "row": return renderContainer(node.children, theme, boxStyle, opts, clip);
    case "column": return renderContainer(node.children, theme, boxStyle, opts, clip);
    case "hstack": return renderHStack(node, theme, boxStyle, opts, clip);
    case "panel": return renderPanel(node, theme, boxStyle, opts, clip);
    case "scrollable": return renderScrollable(node, theme, boxStyle, opts, clip);
    case "selectable": return renderScrollable(node, theme, boxStyle, opts, clip);
    case "statusBar": return renderStatusBar(node, rect, theme);
    case "footer": return renderFooter(node, rect, theme);
    case "askBar": return renderAskBar(node, rect, theme, boxStyle);
    case "textInput": return renderTextInput(node, rect, theme);
    case "fpsCounter": return renderFPSCounter(rect, theme, opts);
    case "canvas": return renderCanvas(node, rect, theme);
    case "ptyView": return renderPtyView(node, rect, theme);
    default: return "";
  }
}

// --- Leaf renderers ---

function renderText(node: TextNode, rect: Rect, theme: Theme, clip?: Rect): string {
  const color = resolveColor(node.color, theme);
  const content = node.text;
  const spans = node.highlight ? node.highlight(content) : null;

  let maxW = rect.width;
  if (clip) {
    maxW = Math.min(maxW, (clip.x + clip.width) - rect.x);
    if (maxW <= 0) return "";
  }

  if (node.wrap && maxW > 0) {
    // Soft-wrap: render multiple lines
    const { lines, offsets } = wrapText(content, maxW);
    const maxLines = Math.min(lines.length, rect.height);
    let out = "";
    for (let i = 0; i < maxLines; i++) {
      out += renderTextLine(lines[i], offsets[i], rect.y + i, rect.x, color, node.bold, node.dim, node.italic, spans, theme);
    }
    return out;
  }

  // Single-line: truncate if needed
  let displayContent = content;
  if (node.truncate && textWidth(content) > maxW) {
    displayContent = truncateText(content, maxW);
  } else if (clip && textWidth(content) > maxW) {
    displayContent = truncateText(content, maxW);
  }

  if (spans) {
    return renderTextLine(displayContent, 0, rect.y, rect.x, color, node.bold, node.dim, node.italic, spans, theme);
  }

  let out = moveTo(rect.y + 1, rect.x + 1);
  if (color) out += fgColor(color);
  if (node.bold) out += BOLD;
  if (node.dim) out += DIM;
  if (node.italic) out += "\x1b[3m";
  out += displayContent;
  out += RESET;
  return out;
}

/** Render a single text line with optional highlight spans. */
function renderTextLine(
  line: string,
  textOffset: number,
  y: number,
  x: number,
  defaultColor: [number, number, number] | null,
  bold?: boolean,
  dim?: boolean,
  italic?: boolean,
  spans?: import("./nodes.ts").Span[] | null,
  theme?: Theme,
): string {
  if (!spans || !theme) {
    // No highlighting — render as a simple string
    let out = moveTo(y + 1, x + 1);
    if (defaultColor) out += fgColor(defaultColor);
    if (bold) out += BOLD;
    if (dim) out += DIM;
    if (italic) out += "\x1b[3m";
    out += line;
    out += RESET;
    return out;
  }

  // Render character by character with span styles
  let out = moveTo(y + 1, x + 1);
  let charIdx = textOffset;
  let lastStyle = "";

  for (const ch of line) {
    let fg = defaultColor;
    let b = bold ?? false;
    let d = dim ?? false;
    let it = italic ?? false;

    for (const span of spans) {
      if (charIdx >= span.start && charIdx < span.end) {
        if (span.color !== undefined) fg = resolveColor(span.color, theme);
        if (span.bold !== undefined) b = span.bold;
        if (span.dim !== undefined) d = span.dim;
        if (span.italic !== undefined) it = span.italic;
        break;
      }
    }

    // Build style string
    let style = "";
    if (fg) style += fgColor(fg);
    if (b) style += BOLD;
    if (d) style += DIM;
    if (it) style += "\x1b[3m";

    if (style !== lastStyle) {
      out += RESET + style;
      lastStyle = style;
    }
    out += ch;
    charIdx++;
  }
  out += RESET;
  return out;
}

function renderDot(node: DotNode, rect: Rect, theme: Theme): string {
  const color = resolveColor(node.color, theme);
  const ch = node.filled ? "\u25cf" : "\u25cb";
  return moveTo(rect.y + 1, rect.x + 1) + fgColor(color) + ch + RESET;
}

function renderCheckbox(node: CheckboxNode, rect: Rect, theme: Theme): string {
  const color = resolveColor(node.color, theme);
  const ch = node.checked ? "\u25a0" : "\u25a1";
  return moveTo(rect.y + 1, rect.x + 1) + fgColor(color) + ch + RESET;
}

function renderProgressBar(node: ProgressBarNode, rect: Rect, theme: Theme): string {
  const color = resolveColor(node.color, theme) ?? theme.info;
  const barStr = renderProgressBarStr(rect.width, node.percent);
  return moveTo(rect.y + 1, rect.x + 1) + fgColor(color) + barStr + RESET;
}

function renderSpinner(node: SpinnerNode, rect: Rect, theme: Theme, opts: RenderOpts): string {
  const color = resolveColor(node.color, theme) ?? theme.fgAc;
  return moveTo(rect.y + 1, rect.x + 1) + fgColor(color) + opts.spinnerChar + RESET;
}

function renderIcon(node: IconNode, rect: Rect, theme: Theme): string {
  const color = resolveColor(node.color, theme);
  return moveTo(rect.y + 1, rect.x + 1) + fgColor(color) + node.char + RESET;
}

function renderSeparator(rect: Rect, theme: Theme, boxStyle: BoxStyle): string {
  return (theme.border ? fg(theme.border[0], theme.border[1], theme.border[2]) : "")
    + hSep(rect.y + 1, rect.x + 1, rect.width, boxStyle)
    + RESET;
}

// --- Container renderers ---

function renderContainer(
  children: UINode[],
  theme: Theme,
  boxStyle: BoxStyle,
  opts: RenderOpts,
  clip?: Rect,
): string {
  let out = "";
  for (const child of children) {
    out += renderNode(child, theme, boxStyle, opts, clip);
  }
  return out;
}

function renderHStack(
  node: HStackNode,
  theme: Theme,
  boxStyle: BoxStyle,
  opts: RenderOpts,
  clip?: Rect,
): string {
  let out = "";
  for (const col of node.children) {
    out += renderNode(col, theme, boxStyle, opts, clip);
  }
  return out;
}

function renderPanel(
  node: PanelNode,
  theme: Theme,
  bs: BoxStyle,
  opts: RenderOpts,
  clip?: Rect,
): string {
  const rect = node._rect!;
  const style = node.style ?? bs;
  let out = "";

  // Fill panel background
  out += bgColor(theme.bg2) + fillRect(rect.y + 1, rect.x + 1, rect.width, rect.height);

  // Draw border
  out += (theme.border ? fg(theme.border[0], theme.border[1], theme.border[2]) : "")
    + drawBox(rect.y + 1, rect.x + 1, rect.width, rect.height, { style });

  // Title
  if (node.title) {
    out += writeAt(rect.y + 1, rect.x + 3,
      " " + fgColor(theme.fgAc) + BOLD + node.title + RESET
      + bgColor(theme.bg2) + (theme.border ? fg(theme.border[0], theme.border[1], theme.border[2]) : "") + " ");
  }

  // Render children
  for (const child of node.children) {
    out += renderNode(child, theme, style, opts, clip);
  }

  return out;
}

function renderScrollable(
  node: ScrollableNode | SelectableNode,
  theme: Theme,
  boxStyle: BoxStyle,
  opts: RenderOpts,
  clip?: Rect,
): string {
  const rect = node._rect!;
  const offset = node.offset;
  const visibleCount = Math.min(node.items.length - offset, rect.height);
  let out = "";

  for (let i = 0; i < visibleCount; i++) {
    const itemIndex = offset + i;
    const itemNodes = node.items[itemIndex];
    if (!itemNodes) continue;

    for (const itemNode of itemNodes) {
      out += renderNode(itemNode, theme, boxStyle, opts, clip);
    }
  }

  return out;
}

// --- Composite renderers ---

function renderStatusBar(node: StatusBarNode, rect: Rect, theme: Theme): string {
  let out = "";
  out += bgColor(theme.bgAc) + fgColor(theme.fg1);
  out += fillLine(rect.y + 1, rect.x + 1, rect.width);
  out += writeAt(rect.y + 1, rect.x + 3, BOLD + node.left + RESET + bgColor(theme.bgAc));
  if (node.right) {
    const rightWidth = visibleLength(node.right);
    out += writeAt(rect.y + 1, rect.x + rect.width - rightWidth - 2,
      fgColor(theme.fg1) + bgColor(theme.bgAc) + node.right);
  }
  out += RESET;
  return out;
}

function renderFooter(node: FooterNode, rect: Rect, theme: Theme): string {
  let out = fgColor(theme.fgMu) + writeAt(rect.y + 1, rect.x + 2, node.hints);
  if (node.right) {
    const rightWidth = visibleLength(node.right);
    out += writeAt(rect.y + 1, rect.x + rect.width - rightWidth - 2, node.right);
  }
  return out + RESET;
}

function renderAskBar(
  node: AskBarNode,
  rect: Rect,
  theme: Theme,
  bs: BoxStyle,
): string {
  const style = node.style ?? bs;
  let out = "";

  // Background
  out += bgColor(theme.bg2) + fillRect(rect.y + 1, rect.x + 1, rect.width, rect.height);

  // Border
  out += (theme.border ? fg(theme.border[0], theme.border[1], theme.border[2]) : "")
    + drawBox(rect.y + 1, rect.x + 1, rect.width, rect.height, { style });

  if (node.active) {
    out += writeAt(rect.y + 2, rect.x + 4,
      fgColor(theme.fg1) + "> " + node.text + "\u2588");
  } else {
    out += writeAt(rect.y + 2, rect.x + 4, fgColor(theme.fgMu) + node.placeholder);
  }

  if (node.rightLabel) {
    const labelW = visibleLength(node.rightLabel);
    out += writeAt(rect.y + 2, rect.x + rect.width - labelW - 3,
      fgColor(theme.fg2) + node.rightLabel);
  }

  out += RESET;
  return out;
}

function renderTextInput(node: TextInputNode, rect: Rect, theme: Theme): string {
  if (node.active) {
    const before = node.text.slice(0, node.cursor);
    const after = node.text.slice(node.cursor);
    return moveTo(rect.y + 1, rect.x + 1) + fgColor(theme.fg1)
      + before + "\u2588" + after + RESET;
  }
  if (node.placeholder) {
    return moveTo(rect.y + 1, rect.x + 1) + fgColor(theme.fgMu)
      + node.placeholder + RESET;
  }
  return "";
}

function renderFPSCounter(rect: Rect, theme: Theme, opts: RenderOpts): string {
  if (!opts.showFPS) return "";
  const label = `${opts.fps} FPS`;
  return moveTo(rect.y + 1, rect.x + 1) + fgColor(theme.fgAc) + label + RESET;
}

/** Execute the canvas draw callback, then render the produced cells to ANSI. */
function renderCanvas(node: CanvasNode, rect: Rect, theme: Theme): string {
  // Run the draw callback to populate _cells
  executeCanvasDraw(node, rect, theme);

  // Render each cell
  let out = "";
  for (const cell of node._cells) {
    const absX = rect.x + cell.x;
    const absY = rect.y + cell.y;
    if (absY < 0 || absY >= rect.y + rect.height) continue;
    if (absX < 0 || absX >= rect.x + rect.width) continue;

    out += moveTo(absY + 1, absX + 1);
    const fgC = resolveColor(cell.color, theme);
    const bgC = resolveColor(cell.bg, theme);
    if (bgC) out += bgColor(bgC);
    if (fgC) out += fgColor(fgC);
    if (cell.bold) out += BOLD;
    if (cell.dim) out += DIM;
    out += cell.char;
    out += RESET;
  }
  return out;
}

/**
 * Run the canvas draw callback, building the DrawContext and collecting cells.
 * Exported so the buffer renderer can also call it.
 */
export function executeCanvasDraw(node: CanvasNode, rect: Rect, theme: Theme): void {
  node._cells = [];
  const cells = node._cells;
  const w = rect.width;
  const h = rect.height;

  const ctx: DrawContext = {
    width: w,
    height: h,

    set(x: number, y: number, char: string, color?: Color, bgc?: Color, bold?: boolean): void {
      if (x >= 0 && x < w && y >= 0 && y < h) {
        cells.push({ x, y, char, color, bg: bgc, bold });
      }
    },

    write(x: number, y: number, str: string, color?: Color, bgc?: Color, bold?: boolean): void {
      let cx = x;
      for (const ch of str) {
        if (cx >= w) break;
        if (cx >= 0 && y >= 0 && y < h) {
          cells.push({ x: cx, y, char: ch, color, bg: bgc, bold });
        }
        cx += charWidth(ch);
      }
    },

    fill(x: number, y: number, fw: number, fh: number, char?: string, color?: Color, bgc?: Color): void {
      const ch = char ?? " ";
      for (let fy = y; fy < y + fh && fy < h; fy++) {
        for (let fx = x; fx < x + fw && fx < w; fx++) {
          if (fx >= 0 && fy >= 0) {
            cells.push({ x: fx, y: fy, char: ch, color, bg: bgc });
          }
        }
      }
    },
  };

  node.draw(ctx);
}

/** Render an embedded PTY view by reading xterm cells directly. */
function renderPtyView(node: PtyViewNode, rect: Rect, theme: Theme): string {
  const handle = node.handle;

  // Resize the PTY to match the layout rect
  if (rect.width !== node._lastCols || rect.height !== node._lastRows) {
    handle.resize(rect.width, rect.height);
    node._lastCols = rect.width;
    node._lastRows = rect.height;
  }

  // Read cells directly from xterm's buffer — no serialize round-trip
  const cells = handle.readCells();

  let out = "";
  for (let r = 0; r < Math.min(cells.length, rect.height); r++) {
    const row = cells[r];
    if (!row) continue;
    out += moveTo(rect.y + r + 1, rect.x + 1);
    for (let c = 0; c < Math.min(row.length, rect.width); c++) {
      const cell = row[c];
      if (cell.bold) out += "\x1b[1m";
      if (cell.dim) out += "\x1b[2m";
      if (cell.italic) out += "\x1b[3m";
      if (cell.underline) out += "\x1b[4m";
      const fgC = cell.fg ?? theme.fg1;
      const bgC = cell.bg ?? theme.bg1;
      if (fgC) out += fg(fgC[0], fgC[1], fgC[2]);
      if (bgC) out += bg(bgC[0], bgC[1], bgC[2]);
      out += cell.char;
      out += RESET;
    }
  }
  return out;
}
