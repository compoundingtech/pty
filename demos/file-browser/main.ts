// Entry point: file browser TUI demo
import { parseKey, CellBuffer, diff, fullRender, effect, hideCursor, showCursor, reset, recordFrame, toggleFPS, isFPSVisible } from "../../src/tui/index.ts";
import { currentTheme, boxStyle, initTree } from "./state.ts";
import { browserScreen } from "./screens/browser.ts";
import type { ScreenContext } from "../../src/tui/index.ts";

const enterAltScreen = "\x1b[?1049h";
const leaveAltScreen = "\x1b[?1049l";

let running = true;
let prevBuffer: CellBuffer | null = null;

function getSize(): [number, number] {
  return [process.stdout.rows ?? 35, process.stdout.columns ?? 120];
}

function cleanup(): void {
  running = false;
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

function renderFrame(): void {
  if (!running) return;
  recordFrame();

  const [rows, cols] = getSize();
  const ctx = createContext(rows, cols);
  const buf = browserScreen.renderToBuffer(ctx);

  let output: string;
  if (prevBuffer && prevBuffer.rows === rows && prevBuffer.cols === cols) {
    output = diff(prevBuffer, buf);
  } else {
    output = fullRender(buf);
  }

  prevBuffer = buf;
  process.stdout.write(hideCursor() + output);
}

// --- Initialize ---
initTree();

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
    // FPS toggle (global)
    if (key.char === "F") {
      toggleFPS();
      continue;
    }

    const [rows, cols] = getSize();
    const ctx = createContext(rows, cols);
    const cont = browserScreen.handleKey(key, ctx);
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
