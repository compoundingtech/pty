// Playground routing expressed as focus scopes + a top-level chord
// interceptor.
//
//   - ctrl+h / ctrl+l (pane-switch) are wired into AppConfig.onKey so
//     they intercept before ANY scope. Those chords must always win —
//     even a modal overlay shouldn't swallow them.
//
//   - "sidebar" and "main" pane scopes sit in the focus stack with
//     `active: () => pane.get() === <id>`. Only one is ever dispatched
//     per key; the other is inert.
//
//   - Modal overlays can push additional scopes on top at will; they'll
//     run before the pane scopes but after AppConfig.onKey, which is
//     the right ordering (modal can eat esc but not ctrl+c).
//
// Mouse is kept as a flat handler because the hit-test logic needs the
// actual rendered rects, which scopes don't track. If we add focus-aware
// hit-testing later we can revisit.

import { batch } from "../../src/tui/index.ts";
import type {
  KeyEvent, MouseEvent, ScreenContext, FocusManager,
} from "../../src/tui/index.ts";
// KeyEvent imported above — needed by makeGlobalChords's returned closure.
import type { Signal } from "../../src/tui/signals.ts";
import type { Demo } from "./types.ts";

export interface RouterDeps {
  selectedDemoId: Signal<string>;
  selectedPane: Signal<"sidebar" | "main">;
  showHelp: Signal<boolean>;
  showSource: Signal<boolean>;
  sidebarEntries: () => { kind: "header" | "demo"; demoId?: string }[];
  selectedDemo: () => Demo;
  selectNextDemo: (delta: number) => void;
  stepDemos: (delta: number) => void;
}

/** Top-level chord interceptor. Install via `AppConfig.onKey` so it runs
 *  before the focus stack — even a modal overlay won't swallow pane
 *  navigation or app-wide shortcuts. */
export function makeGlobalChords(deps: RouterDeps) {
  return (key: KeyEvent): boolean => {
    if (key.name === "h" && key.ctrl) { deps.selectedPane.set("sidebar"); return true; }
    if (key.name === "l" && key.ctrl) { deps.selectedPane.set("main");    return true; }
    return false;
  };
}

/** Push the playground's pane scopes onto `focus`. Returns a disposer. */
export function registerPlaygroundScopes(
  focus: FocusManager,
  deps: RouterDeps,
): () => void {
  const disposeSidebar = focus.push({
    id: "sidebar",
    active: () => deps.selectedPane.get() === "sidebar",
    onKey(key, ctx) {
      if (key.char === "q") { ctx.quit(); return true; }
      if (key.char === "?") { deps.showHelp.set(!deps.showHelp.peek()); return true; }
      if (key.char === "s") { deps.showSource.set(!deps.showSource.peek()); return true; }
      if (key.name === "up")       { deps.selectNextDemo(-1); return true; }
      if (key.name === "down")     { deps.selectNextDemo(1);  return true; }
      if (key.name === "pageup")   { deps.stepDemos(-5); return true; }
      if (key.name === "pagedown") { deps.stepDemos(5);  return true; }
      if (key.name === "right" || key.name === "tab" || key.name === "return") {
        deps.selectedPane.set("main");
        return true;
      }
      // Unhandled — bubble up to the "global" scope so chords like ctrl+h
      // still work. The main scope is inactive while we are, so nothing
      // else can intercept en route.
      return false;
    },
  });

  const disposeMain = focus.push({
    id: "main",
    active: () => deps.selectedPane.get() === "main",
    onKey(key, ctx) {
      // Every key the demo sees, regardless of whether the demo reports
      // it as consumed — nothing should ever leak to the sidebar while
      // main has focus.
      deps.selectedDemo().handleKey(key, ctx);
      return true;
    },
  });

  let disposed = false;
  return () => {
    if (disposed) return;
    disposed = true;
    disposeMain();
    disposeSidebar();
  };
}

/** Mouse is simpler: a single flat handler that decides sidebar vs main
 *  by x-coordinate. Call from the screen's handleMouse. */
export function handleMouse(deps: RouterDeps, event: MouseEvent): boolean {
  const sidebarWidth = 24;
  const inSidebar = event.x < sidebarWidth;

  if (inSidebar) {
    if (event.action === "scrollUp")   { deps.selectNextDemo(-1); return true; }
    if (event.action === "scrollDown") { deps.selectNextDemo(1);  return true; }
    if (event.action === "press" && event.button === "left") {
      const entries = deps.sidebarEntries();
      const entryIdx = event.y - 1;
      if (entryIdx >= 0 && entryIdx < entries.length) {
        const entry = entries[entryIdx];
        if (entry.kind === "demo" && entry.demoId) {
          batch(() => {
            deps.selectedDemoId.set(entry.demoId!);
            deps.selectedPane.set("main");
          });
          return true;
        }
      }
    }
    return false;
  }

  if (event.action === "press" && event.button === "left") {
    deps.selectedPane.set("main");
    return true;
  }
  if (event.action === "scrollUp")   { deps.selectNextDemo(-1); return true; }
  if (event.action === "scrollDown") { deps.selectNextDemo(1);  return true; }
  return false;
}
