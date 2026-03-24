// Entry point: reminders TUI demo
import {
  parseKey, CellBuffer, diff, fullRender, effect,
  hideCursor, showCursor, reset, recordFrame,
  toggleFPS,
} from "../../src/tui/index.ts";
import type { ScreenContext } from "../../src/tui/index.ts";
import { currentTheme, boxStyle, init, stopWatching, currentView } from "./state.ts";
import { handleGlobalKey, activeOverlay } from "./router.ts";
import { renderListView } from "./screens/list-view.ts";
import { renderBoardView } from "./screens/board-view.ts";
import { renderCalendarView } from "./screens/calendar-view.ts";
import { newReminderOverlay } from "./screens/new-reminder.ts";
import { confirmDeleteOverlay } from "./screens/confirm-delete.ts";
import {
  screen, layoutRoot, renderToAnsi,
  type RenderOpts,
} from "../../src/tui/index.ts";

const enterAltScreen = "\x1b[?1049h";
const leaveAltScreen = "\x1b[?1049l";

let running = true;
let prevBuffer: CellBuffer | null = null;

function getSize(): [number, number] {
  return [process.stdout.rows ?? 35, process.stdout.columns ?? 120];
}

function cleanup(): void {
  running = false;
  stopWatching();
  process.stdout.write(showCursor() + reset() + leaveAltScreen);
  if (process.stdin.isTTY) {
    process.stdin.setRawMode(false);
  }
  process.stdin.pause();
}

function createContext(rows: number, cols: number): ScreenContext {
  return {
    rows,
    cols,
    theme: currentTheme.get(),
    boxStyle: boxStyle.get(),
    navigate: () => {},
    back: () => {},
    openOverlay: () => {},
    closeOverlay: () => {},
    isTextInputActive: () => false,
    setTextInputActive: () => {},
  };
}

// Create a dynamic screen that renders the active view
const appScreen = screen({
  id: "reminders",
  render(ctx: ScreenContext) {
    const view = currentView.get();
    if (view === "board") return renderBoardView(ctx);
    if (view === "calendar") return renderCalendarView(ctx);
    return renderListView(ctx);
  },
});

function renderFrame(): void {
  if (!running) return;
  recordFrame();

  const [rows, cols] = getSize();
  const ctx = createContext(rows, cols);

  // Render main screen
  const buf = appScreen.renderToBuffer(ctx);

  // Composite overlay if open
  const overlay = activeOverlay.get();
  if (overlay === "new" || overlay === "edit") {
    const overlayBuf = newReminderOverlay.renderToBuffer(ctx);
    compositeOverlay(buf, overlayBuf);
  } else if (overlay === "confirm-delete") {
    const overlayBuf = confirmDeleteOverlay.renderToBuffer(ctx);
    compositeOverlay(buf, overlayBuf);
  }

  let output: string;
  if (prevBuffer && prevBuffer.rows === rows && prevBuffer.cols === cols) {
    output = diff(prevBuffer, buf);
  } else {
    output = fullRender(buf);
  }

  prevBuffer = buf;
  process.stdout.write(hideCursor() + output);
}

function compositeOverlay(base: CellBuffer, overlay: CellBuffer): void {
  // First pass: find the bounding box of non-empty cells in the overlay
  let minR = base.rows, maxR = 0, minC = base.cols, maxC = 0;
  for (let r = 0; r < base.rows; r++) {
    for (let c = 0; c < base.cols; c++) {
      const cell = overlay.cells[r]?.[c];
      if (cell && (cell.char !== " " || cell.bg !== null)) {
        minR = Math.min(minR, r);
        maxR = Math.max(maxR, r);
        minC = Math.min(minC, c);
        maxC = Math.max(maxC, c);
      }
    }
  }

  // Second pass: copy ALL cells within the bounding box (including spaces)
  for (let r = minR; r <= maxR; r++) {
    for (let c = minC; c <= maxC; c++) {
      const cell = overlay.cells[r]?.[c];
      if (cell) {
        base.cells[r][c] = cell;
      }
    }
  }
}

// --- Initialize ---
init();

// --- Terminal setup ---
process.stdout.write(enterAltScreen + hideCursor());
if (process.stdin.isTTY) {
  process.stdin.setRawMode(true);
}
process.stdin.resume();

// Reactive render loop
effect(() => {
  renderFrame();
});

// Handle stdin
process.stdin.on("data", (data: Buffer | string) => {
  const buf = typeof data === "string" ? Buffer.from(data) : data;
  const keys = parseKey(buf);

  for (const key of keys) {
    if (key.char === "F") {
      toggleFPS();
      continue;
    }

    const [rows, cols] = getSize();
    const ctx = createContext(rows, cols);
    const cont = handleGlobalKey(key, ctx);
    if (!cont) {
      cleanup();
      process.exit(0);
    }
  }
});

// Handle resize
process.stdout.on("resize", () => {
  prevBuffer = null;
  renderFrame();
});

// Handle cleanup
process.on("SIGINT", () => { cleanup(); process.exit(0); });
process.on("SIGTERM", () => { cleanup(); process.exit(0); });
process.on("exit", cleanup);
