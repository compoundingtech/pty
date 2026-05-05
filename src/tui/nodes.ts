// UINode type definitions for the declarative UI framework

import type { BoxStyle } from "./colors.ts";

// --- Color types ---
export type SemanticColor =
  | "ok" | "muted" | "error" | "accent"
  | "primary" | "secondary" | "warn" | "info" | "border";

export type Color = SemanticColor | [number, number, number];

// --- Layout rect (0-based coordinates) ---
export interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

// --- Highlight spans ---

/** A colored span within a text node, used by the highlight callback. */
export interface Span {
  /** Start character index (inclusive, code-point based). */
  start: number;
  /** End character index (exclusive, code-point based). */
  end: number;
  color?: Color;
  bold?: boolean;
  dim?: boolean;
  italic?: boolean;
}

// --- Node definitions ---

export interface TextNode {
  type: "text";
  text: string;
  color?: Color;
  /** Explicit background color. Default is the ambient panel/screen bg. */
  background?: Color;
  bold?: boolean;
  dim?: boolean;
  italic?: boolean;
  /** Swap fg/bg for the rendered cells. Used for text cursors (paint the
   *  character UNDER the cursor with colors swapped, so the glyph doesn't
   *  push neighbors sideways). */
  inverse?: boolean;
  truncate?: boolean;
  /** Soft-wrap text to fit the available width. Expands height as needed. */
  wrap?: boolean;
  /** Highlight callback: receives the full text, returns colored spans.
   *  Applied before wrapping — the framework splits spans at wrap boundaries. */
  highlight?: (text: string) => Span[];
  _rect?: Rect;
}

export interface SpacerNode {
  type: "spacer";
  _rect?: Rect;
}

export interface GapNode {
  type: "gap";
  size: number | "center";
  _rect?: Rect;
}

export interface SeparatorNode {
  type: "separator";
  _rect?: Rect;
}

export interface IndentNode {
  type: "indent";
  depth: number;
  _rect?: Rect;
}

export interface DotNode {
  type: "dot";
  filled: boolean;
  color?: Color;
  _rect?: Rect;
}

export interface CheckboxNode {
  type: "checkbox";
  checked: boolean;
  color?: Color;
  _rect?: Rect;
}

export interface ProgressBarNode {
  type: "progressBar";
  percent: number;
  width?: number;
  color?: Color;
  _rect?: Rect;
}

export interface SpinnerNode {
  type: "spinner";
  color?: Color;
  _rect?: Rect;
}

export interface IconNode {
  type: "icon";
  char: string;
  color?: Color;
  _rect?: Rect;
}

export interface RowNode {
  type: "row";
  children: UINode[];
  _rect?: Rect;
}

export interface ColumnNode {
  type: "column";
  children: UINode[];
  width?: number;
  flex?: boolean;
  _rect?: Rect;
}

export interface HStackNode {
  type: "hstack";
  children: ColumnNode[];
  gap?: number;
  _rect?: Rect;
}

export interface PanelNode {
  type: "panel";
  title: string;
  /** Optional caption rendered on the bottom border (like mactop's
   *  "4/17 layout (skyblue)" strip). Same style as the top title. */
  footerTitle?: string;
  children: UINode[];
  style?: BoxStyle;
  _rect?: Rect;
}

export interface ScrollableNode {
  type: "scrollable";
  items: UINode[][];
  offset: number;
  totalItems: number;
  _rect?: Rect;
}

export interface SelectableNode {
  type: "selectable";
  items: UINode[][];
  selectedIndex: number;
  offset: number;
  totalItems: number;
  _rect?: Rect;
}

export interface StatusBarNode {
  type: "statusBar";
  left: string;
  right: string;
  _rect?: Rect;
}

export interface FooterNode {
  type: "footer";
  /** Left-aligned text. For a single-column footer, pass only this. */
  hints: string;
  /** Optional right-aligned text. When set, the footer renders two
   *  columns — hints on the left, this on the right — padded to fill
   *  the viewport width. */
  right?: string;
  _rect?: Rect;
}

export interface AskBarNode {
  type: "askBar";
  text: string;
  placeholder: string;
  active: boolean;
  rightLabel?: string;
  style?: BoxStyle;
  _rect?: Rect;
}

export interface TextInputNode {
  type: "textInput";
  text: string;
  cursor: number;
  active: boolean;
  placeholder?: string;
  _rect?: Rect;
}

export interface FPSCounterNode {
  type: "fpsCounter";
  _rect?: Rect;
}

/** A cell written by a canvas draw callback. */
export interface CanvasCell {
  x: number;
  y: number;
  char: string;
  color?: Color;
  bg?: Color;
  bold?: boolean;
  dim?: boolean;
}

/** Drawing context passed to the canvas draw callback. */
export interface DrawContext {
  /** Canvas width in columns. */
  width: number;
  /** Canvas height in rows. */
  height: number;
  /** Place a single character at (x, y) relative to the canvas origin. */
  set(x: number, y: number, char: string, color?: Color, bg?: Color, bold?: boolean): void;
  /** Write a string starting at (x, y). Flows left-to-right, no wrapping. */
  write(x: number, y: number, str: string, color?: Color, bg?: Color, bold?: boolean): void;
  /** Fill a rectangle with a character. */
  fill(x: number, y: number, w: number, h: number, char?: string, color?: Color, bg?: Color): void;
}

export interface CanvasNode {
  type: "canvas";
  /** Fixed height if set. Otherwise flex (fills available space). */
  height?: number;
  /** Fixed width if set. Otherwise flex (fills available space in a row). */
  widthHint?: number;
  /** Draw callback — called during rendering with the resolved rect dimensions. */
  draw: (ctx: DrawContext) => void;
  /** Cells produced by the draw callback. Populated during rendering. */
  _cells: CanvasCell[];
  _rect?: Rect;
}

/** One grid cell as read by `PtyHandle.readCells`. Keeps both the
 *  resolved RGB and — when the source was palette-indexed — the raw
 *  0-255 index. Re-emitters that want the outer terminal's theme
 *  (kitty, wezterm, iterm2 with a user palette) should prefer the
 *  index; consumers that need a concrete RGB fall back to `fg`/`bg`. */
export interface PtyCell {
  char: string;
  /** RGB foreground. `null` when the cell used the terminal's default
   *  fg and no explicit color was ever set. For palette cells this is
   *  populated with a flattened VGA/xterm-cube approximation — check
   *  `fgIndex` first if you want the outer terminal's theme. */
  fg: [number, number, number] | null;
  bg: [number, number, number] | null;
  /** 0-255 palette index when the cell was written with an indexed
   *  SGR color (30-37, 90-97, 38;5;N, 48;5;N). `null` for default-
   *  color cells and for true-color cells. Added so pty-layout and
   *  similar re-emitters can round-trip indexed colors and let the
   *  outer terminal resolve them against its own theme. */
  fgIndex: number | null;
  bgIndex: number | null;
  bold: boolean;
  dim: boolean;
  italic: boolean;
  underline: boolean;
}

/** Handle returned by createPty(). Holds the spawned process + xterm terminal. */
export interface PtyHandle {
  /** Write raw input to the child process. */
  write(data: string): void;
  /** Resize the child PTY. Called automatically by the layout engine. */
  resize(cols: number, rows: number): void;
  /**
   * Read the terminal's cell grid directly. Full fidelity — every attribute
   * xterm parsed is preserved. No serialize round-trip.
   * @param scrollOffset Lines to scroll back into history (0 = live viewport).
   */
  readCells(scrollOffset?: number): PtyCell[][];
  /**
   * Read per-row "wrapped" flags aligned with the rows returned by
   * `readCells(scrollOffset)`. A `true` at index `r` means row `r`
   * continues from row `r-1` because the terminal wrapped a long line
   * rather than the child emitting a real newline — the same signal
   * xterm.js exposes via `IBufferLine.isWrapped`.
   *
   * Intended use: reconstructing logical lines from a multi-row text
   * selection (e.g., copying a wrapped URL to the clipboard without a
   * spurious `\n` in the middle).
   * @param scrollOffset Lines to scroll back into history (0 = live viewport).
   */
  readWrappedFlags(scrollOffset?: number): boolean[];
  /** Current PTY dimensions. */
  cols: number;
  rows: number;
  /** Kill the child process / detach from server. */
  kill(): void;
  /** True after the child has exited or been detached. */
  exited: boolean;
  /** True when the PTY has received new data since the last render. Cleared by the consumer. */
  dirty: boolean;
  /** Optional callback fired when the PTY receives data. Still supported as
   *  an escape hatch for consumers that want to react to activity beyond
   *  re-rendering — but you no longer need to wire it up just to get the
   *  framework to re-render. Reading `rev.get()` during render (which
   *  `ptyView()` does automatically) subscribes the reactive effect. */
  onActivity: (() => void) | null;
  /** Reactive revision counter. Bumps on every child-process data arrival,
   *  exit, resize, and theme change. `ptyView()` reads this during render
   *  so the framework's `effect()` re-renders automatically whenever the
   *  terminal content changes. Consumers shouldn't write to this. */
  rev: import("./signals.ts").Signal<number>;
  /** Update the xterm terminal's color theme. Call when the app theme changes. */
  setTheme(theme: import("./colors.ts").Theme): void;
  /** Cursor row position (0-indexed, relative to viewport). */
  readonly cursorRow: number;
  /** Cursor column position (0-indexed). */
  readonly cursorCol: number;
  /** Whether the child process has enabled mouse tracking (modes 1000/1002/1003). */
  readonly mouseMode: boolean;
  /** Whether the child is using the alternate screen buffer (mode 1049). */
  readonly alternateScreen: boolean;
  /** Stack of kitty keyboard protocol flags pushed by the child (CSI > N u
   *  pushes, CSI < u pops). Empty array means the protocol is not active.
   *  Returned as a defensive copy — mutating it has no effect on the PTY. */
  readonly kittyKeyboardFlags: number[];
  /** Whether the child has enabled bracketed paste mode (CSI ? 2004 h).
   *  When true, the child wraps pasted input in `\x1b[200~ ... \x1b[201~`
   *  markers and expects them on stdin; consumers proxying paste should
   *  forward the markers verbatim only to children that have it on. */
  readonly bracketedPasteMode: boolean;
  /** Configured scrollback line count. */
  readonly scrollback: number;
  /** Total lines in the buffer (viewport + scrollback history). */
  readonly bufferLength: number;
  /** Line index at the top of the live viewport. */
  readonly baseY: number;
}

export interface PtyViewNode {
  type: "ptyView";
  handle: PtyHandle;
  /** Last size the PTY was resized to, tracked to avoid redundant resize calls. */
  _lastCols: number;
  _lastRows: number;
  _rect?: Rect;
}

// --- Union type ---
export type UINode =
  | TextNode
  | SpacerNode
  | GapNode
  | SeparatorNode
  | IndentNode
  | DotNode
  | CheckboxNode
  | ProgressBarNode
  | SpinnerNode
  | IconNode
  | RowNode
  | ColumnNode
  | HStackNode
  | PanelNode
  | ScrollableNode
  | SelectableNode
  | StatusBarNode
  | FooterNode
  | AskBarNode
  | TextInputNode
  | FPSCounterNode
  | CanvasNode
  | PtyViewNode;
