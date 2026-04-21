// screen() wrapper: bridges declarative UI nodes to the existing Screen interface

import type { Screen, ScreenContext, Cell } from "./types.ts";
import { emptyCell } from "./types.ts";
import type { KeyEvent } from "./input.ts";
import type { UINode, PanelNode, Rect } from "./nodes.ts";
import { layoutRoot, layoutVertical, layoutPanel, textWidth } from "./layout.ts";
import { renderToAnsi, resolveColor, executeCanvasDraw, type RenderOpts } from "./renderer.ts";
import { CellBuffer } from "./buffer.ts";
import {
  fillRect, drawBox, writeAt, bg, fg, reset as resetAnsi,
  BOLD, RESET, moveTo, hSep,
  charWidth, visibleLength, boxChars, wrapText,
  progressBar as renderProgressBarStr,
} from "./colors.ts";
import type { Theme, BoxStyle } from "./colors.ts";
import { spinnerChar, startSpinnerTimer, stopSpinnerTimer } from "./animation.ts";
import { getCurrentFPS, isFPSVisible } from "./fps.ts";

/** Check if any node in the tree is a spinner */
function treeHasSpinner(nodes: UINode[]): boolean {
  for (const node of nodes) {
    if (node.type === "spinner") return true;
    if (node.type === "row" || node.type === "column") {
      if (treeHasSpinner(node.children)) return true;
    }
    if (node.type === "hstack") {
      if (treeHasSpinner(node.children)) return true;
    }
    if (node.type === "panel") {
      if (treeHasSpinner(node.children)) return true;
    }
    if (node.type === "scrollable" || node.type === "selectable") {
      for (const item of node.items) {
        if (treeHasSpinner(item)) return true;
      }
    }
  }
  return false;
}

export interface DeclarativeScreenConfig {
  id: string;
  render: (ctx: ScreenContext) => UINode[];
  handleKey?: (key: KeyEvent, ctx: ScreenContext) => boolean;
  handleMouse?: (event: import("./input.ts").MouseEvent, ctx: ScreenContext) => boolean;
  onEnter?: (ctx: ScreenContext) => void;
  onLeave?: (ctx: ScreenContext) => void;
  /**
   * Game/animation tick loop. When set, a setInterval runs at the given `ms`
   * rate while this screen is active. The `update` callback should mutate
   * signals — the reactive render loop picks up the changes automatically.
   * The timer starts on onEnter and stops on onLeave.
   */
  tick?: {
    ms: number;
    update: () => void;
  };
}

/**
 * Creates a Screen from declarative UI node builders.
 * The returned Screen is fully compatible with the existing router.
 */
export function screen(config: DeclarativeScreenConfig): Screen {
  let hasSpinners = false;
  let tickTimer: ReturnType<typeof setInterval> | null = null;

  return {
    id: config.id,

    render(ctx: ScreenContext): string {
      const nodes = config.render(ctx);

      // Layout pass
      const viewport = { x: 0, y: 0, width: ctx.cols, height: ctx.rows };
      layoutRoot(nodes, viewport);

      // Manage spinner timer based on tree content
      const newHasSpinners = treeHasSpinner(nodes);
      if (newHasSpinners && !hasSpinners) startSpinnerTimer();
      if (!newHasSpinners && hasSpinners) stopSpinnerTimer();
      hasSpinners = newHasSpinners;

      // Build render opts — only track spinnerChar signal if spinners exist
      const opts: RenderOpts = {
        spinnerChar: newHasSpinners ? spinnerChar.get() : "\u280b",
        fps: getCurrentFPS(),
        showFPS: isFPSVisible(),
      };

      return renderToAnsi(nodes, ctx.theme, ctx.boxStyle, opts);
    },

    renderToBuffer(ctx: ScreenContext): CellBuffer {
      const nodes = config.render(ctx);

      // Layout pass
      const viewport = { x: 0, y: 0, width: ctx.cols, height: ctx.rows };
      layoutRoot(nodes, viewport);

      // Manage spinner timer based on tree content
      const newHasSpinners = treeHasSpinner(nodes);
      if (newHasSpinners && !hasSpinners) startSpinnerTimer();
      if (!newHasSpinners && hasSpinners) stopSpinnerTimer();
      hasSpinners = newHasSpinners;

      const opts: RenderOpts = {
        spinnerChar: newHasSpinners ? spinnerChar.get() : "\u280b",
        fps: getCurrentFPS(),
        showFPS: isFPSVisible(),
      };

      const buf = new CellBuffer(ctx.rows, ctx.cols);
      // Fill with theme bg1
      fillBufRect(buf, 0, 0, ctx.cols, ctx.rows, null, ctx.theme.bg1);
      renderTreeToBuffer(nodes, buf, ctx.theme, ctx.boxStyle, opts);
      return buf;
    },

    handleKey(key: KeyEvent, ctx: ScreenContext): boolean {
      return config.handleKey?.(key, ctx) ?? true;
    },

    handleMouse(event, ctx) {
      return config.handleMouse?.(event, ctx) ?? false;
    },

    onEnter(ctx: ScreenContext): void {
      config.onEnter?.(ctx);
      if (config.tick && !tickTimer) {
        tickTimer = setInterval(config.tick.update, config.tick.ms);
      }
    },

    onLeave(ctx: ScreenContext): void {
      if (tickTimer) {
        clearInterval(tickTimer);
        tickTimer = null;
      }
      if (hasSpinners) {
        stopSpinnerTimer();
        hasSpinners = false;
      }
      config.onLeave?.(ctx);
    },
  };
}

// --- Overlay wrapper ---

export interface OverlayConfig {
  id: string;
  title: string;
  width: number | ((cols: number) => number);
  height: number | ((rows: number) => number);
  render: (ctx: ScreenContext) => UINode[];
  handleKey?: (key: KeyEvent, ctx: ScreenContext) => boolean;
  handleMouse?: (event: import("./input.ts").MouseEvent, ctx: ScreenContext) => boolean;
  onEnter?: (ctx: ScreenContext) => void;
  onLeave?: (ctx: ScreenContext) => void;
}

/**
 * Creates a centered overlay Screen with shadow and panel chrome.
 * Content nodes are laid out inside the panel's content area.
 */
export function overlay(config: OverlayConfig): Screen {
  let hasSpinners = false;

  return {
    id: config.id,

    render(ctx: ScreenContext): string {
      const ow = typeof config.width === "function" ? config.width(ctx.cols) : config.width;
      const oh = typeof config.height === "function" ? config.height(ctx.rows) : config.height;
      const ox = Math.floor((ctx.cols - ow) / 2);
      const oy = Math.floor((ctx.rows - oh) / 2);

      let out = "";

      // Shadow (offset 1 down, 2 right)
      out += bg(8, 10, 16) + fillRect(oy + 2, ox + 3, ow, oh) + RESET;

      // Create a panel node for the overlay content
      const nodes = config.render(ctx);
      const panelNode: PanelNode = {
        type: "panel",
        title: config.title,
        children: nodes,
        style: ctx.boxStyle,
        _rect: { x: ox, y: oy, width: ow, height: oh },
      };

      // Layout panel content (handles separators correctly)
      layoutPanel(panelNode, { x: ox, y: oy, width: ow, height: oh });

      // Manage spinners
      const newHasSpinners = treeHasSpinner(nodes);
      if (newHasSpinners && !hasSpinners) startSpinnerTimer();
      if (!newHasSpinners && hasSpinners) stopSpinnerTimer();
      hasSpinners = newHasSpinners;

      const opts: RenderOpts = {
        spinnerChar: newHasSpinners ? spinnerChar.get() : "\u280b",
        fps: getCurrentFPS(),
        showFPS: isFPSVisible(),
      };

      // Render the panel (draws border, bg, title, children)
      out += renderToAnsi([panelNode], ctx.theme, ctx.boxStyle, opts);
      return out;
    },

    renderToBuffer(ctx: ScreenContext): CellBuffer {
      const ow = typeof config.width === "function" ? config.width(ctx.cols) : config.width;
      const oh = typeof config.height === "function" ? config.height(ctx.rows) : config.height;
      const ox = Math.floor((ctx.cols - ow) / 2);
      const oy = Math.floor((ctx.rows - oh) / 2);

      const buf = new CellBuffer(ctx.rows, ctx.cols);

      // Shadow (offset 1 down, 2 right)
      const shadowBg: [number, number, number] = [8, 10, 16];
      fillBufRect(buf, oy + 1, ox + 2, ow, oh, null, shadowBg);

      // Create a panel node for the overlay content
      const nodes = config.render(ctx);
      const panelNode: PanelNode = {
        type: "panel",
        title: config.title,
        children: nodes,
        style: ctx.boxStyle,
        _rect: { x: ox, y: oy, width: ow, height: oh },
      };

      layoutPanel(panelNode, { x: ox, y: oy, width: ow, height: oh });

      // Manage spinners
      const newHasSpinners = treeHasSpinner(nodes);
      if (newHasSpinners && !hasSpinners) startSpinnerTimer();
      if (!newHasSpinners && hasSpinners) stopSpinnerTimer();
      hasSpinners = newHasSpinners;

      const opts: RenderOpts = {
        spinnerChar: newHasSpinners ? spinnerChar.get() : "\u280b",
        fps: getCurrentFPS(),
        showFPS: isFPSVisible(),
      };

      renderTreeToBuffer([panelNode], buf, ctx.theme, ctx.boxStyle, opts);
      return buf;
    },

    handleKey(key: KeyEvent, ctx: ScreenContext): boolean {
      return config.handleKey?.(key, ctx) ?? true;
    },

    handleMouse(event, ctx) {
      return config.handleMouse?.(event, ctx) ?? false;
    },

    onEnter(ctx: ScreenContext): void {
      config.onEnter?.(ctx);
    },

    onLeave(ctx: ScreenContext): void {
      if (hasSpinners) {
        stopSpinnerTimer();
        hasSpinners = false;
      }
      config.onLeave?.(ctx);
    },
  };
}

// ==========================================================================
// Buffer rendering helpers — write positioned nodes directly to CellBuffer
// ==========================================================================

function makeCell(
  ch: string,
  fgc: [number, number, number] | null,
  bgc: [number, number, number] | null,
  bold: boolean = false,
  dim: boolean = false,
  italic: boolean = false,
): Cell {
  return { char: ch, fg: fgc ? [...fgc] : null, bg: bgc ? [...bgc] : null, bold, dim, italic, underline: false };
}

/** Write a plain string into the buffer at (row, col). Clips to buffer bounds. */
function writeStringBuf(
  buf: CellBuffer,
  row: number,
  col: number,
  str: string,
  fgc: [number, number, number] | null,
  bgc: [number, number, number] | null,
  bold: boolean = false,
  dim: boolean = false,
  italic: boolean = false,
): void {
  let c = col;
  for (const ch of str) {
    const cw = charWidth(ch);
    if (row < 0 || row >= buf.rows) return;
    if (c >= buf.cols) return;
    if (c >= 0) {
      // Preserve existing bg when bgc is null (e.g. text inside a panel)
      const effectiveBg = bgc ?? buf.cells[row][c]?.bg ?? null;
      buf.cells[row][c] = makeCell(ch, fgc, effectiveBg, bold, dim, italic);
      if (cw === 2 && c + 1 < buf.cols) {
        buf.cells[row][c + 1] = makeCell("", fgc, effectiveBg, bold, dim, italic);
      }
    }
    c += cw;
  }
}

/** Write a string into the buffer, applying highlight spans for per-character styling. */
function writeSpannedBuf(
  buf: CellBuffer,
  row: number,
  col: number,
  str: string,
  textOffset: number, // code-point offset of this line in the original text
  spans: import("./nodes.ts").Span[],
  defaultFg: [number, number, number] | null,
  defaultBold: boolean,
  defaultDim: boolean,
  defaultItalic: boolean,
  theme: Theme,
  maxWidth: number,
): void {
  let c = col;
  let charIdx = textOffset;
  for (const ch of str) {
    const cw = charWidth(ch);
    if (c - col >= maxWidth) break;
    if (row < 0 || row >= buf.rows) return;
    if (c >= buf.cols) break;

    // Find the span covering this character
    let fg = defaultFg;
    let bold = defaultBold;
    let dim = defaultDim;
    let italic = defaultItalic;
    for (const span of spans) {
      if (charIdx >= span.start && charIdx < span.end) {
        if (span.color !== undefined) fg = resolveColor(span.color, theme);
        if (span.bold !== undefined) bold = span.bold;
        if (span.dim !== undefined) dim = span.dim;
        if (span.italic !== undefined) italic = span.italic;
        break;
      }
    }

    if (c >= 0) {
      const effectiveBg = buf.cells[row][c]?.bg ?? null;
      buf.cells[row][c] = makeCell(ch, fg, effectiveBg, bold, dim, italic);
      if (cw === 2 && c + 1 < buf.cols) {
        buf.cells[row][c + 1] = makeCell("", fg, effectiveBg, bold, dim, italic);
      }
    }
    c += cw;
    charIdx++;
  }
}

/** Fill a rectangle in the buffer with a character. */
function fillBufRect(
  buf: CellBuffer,
  row: number,
  col: number,
  width: number,
  height: number,
  fgc: [number, number, number] | null = null,
  bgc: [number, number, number] | null = null,
  ch: string = " ",
): void {
  for (let r = row; r < row + height; r++) {
    if (r < 0 || r >= buf.rows) continue;
    for (let c = col; c < col + width; c++) {
      if (c < 0 || c >= buf.cols) continue;
      buf.cells[r][c] = makeCell(ch, fgc, bgc);
    }
  }
}

/** Draw a box border into the buffer. */
function drawBoxBuf(
  buf: CellBuffer,
  row: number,
  col: number,
  width: number,
  height: number,
  style: BoxStyle,
  fgc: [number, number, number] | null,
  bgc: [number, number, number] | null,
): void {
  const b = boxChars(style);
  // Top border
  if (row >= 0 && row < buf.rows) {
    if (col >= 0 && col < buf.cols) buf.cells[row][col] = makeCell(b.tl, fgc, bgc);
    for (let c = col + 1; c < col + width - 1; c++) {
      if (c >= 0 && c < buf.cols) buf.cells[row][c] = makeCell(b.h, fgc, bgc);
    }
    if (col + width - 1 >= 0 && col + width - 1 < buf.cols) buf.cells[row][col + width - 1] = makeCell(b.tr, fgc, bgc);
  }
  // Sides
  for (let r = row + 1; r < row + height - 1; r++) {
    if (r < 0 || r >= buf.rows) continue;
    if (col >= 0 && col < buf.cols) buf.cells[r][col] = makeCell(b.v, fgc, bgc);
    if (col + width - 1 >= 0 && col + width - 1 < buf.cols) buf.cells[r][col + width - 1] = makeCell(b.v, fgc, bgc);
  }
  // Bottom border
  const bottomRow = row + height - 1;
  if (bottomRow >= 0 && bottomRow < buf.rows) {
    if (col >= 0 && col < buf.cols) buf.cells[bottomRow][col] = makeCell(b.bl, fgc, bgc);
    for (let c = col + 1; c < col + width - 1; c++) {
      if (c >= 0 && c < buf.cols) buf.cells[bottomRow][c] = makeCell(b.h, fgc, bgc);
    }
    if (col + width - 1 >= 0 && col + width - 1 < buf.cols) buf.cells[bottomRow][col + width - 1] = makeCell(b.br, fgc, bgc);
  }
}

/** Draw a horizontal separator into the buffer. */
function hSepBuf(
  buf: CellBuffer,
  row: number,
  col: number,
  width: number,
  style: BoxStyle,
  fgc: [number, number, number] | null,
  bgc: [number, number, number] | null,
): void {
  if (row < 0 || row >= buf.rows) return;
  const b = boxChars(style);
  // Preserve the existing background at each cell. Separators live inside
  // panels; if we zero the bg to null here the row renders with the
  // terminal's default background, showing through as a grey band.
  const bgAt = (c: number): [number, number, number] | null =>
    bgc ?? buf.cells[row][c]?.bg ?? null;
  if (col >= 0 && col < buf.cols) buf.cells[row][col] = makeCell(b.lj, fgc, bgAt(col));
  for (let c = col + 1; c < col + width - 1; c++) {
    if (c >= 0 && c < buf.cols) buf.cells[row][c] = makeCell(b.h, fgc, bgAt(c));
  }
  const last = col + width - 1;
  if (last >= 0 && last < buf.cols) buf.cells[row][last] = makeCell(b.rj, fgc, bgAt(last));
}

// --- Main tree-to-buffer renderer ---

function renderTreeToBuffer(
  nodes: UINode[],
  buf: CellBuffer,
  theme: Theme,
  boxStyle: BoxStyle,
  opts: RenderOpts,
): void {
  for (const node of nodes) {
    renderNodeToBuffer(node, buf, theme, boxStyle, opts);
  }
}

function renderNodeToBuffer(
  node: UINode,
  buf: CellBuffer,
  theme: Theme,
  bs: BoxStyle,
  opts: RenderOpts,
): void {
  if (!node._rect) return;
  const rect = node._rect;
  if (rect.width <= 0 || rect.height <= 0) return;

  switch (node.type) {
    case "text": {
      let fgColor = resolveColor(node.color, theme);
      let bgColor = resolveColor(node.background, theme);
      // inverse: swap fg/bg. If bg wasn't set, fall back to a sensible
      // default so the cell is visibly "highlighted" — use the ambient
      // foreground as the new bg, and the ambient bg as the new fg.
      if (node.inverse) {
        const fallbackBg = fgColor ?? theme.fg1;
        const fallbackFg = bgColor ?? theme.bg1;
        fgColor = fallbackFg;
        bgColor = fallbackBg;
      }
      const defaultBold = node.bold ?? false;
      const defaultDim = node.dim ?? false;
      const defaultItalic = node.italic ?? false;
      const content = node.text;

      // Compute highlight spans once (if any)
      const spans = node.highlight ? node.highlight(content) : null;

      if (node.wrap && rect.width > 0) {
        // Soft-wrap: split into visual lines and render each
        const { lines, offsets } = wrapText(content, rect.width);
        const maxLines = Math.min(lines.length, rect.height);
        for (let i = 0; i < maxLines; i++) {
          const lineY = rect.y + i;
          if (lineY >= buf.rows) break;
          if (spans) {
            writeSpannedBuf(buf, lineY, rect.x, lines[i], offsets[i], spans, fgColor, defaultBold, defaultDim, defaultItalic, theme, rect.width);
          } else {
            writeStringBuf(buf, lineY, rect.x, lines[i], fgColor, bgColor, defaultBold, defaultDim, defaultItalic);
          }
        }
      } else {
        // Single line: truncate if needed
        let displayContent = content;
        if ((node.truncate || textWidth(content) > rect.width) && textWidth(content) > rect.width) {
          if (rect.width <= 1) {
            displayContent = content.slice(0, rect.width);
          } else {
            let w = 0;
            let result = "";
            for (const ch of content) {
              const cw = charWidth(ch);
              if (w + cw + 1 > rect.width) break;
              result += ch;
              w += cw;
            }
            displayContent = result + "\u2026";
          }
        }
        if (spans) {
          writeSpannedBuf(buf, rect.y, rect.x, displayContent, 0, spans, fgColor, defaultBold, defaultDim, defaultItalic, theme, rect.width);
        } else {
          writeStringBuf(buf, rect.y, rect.x, displayContent, fgColor, bgColor, defaultBold, defaultDim, defaultItalic);
        }
      }
      break;
    }
    case "spacer":
    case "gap":
    case "indent":
      break;
    case "separator":
      hSepBuf(buf, rect.y, rect.x, rect.width, bs, theme.border, null);
      break;
    case "dot": {
      const color = resolveColor(node.color, theme);
      const ch = node.filled ? "\u25cf" : "\u25cb";
      writeStringBuf(buf, rect.y, rect.x, ch, color, null);
      break;
    }
    case "checkbox": {
      const color = resolveColor(node.color, theme);
      const ch = node.checked ? "\u25a0" : "\u25a1";
      writeStringBuf(buf, rect.y, rect.x, ch, color, null);
      break;
    }
    case "progressBar": {
      const color = resolveColor(node.color, theme) ?? theme.info;
      const barStr = renderProgressBarStr(rect.width, node.percent);
      writeStringBuf(buf, rect.y, rect.x, barStr, color, null);
      break;
    }
    case "spinner": {
      const color = resolveColor(node.color, theme) ?? theme.fgAc;
      writeStringBuf(buf, rect.y, rect.x, opts.spinnerChar, color, null);
      break;
    }
    case "icon": {
      const color = resolveColor(node.color, theme);
      writeStringBuf(buf, rect.y, rect.x, node.char, color, null);
      break;
    }
    case "row":
    case "column":
      for (const child of node.children) {
        renderNodeToBuffer(child, buf, theme, bs, opts);
      }
      break;
    case "hstack":
      for (const col of node.children) {
        renderNodeToBuffer(col, buf, theme, bs, opts);
      }
      break;
    case "panel": {
      const style = node.style ?? bs;
      // Fill panel background
      fillBufRect(buf, rect.y, rect.x, rect.width, rect.height, null, theme.bg2);
      // Draw border
      drawBoxBuf(buf, rect.y, rect.x, rect.width, rect.height, style, theme.border, theme.bg2);
      // Top title
      if (node.title) {
        writeStringBuf(buf, rect.y, rect.x + 2, " ", theme.border, theme.bg2);
        writeStringBuf(buf, rect.y, rect.x + 3, node.title, theme.fgAc, theme.bg2, true);
        const titleEnd = rect.x + 3 + textWidth(node.title);
        writeStringBuf(buf, rect.y, titleEnd, " ", theme.border, theme.bg2);
      }
      // Bottom caption (optional) — rendered on the bottom border with the
      // same chrome as the top title. Left-aligned to mirror the top.
      if (node.footerTitle) {
        const by = rect.y + rect.height - 1;
        writeStringBuf(buf, by, rect.x + 2, " ", theme.border, theme.bg2);
        writeStringBuf(buf, by, rect.x + 3, node.footerTitle, theme.fgAc, theme.bg2, true);
        const capEnd = rect.x + 3 + textWidth(node.footerTitle);
        writeStringBuf(buf, by, capEnd, " ", theme.border, theme.bg2);
      }
      // Render children
      for (const child of node.children) {
        renderNodeToBuffer(child, buf, theme, style, opts);
      }
      break;
    }
    case "scrollable":
    case "selectable": {
      const offset = node.offset;
      const visibleCount = Math.min(node.items.length - offset, rect.height);
      for (let i = 0; i < visibleCount; i++) {
        const itemIndex = offset + i;
        const itemNodes = node.items[itemIndex];
        if (!itemNodes) continue;
        for (const itemNode of itemNodes) {
          renderNodeToBuffer(itemNode, buf, theme, bs, opts);
        }
      }
      break;
    }
    case "statusBar": {
      // Fill line with accent background
      fillBufRect(buf, rect.y, rect.x, rect.width, 1, theme.fg1, theme.bgAc);
      writeStringBuf(buf, rect.y, rect.x + 2, node.left, theme.fg1, theme.bgAc, true);
      if (node.right) {
        const rightWidth = visibleLength(node.right);
        // node.right may contain ANSI — strip it for buffer rendering
        const plain = node.right.replace(/\x1b\[[0-9;]*[a-zA-Z]/g, "");
        writeStringBuf(buf, rect.y, rect.x + rect.width - rightWidth - 2, plain, theme.fg1, theme.bgAc);
      }
      break;
    }
    case "footer": {
      writeStringBuf(buf, rect.y, rect.x + 1, node.hints, theme.fgMu, null);
      if (node.right) {
        const rightWidth = visibleLength(node.right);
        const plain = node.right.replace(/\x1b\[[0-9;]*[a-zA-Z]/g, "");
        writeStringBuf(buf, rect.y, rect.x + rect.width - rightWidth - 2, plain, theme.fgMu, null);
      }
      break;
    }
    case "askBar": {
      const style = node.style ?? bs;
      // Background
      fillBufRect(buf, rect.y, rect.x, rect.width, rect.height, null, theme.bg2);
      // Border
      drawBoxBuf(buf, rect.y, rect.x, rect.width, rect.height, style, theme.border, theme.bg2);
      if (node.active) {
        writeStringBuf(buf, rect.y + 1, rect.x + 3, "> " + node.text + "\u2588", theme.fg1, theme.bg2);
      } else {
        writeStringBuf(buf, rect.y + 1, rect.x + 3, node.placeholder, theme.fgMu, theme.bg2);
      }
      if (node.rightLabel) {
        const labelW = visibleLength(node.rightLabel);
        writeStringBuf(buf, rect.y + 1, rect.x + rect.width - labelW - 3, node.rightLabel, theme.fg2, theme.bg2);
      }
      break;
    }
    case "textInput": {
      if (node.active) {
        const before = node.text.slice(0, node.cursor);
        const after = node.text.slice(node.cursor);
        writeStringBuf(buf, rect.y, rect.x, before + "\u2588" + after, theme.fg1, null);
      } else if (node.placeholder) {
        writeStringBuf(buf, rect.y, rect.x, node.placeholder, theme.fgMu, null);
      }
      break;
    }
    case "fpsCounter": {
      if (!opts.showFPS) break;
      const label = `${opts.fps} FPS`;
      writeStringBuf(buf, rect.y, rect.x, label, theme.fgAc, null);
      break;
    }
    case "canvas": {
      executeCanvasDraw(node, rect, theme);
      for (const cell of node._cells) {
        const absX = rect.x + cell.x;
        const absY = rect.y + cell.y;
        if (absY < 0 || absY >= rect.y + rect.height) continue;
        if (absX < 0 || absX >= rect.x + rect.width) continue;
        const fgC = resolveColor(cell.color, theme);
        const bgC = resolveColor(cell.bg, theme);
        if (absY >= 0 && absY < buf.rows && absX >= 0 && absX < buf.cols) {
          buf.cells[absY][absX] = makeCell(cell.char, fgC, bgC, cell.bold ?? false, cell.dim ?? false);
        }
      }
      break;
    }
    case "ptyView": {
      // Resize PTY to match layout
      if (rect.width !== node._lastCols || rect.height !== node._lastRows) {
        node.handle.resize(rect.width, rect.height);
        node._lastCols = rect.width;
        node._lastRows = rect.height;
      }
      // Subscribe the enclosing effect() to the handle's revision signal
      // so the next data arrival / exit / resize / theme change triggers
      // a re-render automatically. Without this, output from the embedded
      // pty would "buffer" until some OTHER signal read forced a render
      // (like a keypress), because nothing else in this path is tracked.
      node.handle.rev.get();
      // Read xterm cells directly — no serialize round-trip
      const ptyCells = node.handle.readCells();
      for (let r = 0; r < Math.min(ptyCells.length, rect.height); r++) {
        const absY = rect.y + r;
        if (absY < 0 || absY >= buf.rows) continue;
        const ptyRow = ptyCells[r];
        if (!ptyRow) continue;
        for (let c = 0; c < Math.min(ptyRow.length, rect.width); c++) {
          const absX = rect.x + c;
          if (absX < 0 || absX >= buf.cols) continue;
          const cell = ptyRow[c];
          buf.cells[absY][absX] = makeCell(
            cell.char || " ",
            cell.fg ?? (theme.fg1 ? [...theme.fg1] : null),
            cell.bg ?? (theme.bg1 ? [...theme.bg1] : null),
            cell.bold, cell.dim, cell.italic,
          );
        }
      }
      break;
    }
  }
}
