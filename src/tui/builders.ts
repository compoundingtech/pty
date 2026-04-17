// Builder functions for constructing UI node trees

import type {
  UINode, Color, TextNode, SpacerNode, GapNode, SeparatorNode,
  IndentNode, DotNode, CheckboxNode, ProgressBarNode, SpinnerNode,
  IconNode, RowNode, ColumnNode, HStackNode, PanelNode,
  ScrollableNode, SelectableNode, StatusBarNode, FooterNode,
  AskBarNode, TextInputNode, FPSCounterNode, CanvasNode,
  DrawContext, PtyHandle, PtyViewNode,
} from "./nodes.ts";
import type { BoxStyle, Theme } from "./colors.ts";
import type { ScrollRegion } from "./scrollable.ts";
import type { TextInputState } from "./text-input.ts";
import { createRequire } from "node:module";
const require = createRequire(import.meta.url);

function rgbToHex(c: [number, number, number]): string {
  return "#" + c.map(v => v.toString(16).padStart(2, "0")).join("");
}

function brighten(c: [number, number, number], amt = 40): [number, number, number] {
  return [Math.min(255, c[0] + amt), Math.min(255, c[1] + amt), Math.min(255, c[2] + amt)];
}

/**
 * Convert our app Theme into an xterm ITheme with a full 16-color ANSI palette.
 * This makes CLI tools (ls, git, etc.) inside embedded PTYs use colors that
 * are coherent with the surrounding UI.
 */
function hexOrDefault(c: [number, number, number] | null, fallback: string): string {
  return c ? rgbToHex(c) : fallback;
}

export function themeToXterm(theme: Theme): Record<string, string> {
  return {
    foreground: hexOrDefault(theme.fg1, "#d2dae8"),
    background: hexOrDefault(theme.bg1, "#0f111a"),
    cursor: hexOrDefault(theme.fgAc, "#64a0ff"),
    cursorAccent: hexOrDefault(theme.bg1, "#0f111a"),
    selection: hexOrDefault(theme.bgHi, "#1e2841"),
    // Standard 8 colors
    black: hexOrDefault(theme.bg1, "#0f111a"),
    red: hexOrDefault(theme.err, "#f05050"),
    green: hexOrDefault(theme.ok, "#50c878"),
    yellow: hexOrDefault(theme.warn, "#f0b432"),
    blue: hexOrDefault(theme.info, "#50aaf0"),
    magenta: hexOrDefault(theme.fgAc, "#64a0ff"),
    cyan: hexOrDefault(theme.fg2, "#8c9bb9"),
    white: hexOrDefault(theme.fg1, "#d2dae8"),
    // Bright variants
    brightBlack: hexOrDefault(theme.fgMu, "#465069"),
    brightRed: hexOrDefault(theme.err ? brighten(theme.err) : null, "#ff6666"),
    brightGreen: hexOrDefault(theme.ok ? brighten(theme.ok) : null, "#78e0a0"),
    brightYellow: hexOrDefault(theme.warn ? brighten(theme.warn) : null, "#ffcc5a"),
    brightBlue: hexOrDefault(theme.info ? brighten(theme.info) : null, "#78c8ff"),
    brightMagenta: hexOrDefault(theme.fgAc ? brighten(theme.fgAc) : null, "#8cc0ff"),
    brightCyan: hexOrDefault(theme.fg2 ? brighten(theme.fg2) : null, "#b4c3e1"),
    brightWhite: hexOrDefault(theme.fg1 ? brighten(theme.fg1) : null, "#ffffff"),
  };
}

export function text(
  str: string,
  color?: Color,
  opts?: { bold?: boolean; dim?: boolean; italic?: boolean; truncate?: boolean; wrap?: boolean; highlight?: (text: string) => import("./nodes.ts").Span[] },
): TextNode {
  return { type: "text", text: str, color, ...opts };
}

export function spacer(): SpacerNode {
  return { type: "spacer" };
}

export function gap(size: number | "center"): GapNode {
  return { type: "gap", size };
}

export function separator(): SeparatorNode {
  return { type: "separator" };
}

export function indent(depth: number): IndentNode {
  return { type: "indent", depth };
}

export function dot(filled: boolean, color?: Color): DotNode {
  return { type: "dot", filled, color };
}

export function checkbox(checked: boolean, color?: Color): CheckboxNode {
  return { type: "checkbox", checked, color };
}

export function progressBar(
  percent: number,
  opts?: { width?: number; color?: Color },
): ProgressBarNode {
  return { type: "progressBar", percent, width: opts?.width, color: opts?.color };
}

export function spinner(color?: Color): SpinnerNode {
  return { type: "spinner", color };
}

export function icon(char: string, color?: Color): IconNode {
  return { type: "icon", char, color };
}

export function row(...children: UINode[]): RowNode {
  return { type: "row", children };
}

export function column(
  opts: { width?: number; flex?: boolean },
  children: UINode[],
): ColumnNode {
  return { type: "column", children, width: opts.width, flex: opts.flex };
}

export function hstack(
  opts: { gap?: number },
  children: ColumnNode[],
): HStackNode {
  return { type: "hstack", children, gap: opts.gap };
}

export function panel(
  title: string,
  children: UINode[],
  style?: BoxStyle,
): PanelNode {
  return { type: "panel", title, children, style };
}

export function scrollable<T>(
  items: T[],
  renderFn: (item: T, index: number) => UINode[],
): ScrollableNode {
  const rendered = items.map((item, i) => renderFn(item, i));
  return {
    type: "scrollable",
    items: rendered,
    offset: 0,
    totalItems: items.length,
  };
}

export function selectable<T>(
  region: ScrollRegion,
  items: T[],
  renderFn: (item: T, index: number, selected: boolean) => UINode[],
): SelectableNode {
  const rendered = items.map((item, i) => renderFn(item, i, i === region.selectedIndex));
  return {
    type: "selectable",
    items: rendered,
    selectedIndex: region.selectedIndex,
    offset: region.offset,
    totalItems: region.totalItems,
  };
}

/**
 * Grouped selectable: renders groups of items with section headers.
 * The ScrollRegion's selectedIndex counts only selectable items (not headers).
 * Headers and spacing rows are included in the visual output but the
 * scroll offset is mapped from item-space to visual-row-space automatically.
 */
export interface SelectableGroup<T> {
  title: string;
  items: T[];
}

export function groupedSelectable<T>(
  region: ScrollRegion,
  groups: SelectableGroup<T>[],
  renderItem: (item: T, globalIndex: number, selected: boolean) => UINode[],
  renderHeader: (group: SelectableGroup<T>) => UINode[] =
    (group) => [text(group.title, "accent", { bold: true }), text(` (${group.items.length})`, "muted")],
): SelectableNode {
  const allRows: UINode[][] = [];
  let globalIdx = 0;
  // Track which visual row each selectable item maps to
  let selectedVisualRow = 0;

  for (const group of groups) {
    if (allRows.length > 0) allRows.push([]); // spacing
    allRows.push(renderHeader(group)); // header
    for (const item of group.items) {
      if (globalIdx === region.selectedIndex) {
        selectedVisualRow = allRows.length;
      }
      allRows.push(renderItem(item, globalIdx, globalIdx === region.selectedIndex));
      globalIdx++;
    }
  }

  // Compute visual offset: try to keep the selected row visible in the viewport.
  // Start from 0 and scroll down enough so the selected row is in view.
  let visualOffset = 0;
  if (selectedVisualRow >= region.viewportHeight) {
    visualOffset = selectedVisualRow - region.viewportHeight + 2;
  }

  return {
    type: "selectable",
    items: allRows,
    selectedIndex: region.selectedIndex,
    offset: Math.max(0, visualOffset),
    totalItems: region.totalItems,
  };
}

export function statusBar(left: string, right: string): StatusBarNode {
  return { type: "statusBar", left, right };
}

export function footer(hints: string, right?: string): FooterNode {
  return right == null ? { type: "footer", hints } : { type: "footer", hints, right };
}

export function askBar(
  state: TextInputState,
  opts?: { placeholder?: string; rightLabel?: string; style?: BoxStyle },
): AskBarNode {
  return {
    type: "askBar",
    text: state.text,
    placeholder: opts?.placeholder ?? "> Ask anything...",
    active: state.active,
    rightLabel: opts?.rightLabel,
    style: opts?.style,
  };
}

export function textInput(
  state: TextInputState,
  opts?: { placeholder?: string },
): TextInputNode {
  return {
    type: "textInput",
    text: state.text,
    cursor: state.cursor,
    active: state.active,
    placeholder: opts?.placeholder,
  };
}

export function fpsCounter(): FPSCounterNode {
  return { type: "fpsCounter" };
}

/**
 * Free-form drawing surface. Participates in layout like any other node
 * (flex by default, or set fixed height/widthHint), but gives you a
 * DrawContext callback to place characters at arbitrary positions.
 *
 * ```
 * canvas((ctx) => {
 *   ctx.fill(0, 0, ctx.width, ctx.height, ".", "muted");
 *   ctx.write(2, 1, "Player", "ok");
 *   ctx.set(playerX, playerY, "@", "accent");
 * })
 * ```
 */
export function canvas(
  draw: (ctx: DrawContext) => void,
  opts?: { height?: number; width?: number },
): CanvasNode {
  return {
    type: "canvas",
    draw,
    height: opts?.height,
    widthHint: opts?.width,
    _cells: [],
  };
}

/**
 * Spawn a child process in an embedded PTY. Returns a PtyHandle that you
 * manage in onEnter/onLeave and render with ptyView().
 *
 * ```
 * const handle = createPty("/bin/bash", [], { cols: 80, rows: 24 });
 * // in render: ptyView(handle)
 * // in handleKey: handle.write(key sequence)
 * // in onLeave: handle.kill()
 * ```
 */
// --- Palette → RGB conversion (for xterm palette color mode) ---
const PALETTE_16: [number, number, number][] = [
  [0, 0, 0], [204, 0, 0], [0, 204, 0], [204, 204, 0],
  [0, 0, 204], [204, 0, 204], [0, 204, 204], [204, 204, 204],
  [85, 85, 85], [255, 85, 85], [85, 255, 85], [255, 255, 85],
  [85, 85, 255], [255, 85, 255], [85, 255, 255], [255, 255, 255],
];
function paletteToRgb(n: number): [number, number, number] {
  if (n < 16) return PALETTE_16[n] ?? [255, 255, 255];
  if (n >= 232) { const v = (n - 232) * 10 + 8; return [v, v, v]; }
  const idx = n - 16;
  return [Math.floor(idx / 36) * 51, Math.floor((idx % 36) / 6) * 51, (idx % 6) * 51];
}

/**
 * Read xterm-headless terminal buffer cells directly with full color fidelity.
 * No serialize round-trip. Uses xterm's cell API to extract RGB/palette/default colors.
 */
function readXtermCells(
  terminal: any,
  rows: number,
  cols: number,
  scrollOffset: number = 0,
): ReturnType<PtyHandle["readCells"]> {
  const buf = terminal.buffer.active;
  const baseY = buf.baseY as number;
  const startLine = Math.max(0, baseY - scrollOffset);
  const grid: ReturnType<PtyHandle["readCells"]> = [];

  for (let r = 0; r < rows; r++) {
    const lineIdx = startLine + r;
    const line = lineIdx < buf.length ? buf.getLine(lineIdx) : null;
    const row: typeof grid[0] = [];
    for (let c = 0; c < cols; c++) {
      if (!line) {
        row.push({ char: " ", fg: null, bg: null, bold: false, dim: false, italic: false, underline: false });
        continue;
      }
      const cell = line.getCell(c);
      if (!cell) {
        row.push({ char: " ", fg: null, bg: null, bold: false, dim: false, italic: false, underline: false });
        continue;
      }

      // Extract foreground color
      let fg: [number, number, number] | null = null;
      if (cell.isFgRGB()) {
        const v = cell.getFgColor();
        fg = [(v >> 16) & 0xff, (v >> 8) & 0xff, v & 0xff];
      } else if (cell.isFgPalette()) {
        fg = paletteToRgb(cell.getFgColor());
      }
      // isFgDefault() → null (use theme default)

      // Extract background color
      let bg: [number, number, number] | null = null;
      if (cell.isBgRGB()) {
        const v = cell.getBgColor();
        bg = [(v >> 16) & 0xff, (v >> 8) & 0xff, v & 0xff];
      } else if (cell.isBgPalette()) {
        bg = paletteToRgb(cell.getBgColor());
      }
      // isBgDefault() → null (use theme default)

      row.push({
        char: cell.getChars() || " ",
        fg,
        bg,
        bold: !!cell.isBold(),
        dim: !!cell.isDim(),
        italic: !!cell.isItalic(),
        underline: !!cell.isUnderline(),
      });
    }
    grid.push(row);
  }
  return grid;
}

/** Read per-row `isWrapped` flags aligned with `readXtermCells(scrollOffset)`.
 *  A `true` at index `r` means that row continues the previous row because
 *  xterm wrapped a long line (not because the child emitted `\n`). Used by
 *  consumers reconstructing logical lines from a visually-multi-row
 *  selection — e.g., copying a wrapped URL without a spurious newline. */
function readXtermWrappedFlags(
  terminal: any,
  rows: number,
  scrollOffset: number = 0,
): boolean[] {
  const buf = terminal.buffer.active;
  const baseY = buf.baseY as number;
  const startLine = Math.max(0, baseY - scrollOffset);
  const flags: boolean[] = new Array(rows);
  for (let r = 0; r < rows; r++) {
    const lineIdx = startLine + r;
    const line = lineIdx < buf.length ? buf.getLine(lineIdx) : null;
    flags[r] = !!(line?.isWrapped);
  }
  return flags;
}

export function createPty(
  command: string,
  args: string[] = [],
  opts?: { cols?: number; rows?: number; scrollback?: number; cwd?: string; env?: Record<string, string>; theme?: Theme },
): PtyHandle {
  // Lazy-import node-pty and @xterm/headless to avoid top-level side effects
  // and keep the module loadable in test environments that don't need PTY.
  const pty = require("node-pty") as typeof import("node-pty");
  const xtermMod = require("@xterm/headless") as { Terminal: typeof import("@xterm/headless").Terminal };
  // @xterm/headless is CJS — the Terminal class may be on .default or directly
  const TerminalClass = (xtermMod as any).default?.Terminal ?? xtermMod.Terminal;

  const cols = opts?.cols ?? 80;
  const rows = opts?.rows ?? 24;
  const scrollbackSize = opts?.scrollback ?? 0;

  const xtermOpts: any = { rows, cols, scrollback: scrollbackSize, allowProposedApi: true };
  if (opts?.theme) xtermOpts.theme = themeToXterm(opts.theme);
  const terminal = new TerminalClass(xtermOpts);

  // Track mouse mode via CSI parser (same approach as server.ts)
  let mouseMode = false;
  terminal.parser.registerCsiHandler(
    { prefix: "?", final: "h" },
    (params: any) => {
      for (const p of params) {
        const v = typeof p === "number" ? p : p[0];
        if (v === 1000 || v === 1002 || v === 1003) mouseMode = true;
      }
      return false;
    },
  );
  terminal.parser.registerCsiHandler(
    { prefix: "?", final: "l" },
    (params: any) => {
      for (const p of params) {
        const v = typeof p === "number" ? p : p[0];
        if (v === 1000 || v === 1002 || v === 1003) mouseMode = false;
      }
      return false;
    },
  );

  // Track kitty keyboard protocol flag stack
  const kittyKeyboardStack: number[] = [];
  terminal.parser.registerCsiHandler(
    { prefix: ">", final: "u" },
    (params: any) => {
      const p = params[0];
      kittyKeyboardStack.push(typeof p === "number" ? p : p[0]);
      return false;
    },
  );
  terminal.parser.registerCsiHandler(
    { prefix: "<", final: "u" },
    () => {
      kittyKeyboardStack.pop();
      return false;
    },
  );

  const childEnv: Record<string, string> = {
    ...(process.env as Record<string, string>),
    ...opts?.env,
    TERM: "xterm-256color",
  };

  const proc = pty.spawn(command, args, {
    name: "xterm-256color",
    cols,
    rows,
    cwd: opts?.cwd ?? process.cwd(),
    env: childEnv,
  });

  let exited = false;
  let dirty = false;

  proc.onData((data: string) => {
    terminal.write(data, () => {
      dirty = true;
      handle.onActivity?.();
    });
  });
  proc.onExit(() => {
    exited = true;
    dirty = true;
    handle.onActivity?.();
  });

  const handle: PtyHandle = {
    write(data: string) { if (!exited) proc.write(data); },

    resize(newCols: number, newRows: number) {
      if (!exited && (newCols !== handle.cols || newRows !== handle.rows)) {
        handle.cols = newCols;
        handle.rows = newRows;
        proc.resize(newCols, newRows);
        terminal.resize(newCols, newRows);
        dirty = true;
      }
    },

    readCells(scrollOffset?: number) {
      return readXtermCells(terminal, handle.rows, handle.cols, scrollOffset ?? 0);
    },

    readWrappedFlags(scrollOffset?: number) {
      return readXtermWrappedFlags(terminal, handle.rows, scrollOffset ?? 0);
    },

    kill() {
      if (!exited) {
        try { proc.kill(); } catch {}
        exited = true;
      }
      terminal.dispose();
    },

    setTheme(t: Theme) {
      terminal.options.theme = themeToXterm(t);
      dirty = true;
    },

    cols,
    rows,
    get exited() { return exited; },
    get dirty() { return dirty; },
    set dirty(v: boolean) { dirty = v; },
    onActivity: null,
    get cursorRow() { return terminal.buffer.active.cursorY; },
    get cursorCol() { return terminal.buffer.active.cursorX; },
    get mouseMode() { return mouseMode; },
    get alternateScreen() { return terminal.buffer.active.type === "alternate"; },
    get kittyKeyboardFlags() { return [...kittyKeyboardStack]; },
    get scrollback() { return scrollbackSize; },
    get bufferLength() { return terminal.buffer.active.length; },
    get baseY() { return terminal.buffer.active.baseY; },
  };

  return handle;
}

/**
 * Attach to an existing named PTY session (started with `pty run`).
 * Returns the same PtyHandle interface, but the child process is owned
 * by the external daemon — kill() detaches instead of terminating it.
 *
 * ```
 * const handle = await attachPty("my-server");
 * // in render: ptyView(handle)
 * // in handleKey: handle.write(key sequence)
 * // in onLeave: handle.kill() — detaches, process keeps running
 * ```
 */
export async function attachPty(
  name: string,
  opts?: { cols?: number; rows?: number; scrollback?: number; theme?: Theme },
): Promise<PtyHandle> {
  const net = require("node:net") as typeof import("node:net");
  const xtermMod = require("@xterm/headless") as any;
  const TerminalClass = xtermMod.default?.Terminal ?? xtermMod.Terminal;
  const { getSocketPath } = require("../sessions.js") as typeof import("../sessions.ts");
  const {
    MessageType, PacketReader, encodeAttach, encodeData, encodeResize, encodeDetach,
  } = require("../protocol.js") as typeof import("../protocol.ts");

  const cols = opts?.cols ?? 80;
  const rows = opts?.rows ?? 24;
  const scrollbackSize = opts?.scrollback ?? 0;

  const xtermOpts: any = { rows, cols, scrollback: scrollbackSize, allowProposedApi: true };
  if (opts?.theme) xtermOpts.theme = themeToXterm(opts.theme);
  const terminal = new TerminalClass(xtermOpts);

  // Track mouse mode via CSI parser
  let mouseMode = false;
  terminal.parser.registerCsiHandler(
    { prefix: "?", final: "h" },
    (params: any) => {
      for (const p of params) {
        const v = typeof p === "number" ? p : p[0];
        if (v === 1000 || v === 1002 || v === 1003) mouseMode = true;
      }
      return false;
    },
  );
  terminal.parser.registerCsiHandler(
    { prefix: "?", final: "l" },
    (params: any) => {
      for (const p of params) {
        const v = typeof p === "number" ? p : p[0];
        if (v === 1000 || v === 1002 || v === 1003) mouseMode = false;
      }
      return false;
    },
  );

  // Track kitty keyboard protocol flag stack
  const kittyKeyboardStack: number[] = [];
  terminal.parser.registerCsiHandler(
    { prefix: ">", final: "u" },
    (params: any) => {
      const p = params[0];
      kittyKeyboardStack.push(typeof p === "number" ? p : p[0]);
      return false;
    },
  );
  terminal.parser.registerCsiHandler(
    { prefix: "<", final: "u" },
    () => {
      kittyKeyboardStack.pop();
      return false;
    },
  );

  const socketPath = getSocketPath(name);

  // Connect to the daemon socket
  const socket: import("node:net").Socket = await new Promise((resolve, reject) => {
    const s = net.createConnection(socketPath);
    s.on("connect", () => resolve(s));
    s.on("error", (err) => reject(new Error(`Failed to attach to session "${name}": ${err.message}`)));
  });

  let exited = false;
  let exitCode: number | null = null;
  let dirty = false;
  const reader = new PacketReader();

  const handle: PtyHandle = {
    write(data: string) { if (!exited) socket.write(encodeData(data)); },

    resize(newCols: number, newRows: number) {
      if (!exited && (newCols !== handle.cols || newRows !== handle.rows)) {
        handle.cols = newCols;
        handle.rows = newRows;
        socket.write(encodeResize(newRows, newCols));
        terminal.resize(newCols, newRows);
        dirty = true;
      }
    },

    readCells(scrollOffset?: number) {
      return readXtermCells(terminal, handle.rows, handle.cols, scrollOffset ?? 0);
    },

    readWrappedFlags(scrollOffset?: number) {
      return readXtermWrappedFlags(terminal, handle.rows, scrollOffset ?? 0);
    },

    kill() {
      // Detach only — the daemon keeps the process running
      try { socket.write(encodeDetach()); } catch {}
      try { socket.destroy(); } catch {}
      terminal.dispose();
      exited = true;
    },

    setTheme(t: Theme) {
      terminal.options.theme = themeToXterm(t);
      dirty = true;
    },

    cols,
    rows,
    get exited() { return exited; },
    get dirty() { return dirty; },
    set dirty(v: boolean) { dirty = v; },
    onActivity: null,
    get cursorRow() { return terminal.buffer.active.cursorY; },
    get cursorCol() { return terminal.buffer.active.cursorX; },
    get mouseMode() { return mouseMode; },
    get alternateScreen() { return terminal.buffer.active.type === "alternate"; },
    get kittyKeyboardFlags() { return [...kittyKeyboardStack]; },
    get scrollback() { return scrollbackSize; },
    get bufferLength() { return terminal.buffer.active.length; },
    get baseY() { return terminal.buffer.active.baseY; },
  };

  socket.on("data", (data: Buffer) => {
    let packets;
    try { packets = reader.feed(data); } catch {
      try { socket.destroy(); } catch {}
      return;
    }
    for (const packet of packets) {
      switch (packet.type) {
        case MessageType.SCREEN:
          terminal.reset();
          terminal.write(packet.payload.toString());
          dirty = true;
          handle.onActivity?.();
          break;
        case MessageType.DATA:
          terminal.write(packet.payload.toString(), () => {
            dirty = true;
            handle.onActivity?.();
          });
          break;
        case MessageType.EXIT:
          exitCode = packet.payload.readInt32BE(0);
          exited = true;
          dirty = true;
          handle.onActivity?.();
          break;
      }
    }
  });

  socket.write(encodeAttach(rows, cols));
  await new Promise(r => setTimeout(r, 100));

  return handle;
}

/**
 * Render an embedded PTY session into the layout. Flex-sized by default.
 * The PTY is automatically resized to match the layout rect.
 *
 * ```
 * hstack({ gap: 1 }, [
 *   column({ width: 30 }, [panel("Sidebar", [...])]),
 *   column({ flex: true }, [ptyView(handle)]),
 * ])
 * ```
 */
export function ptyView(handle: PtyHandle): PtyViewNode {
  return {
    type: "ptyView",
    handle,
    _lastCols: handle.cols,
    _lastRows: handle.rows,
  };
}
