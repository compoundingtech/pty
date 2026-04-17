// Public API for the declarative TUI framework

// Input
export { parseKey, type KeyEvent } from "./input.ts";

// Signals
export {
  signal, computed, effect, batch,
  debouncedSignal,
  type Signal, type Computed, type DebouncedSignal,
} from "./signals.ts";

// Types
export type { Cell, Screen, ScreenContext } from "./types.ts";
export { emptyCell, cellsEqual } from "./types.ts";

// Colors / rendering utilities
export type { Theme, BoxStyle } from "./colors.ts";
export {
  charWidth, visibleLength, stripAnsi, truncate, wrapText, pad,
  moveTo, fg, bg, reset, bold, dim, italic, underline, inverse,
  BOLD, DIM, RESET,
  clearScreen, hideCursor, showCursor,
  writeAt, fillRect, fillLine,
  drawBox, hSep, boxChars,
  progressBar as progressBarString,
  themes, c, initScreen, titleBar, footerBar,
  panel as drawPanel, panelLine,
  askBar as drawAskBar, askBarCompact,
  agentActivity,
} from "./colors.ts";

// Cell buffer
export { CellBuffer, diff, fullRender } from "./buffer.ts";

// Scrollable
export {
  createScrollRegion, updateScrollRegion,
  scrollUp, scrollDown, pageUp, pageDown,
  scrollToTop, scrollToBottom, visibleSlice,
  type ScrollRegion,
} from "./scrollable.ts";

// Text input
export {
  createTextInput, activateTextInput, deactivateTextInput,
  handleTextInputKey, finishProcessing,
  type TextInputState,
} from "./text-input.ts";

// Node types
export type {
  UINode, Color, SemanticColor, Rect, Span,
  TextNode, SpacerNode, GapNode, SeparatorNode, IndentNode,
  DotNode, CheckboxNode, ProgressBarNode, SpinnerNode, IconNode,
  RowNode, ColumnNode, HStackNode, PanelNode,
  ScrollableNode, SelectableNode,
  StatusBarNode, FooterNode, AskBarNode, TextInputNode,
  FPSCounterNode, CanvasNode, CanvasCell, DrawContext,
  PtyHandle, PtyViewNode,
} from "./nodes.ts";

// Builders
export {
  text, spacer, gap, separator, indent,
  dot, checkbox, progressBar, spinner, icon,
  row, column, hstack, panel,
  scrollable, selectable, groupedSelectable, type SelectableGroup,
  statusBar, footer, askBar, textInput,
  fpsCounter, canvas,
  createPty, attachPty, ptyView, themeToXterm,
} from "./builders.ts";

// Layout
export { layoutRoot, layoutVertical, layoutRow, layoutPanel, textWidth } from "./layout.ts";

// Renderer
export { renderToAnsi, resolveColor, type RenderOpts } from "./renderer.ts";

// Screen wrapper
export { screen, overlay, type DeclarativeScreenConfig, type OverlayConfig } from "./screen.ts";

// Animation
export {
  spinnerChar, startSpinnerTimer, stopSpinnerTimer, isSpinnerRunning,
} from "./animation.ts";

// FPS
export {
  recordFrame, getCurrentFPS, isFPSVisible, toggleFPS,
} from "./fps.ts";

// App lifecycle
export { app, type AppConfig, type App } from "./app.ts";

// Session management
export {
  listSessions, getSession,
  type SessionInfo, type SessionMetadata,
} from "../sessions.ts";

// Daemon spawning
export { spawnDaemon, type SpawnDaemonOptions } from "../spawn.ts";
