// App lifecycle wrapper: handles alt screen, raw mode, render loop, stdin, cleanup.
// Supports pause/resume for handing the terminal to another process (e.g. attach).
import type { Screen, ScreenContext } from "./types.ts";
import type { Theme, BoxStyle } from "./colors.ts";
import type { KeyEvent, MouseEvent, InputEvent } from "./input.ts";
import { parseInput, isMouseEvent, MOUSE_ENABLE_SGR, MOUSE_DISABLE_SGR } from "./input.ts";
import { createFocusManager, type FocusManager } from "./focus.ts";
import { hideCursor, showCursor, reset } from "./colors.ts";
import { CellBuffer, diff, fullRender } from "./buffer.ts";
import { recordFrame, getCurrentFPS, isFPSVisible } from "./fps.ts";
import { themes } from "./colors.ts";
import { effect } from "./signals.ts";

const enterAltScreen = "\x1b[?1049h";
const leaveAltScreen = "\x1b[?1049l";

/** Configuration for an app created with `app()`. */
export interface AppConfig {
  /** The screen to render. A function is called each frame (reads signals → auto-rerenders). */
  screen: Screen | (() => Screen);
  /** Optional overlay rendered on top of the main screen. */
  overlay?: () => Screen | null;
  /** Called before the screen's handleKey. Return true = key consumed, false = pass to screen. */
  onKey?: (key: KeyEvent) => boolean;
  /** Called before the screen's handleMouse. Return true = consumed. */
  onMouse?: (event: MouseEvent) => boolean;
  /** Theme provider. Defaults to coolBlue. */
  theme?: () => Theme;
  /** Box style provider. Defaults to "rounded". */
  boxStyle?: () => BoxStyle;
  /** Enable SGR mouse reporting on start (CSI ?1002h + ?1006h) and disable
   *  on stop. Default: false. Enable this when your app wants to receive
   *  mouse events via screen.handleMouse / onMouse. */
  mouse?: boolean;
  /** Shared focus manager. If omitted, `app()` creates one automatically
   *  and exposes it as `ctx.focus` on every screen render/handleKey call.
   *  Provide your own to share focus state across app instances (rare). */
  focus?: FocusManager;
}

/** A running TUI app with lifecycle control. */
export interface App {
  /** Enter alt screen, raw mode, start render loop and input handling. */
  start(): void;
  /** Clean exit: restore terminal, dispose everything. */
  stop(): void;
  /** Hand terminal to another process. Stops rendering and releases stdin. */
  pause(): void;
  /** Take terminal back after pause. Restores rendering and input. */
  resume(): void;
}

export function app(config: AppConfig): App {
  const stdin = process.stdin;
  const stdout = process.stdout;

  let running = false;
  let prevBuffer: CellBuffer | null = null;
  let effectDispose: (() => void) | null = null;
  let stdinHandler: ((data: Buffer | string) => void) | null = null;
  let resizeHandler: (() => void) | null = null;
  let sigintHandler: (() => void) | null = null;
  let sigtermHandler: (() => void) | null = null;
  let exitHandler: (() => void) | null = null;
  let activeScreen: Screen | null = null;
  let activeOverlay: Screen | null = null;
  const focus: FocusManager = config.focus ?? createFocusManager();

  function getSize(): [number, number] {
    return [(stdout as any).rows ?? 35, (stdout as any).columns ?? 120];
  }

  function getTheme(): Theme {
    return config.theme ? config.theme() : themes.coolBlue as Theme;
  }

  function getBoxStyle(): BoxStyle {
    return config.boxStyle ? config.boxStyle() : "rounded";
  }

  function resolveScreen(): Screen {
    return typeof config.screen === "function" ? config.screen() : config.screen;
  }

  function createContext(rows: number, cols: number): ScreenContext {
    return {
      rows,
      cols,
      theme: getTheme(),
      boxStyle: getBoxStyle(),
      navigate: () => {},
      back: () => {},
      openOverlay: () => {},
      closeOverlay: () => {},
      isTextInputActive: () => false,
      setTextInputActive: () => {},
      quit: () => {
        self.stop();
        process.exit(0);
      },
      focus,
    };
  }

  function renderFrame(): void {
    if (!running) return;
    recordFrame();

    const [rows, cols] = getSize();
    const ctx = createContext(rows, cols);
    const scr = resolveScreen();

    // Detect screen transition
    if (scr !== activeScreen) {
      activeScreen?.onLeave?.(ctx);
      activeScreen = scr;
      scr.onEnter?.(ctx);
    }

    const buf = scr.renderToBuffer(ctx);

    // Composite overlay if present
    if (config.overlay) {
      const ov = config.overlay() ?? null;
      // Detect overlay transition
      if (ov !== activeOverlay) {
        activeOverlay?.onLeave?.(ctx);
        activeOverlay = ov;
        ov?.onEnter?.(ctx);
      }
      if (ov) {
        const overlayBuf = ov.renderToBuffer(ctx);
        // Bounding-box composite: find non-empty region, copy all cells within
        let minR = buf.rows, maxR = 0, minC = buf.cols, maxC = 0;
        for (let r = 0; r < buf.rows; r++) {
          for (let c = 0; c < buf.cols; c++) {
            const cell = overlayBuf.cells[r]?.[c];
            if (cell && (cell.char !== " " || cell.bg !== null)) {
              minR = Math.min(minR, r);
              maxR = Math.max(maxR, r);
              minC = Math.min(minC, c);
              maxC = Math.max(maxC, c);
            }
          }
        }
        for (let r = minR; r <= maxR; r++) {
          for (let c = minC; c <= maxC; c++) {
            const cell = overlayBuf.cells[r]?.[c];
            if (cell) buf.cells[r][c] = cell;
          }
        }
      }
    }

    // FPS overlay (top-right corner)
    if (isFPSVisible()) {
      const fps = getCurrentFPS();
      const theme = getTheme();
      const label = ` ${fps} FPS `;
      const col = (getSize()[1]) - label.length - 1;
      for (let i = 0; i < label.length; i++) {
        if (col + i >= 0 && col + i < buf.cols) {
          buf.cells[0][col + i] = {
            char: label[i], fg: theme.bg1 ? [...theme.bg1] : null, bg: theme.fgAc ? [...theme.fgAc] : null,
            fgIndex: null, bgIndex: null,
            bold: false, dim: false, italic: false, underline: false,
          };
        }
      }
    }

    let output: string;
    const [rows2, cols2] = getSize();
    if (prevBuffer && prevBuffer.rows === rows2 && prevBuffer.cols === cols2) {
      output = diff(prevBuffer, buf);
    } else {
      output = fullRender(buf);
    }

    prevBuffer = buf;
    stdout.write(hideCursor() + output);
  }

  function registerListeners(): void {
    stdinHandler = (data: Buffer | string) => {
      const buf = typeof data === "string" ? Buffer.from(data) : data;
      const events = parseInput(buf);
      for (const event of events) {
        if (isMouseEvent(event)) {
          // Global mouse interceptor.
          if (config.onMouse && config.onMouse(event)) continue;
          const [rows, cols] = getSize();
          const ctx = createContext(rows, cols);
          const scr = resolveScreen();
          scr.handleMouse?.(event, ctx);
          continue;
        }
        const key = event;

        // Global key interceptor — if onKey returns true, the screen never
        // sees the event (used for global shortcuts like command palette).
        if (config.onKey && config.onKey(key)) continue;

        // Default ctrl+c handler: quits unless the global onKey already
        // consumed it above. Screens that want to override this (e.g. a
        // composer with double-ctrl-c to confirm) can intercept via
        // config.onKey.
        if (key.name === "c" && key.ctrl) {
          self.stop();
          process.exit(130);
        }

        // Screen key handler. Return value is a hint for nested routing;
        // the app ignores it. Screens that want to quit call ctx.quit()
        // explicitly — returning false from here used to quit the app
        // and was a footgun.
        const [rows, cols] = getSize();
        const ctx = createContext(rows, cols);
        const scr = resolveScreen();
        scr.handleKey(key, ctx);
      }
    };
    stdin.on("data", stdinHandler);

    resizeHandler = () => {
      prevBuffer = null;
      renderFrame();
    };
    stdout.on("resize", resizeHandler);

    sigintHandler = () => { self.stop(); process.exit(0); };
    sigtermHandler = () => { self.stop(); process.exit(0); };
    exitHandler = () => { self.stop(); };
    process.on("SIGINT", sigintHandler);
    process.on("SIGTERM", sigtermHandler);
    process.on("exit", exitHandler);
  }

  function removeListeners(): void {
    if (stdinHandler) { stdin.removeListener("data", stdinHandler); stdinHandler = null; }
    if (resizeHandler) { stdout.removeListener("resize", resizeHandler); resizeHandler = null; }
    if (sigintHandler) { process.removeListener("SIGINT", sigintHandler); sigintHandler = null; }
    if (sigtermHandler) { process.removeListener("SIGTERM", sigtermHandler); sigtermHandler = null; }
    if (exitHandler) { process.removeListener("exit", exitHandler); exitHandler = null; }
  }

  function enterTerminal(): void {
    stdout.write(enterAltScreen + hideCursor());
    if (config.mouse) stdout.write(MOUSE_ENABLE_SGR);
    if (stdin.isTTY) stdin.setRawMode(true);
    stdin.resume();
  }

  function leaveTerminal(full: boolean): void {
    if (config.mouse) stdout.write(MOUSE_DISABLE_SGR);
    if (full) {
      stdout.write(showCursor() + reset() + leaveAltScreen);
    } else {
      stdout.write(showCursor() + leaveAltScreen);
    }
    if (stdin.isTTY && stdin.isRaw) stdin.setRawMode(false);
    stdin.pause();
  }

  const self: App = {
    start() {
      running = true;
      prevBuffer = null;
      enterTerminal();
      registerListeners();
      effectDispose = effect(() => { renderFrame(); });
    },

    stop() {
      if (!running) return;
      running = false;
      if (effectDispose) { effectDispose(); effectDispose = null; }
      removeListeners();
      // Call onLeave for active overlay and screen
      const ctx = createContext(...getSize());
      activeOverlay?.onLeave?.(ctx);
      activeScreen?.onLeave?.(ctx);
      activeOverlay = null;
      activeScreen = null;
      leaveTerminal(true);
    },

    pause() {
      if (!running) return;
      running = false;
      if (effectDispose) { effectDispose(); effectDispose = null; }
      removeListeners();
      leaveTerminal(false);
    },

    resume() {
      running = true;
      prevBuffer = null;
      enterTerminal();
      registerListeners();
      effectDispose = effect(() => { renderFrame(); });
    },
  };

  return self;
}
