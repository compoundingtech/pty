// loggy — wrap a command, capture stdout/stderr separately, live TUI with
// filter/search. See demos/loggy/README-ish notes at the top of the repo's
// changelog when shipped; for now, ./demos/run loggy <command>.

import {
  parseKey, CellBuffer, diff, fullRender, effect, batch,
  hideCursor, showCursor, reset,
  signal, computed, themes,
  type ScreenContext, type Theme,
} from "../../src/tui/index.ts";
import { parseArgs, UsageError, USAGE } from "./cli.ts";
import { createStore, applyView, type FilterMode } from "./store.ts";
import { createSinks } from "./sinks.ts";
import { spawnChild } from "./child.ts";
import { buildLoggyScreen, type ScreenState } from "./screen.ts";

const enterAltScreen = "\x1b[?1049h";
const leaveAltScreen = "\x1b[?1049l";

// ---- Parse argv ------------------------------------------------------------

let parsed;
try {
  parsed = parseArgs(process.argv.slice(2));
} catch (e) {
  const err = e as Error;
  if (err.message === "__help__") {
    process.stdout.write(USAGE);
    process.exit(0);
  }
  if (err instanceof UsageError) {
    process.stderr.write(`loggy: ${err.message}\n\n${USAGE}`);
    process.exit(2);
  }
  throw e;
}

// ---- Signals ---------------------------------------------------------------

const store = createStore(parsed.scrollback);
const filter = signal<FilterMode>("both");
const searchQuery = signal("");
const searchActive = signal(false);
const follow = signal(true);
const selectedIndex = signal(0);
const scrollOffset = signal(0);
const childState = signal<{
  alive: boolean;
  pid: number | undefined;
  exitCode: number | null;
  signal: string | null;
}>({ alive: true, pid: undefined, exitCode: null, signal: null });

/** True between the first and second Ctrl+C (or until the timeout clears
 *  it). Drives the footer prompt and the quit-vs-arm decision. */
const quitPending = signal(false);
const QUIT_WINDOW_MS = 2000;
let quitPendingTimer: ReturnType<typeof setTimeout> | null = null;

function armQuit(): void {
  quitPending.set(true);
  if (quitPendingTimer) clearTimeout(quitPendingTimer);
  quitPendingTimer = setTimeout(() => {
    quitPending.set(false);
    quitPendingTimer = null;
  }, QUIT_WINDOW_MS);
}

function clearQuit(): void {
  if (quitPendingTimer) {
    clearTimeout(quitPendingTimer);
    quitPendingTimer = null;
  }
  if (quitPending.peek()) quitPending.set(false);
}

const visibleLogs = computed(() => applyView(store.entries.get(), filter.get(), searchQuery.get()));

// ---- Sinks + child ---------------------------------------------------------

const sinks = createSinks({ out: parsed.out, err: parsed.err, log: parsed.log });

const child = spawnChild(parsed.command, parsed.args, {
  forceColor: !parsed.noColor,
  onLine: (source, line) => {
    const entry = store.append(source, line);
    sinks.write(source, line, entry.ts);
  },
  onExit: (code, signal) => {
    batch(() => {
      childState.set({
        alive: false,
        pid: child.pid,
        exitCode: code,
        signal,
      });
    });
  },
  onError: (err) => {
    // Surface spawn failures in the log itself so the user sees them.
    store.append("err", `loggy: failed to spawn: ${err.message}`);
    batch(() => {
      childState.set({ alive: false, pid: undefined, exitCode: 127, signal: null });
    });
  },
});

// Once we have a PID, update the status line.
childState.set({ alive: true, pid: child.pid, exitCode: null, signal: null });

// ---- Screen + render loop --------------------------------------------------

let running = true;
let prevBuffer: CellBuffer | null = null;

function currentTheme(): Theme {
  // Match other demos: pick the default terminal theme so colors follow
  // the user's actual terminal palette.
  return themes.terminal ?? themes.coolBlue ?? Object.values(themes)[0];
}

function getSize(): [number, number] {
  return [process.stdout.rows ?? 35, process.stdout.columns ?? 120];
}

function createContext(rows: number, cols: number): ScreenContext {
  return {
    rows, cols,
    theme: currentTheme(),
    boxStyle: "rounded",
    navigate: () => {},
    back: () => {},
    openOverlay: () => {},
    closeOverlay: () => {},
    isTextInputActive: () => searchActive.get(),
    setTextInputActive: (v) => searchActive.set(v),
  };
}

let quitting = false;
async function cleanup(): Promise<void> {
  if (!running) return;
  running = false;
  // Restore terminal first so any subsequent messages aren't eaten by the
  // alt-screen buffer.
  process.stdout.write(showCursor() + reset() + leaveAltScreen);
  if (process.stdin.isTTY) {
    try { process.stdin.setRawMode(false); } catch {}
  }
  process.stdin.pause();
  await child.stop(2000);
  await sinks.close();
}

async function quit(): Promise<void> {
  if (quitting) return;
  quitting = true;
  await cleanup();
  process.exit(childState.peek().exitCode ?? 0);
}

const state: ScreenState = {
  commandDisplay: [parsed.command, ...parsed.args].join(" "),
  visibleLogs,
  filter, searchQuery, searchActive, follow,
  selectedIndex, scrollOffset,
  childState,
  quitPending,
  onCtrlC: () => {
    if (quitPending.peek()) {
      // Second press within the window — actually quit.
      clearQuit();
      void quit();
    } else {
      armQuit();
    }
  },
};

const screen = buildLoggyScreen(state);

function renderFrame(): void {
  if (!running) return;
  const [rows, cols] = getSize();
  const ctx = createContext(rows, cols);
  const buf = screen.renderToBuffer(ctx);

  let output: string;
  if (prevBuffer && prevBuffer.rows === rows && prevBuffer.cols === cols) {
    output = diff(prevBuffer, buf);
  } else {
    output = fullRender(buf);
  }
  prevBuffer = buf;
  process.stdout.write(hideCursor() + output);
}

// ---- Terminal setup --------------------------------------------------------

process.stdout.write(enterAltScreen + hideCursor());
if (process.stdin.isTTY) {
  process.stdin.setRawMode(true);
}
process.stdin.resume();

// Reactive render loop. Deps are auto-registered by reads inside
// renderFrame. The store's `version` is a debouncedSignal so a
// firehose of appends coalesces to at most one notification per tick
// — the effect naturally runs at most once per tick without any
// render-side throttling.
effect(() => { renderFrame(); });

process.stdin.on("data", (data: Buffer | string) => {
  const buf = typeof data === "string" ? Buffer.from(data) : data;
  const keys = parseKey(buf);
  for (const key of keys) {
    // Any non-Ctrl+C key cancels a pending quit. Ctrl+C itself is
    // handled by the screen (which will either arm or confirm).
    if (quitPending.peek() && !(key.name === "c" && key.ctrl)) {
      clearQuit();
    }
    const [rows, cols] = getSize();
    const ctx = createContext(rows, cols);
    screen.handleKey(key, ctx);
  }
});

process.stdout.on("resize", () => {
  prevBuffer = null;
  renderFrame();
});

// In raw mode, Ctrl+C arrives as a 0x03 byte via stdin (handled by the
// screen's double-press logic). We still register SIGINT as a safety net
// for cases where raw mode isn't active (e.g., piped stdin) — those
// contexts don't get the double-press UX, but quitting is the right thing.
process.on("SIGINT", () => { void quit(); });
process.on("SIGTERM", () => { void quit(); });
