// The single loggy TUI screen.
//
// Layout:
//
//   ╭─ loggy — <command> ─────────────────────────────╮
//   │  ● out  line one                                │
//   │  ● err  an error here                           │
//   │  ...                                            │
//   ╰─────────────────────────────────────────────────╯
//    [b]oth [o]ut [e]rr  /search  [f]ollow  q quit  ● running (pid)

import {
  screen, text, row, panel, selectable, footer, canvas,
  updateScrollRegion,
  type ScreenContext, type UINode, type KeyEvent,
} from "../../src/tui/index.ts";
import type { LogEntry, FilterMode } from "./store.ts";
import type { Signal } from "../../src/tui/index.ts";

/** Narrow read-only interface compatible with both Signal<T> and
 *  computed<T>. screen.ts only reads the visible logs via `.get()`;
 *  ownership of the underlying reactive state stays with main.ts. */
export interface ReadableSignal<T> {
  get(): T;
}

export interface ScreenState {
  commandDisplay: string;
  /** Derived view of log entries after filter + search. */
  visibleLogs: ReadableSignal<LogEntry[]>;
  filter: Signal<FilterMode>;
  searchQuery: Signal<string>;
  searchActive: Signal<boolean>;
  follow: Signal<boolean>;
  selectedIndex: Signal<number>;
  scrollOffset: Signal<number>;
  childState: Signal<{
    alive: boolean;
    pid: number | undefined;
    exitCode: number | null;
    signal: string | null;
  }>;
  /** Set by main.ts when the first Ctrl+C lands; cleared by main.ts after
   *  a short window. Screen reads this to prompt the user to confirm. */
  quitPending: ReadableSignal<boolean>;
  /** Called on every Ctrl+C. main.ts handles the first-vs-second logic
   *  and the teardown timer. */
  onCtrlC: () => void;
}

export function buildLoggyScreen(state: ScreenState) {
  return screen({
    id: "loggy",

    render(ctx: ScreenContext): UINode[] {
      const entries = state.visibleLogs.get();
      const viewport = Math.max(1, ctx.rows - 4); // panel top + bottom + footer

      // If follow mode is on, always position the viewport at the bottom
      // regardless of the stored selectedIndex. We do NOT write to the
      // signal from inside render — that causes cascading re-renders and
      // stale-state bugs. Keeping this read-only keeps the render
      // deterministic; manual scrolling flips follow off in handleKey.
      const follow = state.follow.get();
      const last = Math.max(0, entries.length - 1);
      const effectiveSelected = follow
        ? last
        : Math.min(state.selectedIndex.get(), last);

      const region = updateScrollRegion(
        {
          offset: state.scrollOffset.get(),
          selectedIndex: effectiveSelected,
          totalItems: entries.length,
          viewportHeight: viewport,
        },
        entries.length,
        viewport,
      );

      const title = "loggy — " + state.commandDisplay;

      // When entries is empty we still want the panel at full height —
      // otherwise layoutPanel shrinks to fit the fixed-height text node
      // and the panel collapses to one row. Pair the text with a flex
      // spacer so the panel stays the same size regardless of state.
      const panelBody = entries.length === 0
        ? [
            text("  (waiting for matching output…)", "muted", { dim: true }),
            canvas(() => {}, {}), // fills remaining height
          ]
        : [
            // selectable is already flex — DO NOT add a sibling flex
            // spacer here, or the panel's flex budget will be split and
            // only half the viewport will show log lines.
            selectable(region, entries, (entry, _i, _selected) =>
              renderLogLine(entry, follow)),
          ];

      return [
        panel(title, panelBody),
        renderFooter(state),
      ];
    },

    handleKey(key: KeyEvent, ctx: ScreenContext): boolean {
      return handleKey(state, key, ctx);
    },
  });
}

function renderLogLine(entry: LogEntry, _followOn: boolean): UINode[] {
  // " ● out " or " ● err " prefix then the content. stderr lines render
  // bold + error color; stdout keep their original colors (ANSI passes
  // through via the text builder).
  if (entry.source === "err") {
    return [
      row(
        text("  ● err  ", "error", { bold: true }),
        text(entry.line, "error", { truncate: true }),
      ),
    ];
  }
  return [
    row(
      text("  ● out  ", "muted", { dim: true }),
      text(entry.line, "primary", { truncate: true }),
    ),
  ];
}

function renderFooter(state: ScreenState): UINode {
  const filter = state.filter.get();
  const searchActive = state.searchActive.get();
  const query = state.searchQuery.get();
  const follow = state.follow.get();
  const child = state.childState.get();

  const quitPending = state.quitPending.get();

  const leftText = quitPending
    ? "Press Ctrl+C again to quit (or any other key to cancel)"
    : searchActive
    ? `/ ${query}\u2588`
    : `${marker("b", filter === "both")}oth  ` +
      `${marker("o", filter === "out")}ut  ` +
      `${marker("e", filter === "err")}rr  ` +
      (query ? `search="${query}"  ` : "/search  ") +
      `${marker("f", follow)}ollow  ctrl+c\u00d72 quit`;

  const statusText = child.alive
    ? `\u25cf running (${child.pid ?? "?"})`
    : child.signal
    ? `\u2717 signaled (${child.signal})`
    : `\u25cb exited (code ${child.exitCode ?? "?"})`;

  // Two-column bottom bar: keybindings / search prompt on the left,
  // child process status right-aligned. Anchored to the bottom by the
  // framework's root layout.
  return footer(leftText, statusText);
}

function marker(letter: string, active: boolean): string {
  return active ? `[${letter.toUpperCase()}]` : `[${letter}]`;
}

function handleKey(state: ScreenState, key: KeyEvent, ctx: ScreenContext): boolean {
  // Ctrl+C takes two consecutive presses to quit. main.ts handles the
  // first-vs-second state + timeout; we just tell it one happened. Any
  // OTHER key press cancels the pending quit before falling through.
  if (key.name === "c" && key.ctrl) {
    state.onCtrlC();
    return true;
  }

  // --- Search mode captures most keys ---
  if (state.searchActive.peek()) {
    if (key.name === "escape") {
      // Esc clears the search AND exits search mode
      state.searchActive.set(false);
      state.searchQuery.set("");
      return true;
    }
    if (key.name === "return") {
      // Return commits the query and exits search-input mode; query stays
      state.searchActive.set(false);
      return true;
    }
    if (key.name === "backspace") {
      const q = state.searchQuery.peek();
      if (q.length > 0) state.searchQuery.set(q.slice(0, -1));
      return true;
    }
    if (key.char && !key.ctrl && !key.alt) {
      state.searchQuery.set(state.searchQuery.peek() + key.char);
      return true;
    }
    return true; // swallow other keys in search mode
  }

  // --- Global keys (not in search mode) ---
  switch (key.name) {
    case "o":
      state.filter.set("out");
      resetScroll(state);
      return true;

    case "e":
      state.filter.set("err");
      resetScroll(state);
      return true;

    case "b":
      state.filter.set("both");
      resetScroll(state);
      return true;

    case "f":
      state.follow.set(!state.follow.peek());
      return true;

    case "/":
      state.searchActive.set(true);
      return true;

    case "escape":
      // Outside search mode: clear any active search query.
      if (state.searchQuery.peek()) {
        state.searchQuery.set("");
        return true;
      }
      return true;

    case "up":
      manualScroll(state, -1);
      return true;

    case "down":
      manualScroll(state, 1);
      return true;

    case "pageup":
      manualScroll(state, -Math.max(1, ctx.rows - 6));
      return true;

    case "pagedown":
      manualScroll(state, Math.max(1, ctx.rows - 6));
      return true;

    case "g": {
      // lowercase g → top, uppercase G → bottom + follow
      if (key.char === "G") {
        state.selectedIndex.set(Math.max(0, state.visibleLogs.get().length - 1));
        state.follow.set(true);
      } else {
        state.follow.set(false);
        state.selectedIndex.set(0);
        state.scrollOffset.set(0);
      }
      return true;
    }
  }

  return true;
}

function manualScroll(state: ScreenState, delta: number): void {
  state.follow.set(false);
  const total = state.visibleLogs.get().length;
  if (total === 0) return;
  const next = clamp(state.selectedIndex.peek() + delta, 0, total - 1);
  state.selectedIndex.set(next);
}

function resetScroll(state: ScreenState): void {
  // When filter/search changes, jump back to the bottom and resume follow.
  state.follow.set(true);
  state.scrollOffset.set(0);
  state.selectedIndex.set(0);
}

function clamp(n: number, lo: number, hi: number): number {
  if (n < lo) return lo;
  if (n > hi) return hi;
  return n;
}
