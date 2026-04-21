// Focus manager — a small, stack-based event router for TUI apps that
// have panes / overlays / nested widgets.
//
// Why: without this, every app rolls its own "which-area-gets-this-key"
// dispatcher. We've done that in the reminders TUI, the playground, and
// the interactive pty list — three apps, three bespoke routers, three
// ways to get it wrong. The most common bug: a key that one layer
// "didn't care about" silently reaches another layer that DID care and
// triggers the wrong action.
//
// Model:
//   - Scopes form a stack. Innermost = most recently pushed.
//   - Each scope has optional onKey / onMouse handlers.
//   - Dispatch walks innermost -> outermost. First handler to return
//     true consumes the event; the rest are skipped.
//   - A scope can declare itself conditionally active via `active()`.
//     Inactive scopes are skipped during dispatch (but still kept in
//     the stack so their disposers and onKey closures stay stable).
//
// Typical use:
//
//   // Outermost: global shortcuts.
//   focus.push({
//     id: "global",
//     onKey(key, ctx) {
//       if (key.char === "q") { ctx.quit(); return true; }
//       return false;
//     },
//   });
//
//   // Per-pane scopes. Both stay pushed; only the active one dispatches.
//   focus.push({
//     id: "sidebar",
//     active: () => pane.get() === "sidebar",
//     onKey(key) { if (key.name === "up") { ... return true; } return false; },
//   });
//   focus.push({
//     id: "main",
//     active: () => pane.get() === "main",
//     onKey(key, ctx) {
//       return currentDemo().handleKey(key, ctx);
//     },
//   });
//
//   // Modal / overlay appears on top — its onKey runs first while alive.
//   const dispose = focus.push({ id: "confirm-delete", onKey: ... });
//   // when closed:
//   dispose();

import type { KeyEvent, MouseEvent } from "./input.ts";
import type { ScreenContext } from "./types.ts";

export interface FocusScope {
  /** Short identifier — used for debugging and introspection, not
   *  required to be unique (though it usually is). */
  id: string;
  /** Predicate controlling whether this scope dispatches events. Defaults
   *  to always-active. Use this to keep multiple sibling scopes in the
   *  stack (e.g. one per pane) and switch which one responds. */
  active?: () => boolean;
  /** Returns true when the scope consumed the key. False lets the event
   *  bubble to the next-outer scope. */
  onKey?: (key: KeyEvent, ctx: ScreenContext) => boolean;
  /** Same semantics as onKey for mouse events. */
  onMouse?: (event: MouseEvent, ctx: ScreenContext) => boolean;
}

export interface FocusManager {
  /** Push a scope onto the stack. Returns a disposer that removes it.
   *  The disposer is idempotent — calling it twice is safe. */
  push(scope: FocusScope): () => void;
  /** Current innermost-active scope, or null if none. */
  current(): FocusScope | null;
  /** Snapshot of every scope, root -> innermost. */
  stack(): readonly FocusScope[];
  /** Dispatch a key event. Returns true if any scope consumed it. */
  dispatchKey(key: KeyEvent, ctx: ScreenContext): boolean;
  /** Dispatch a mouse event. */
  dispatchMouse(event: MouseEvent, ctx: ScreenContext): boolean;
}

export function createFocusManager(): FocusManager {
  const scopes: FocusScope[] = [];

  function isActive(scope: FocusScope): boolean {
    return scope.active ? scope.active() : true;
  }

  return {
    push(scope: FocusScope): () => void {
      scopes.push(scope);
      let disposed = false;
      return () => {
        if (disposed) return;
        disposed = true;
        const i = scopes.indexOf(scope);
        if (i >= 0) scopes.splice(i, 1);
      };
    },

    current(): FocusScope | null {
      for (let i = scopes.length - 1; i >= 0; i--) {
        if (isActive(scopes[i])) return scopes[i];
      }
      return null;
    },

    stack(): readonly FocusScope[] {
      return scopes.slice();
    },

    dispatchKey(key: KeyEvent, ctx: ScreenContext): boolean {
      // Iterate on a snapshot so a handler can push or pop scopes
      // without disturbing the current dispatch pass.
      const snapshot = scopes.slice();
      for (let i = snapshot.length - 1; i >= 0; i--) {
        const scope = snapshot[i];
        if (!isActive(scope)) continue;
        if (scope.onKey && scope.onKey(key, ctx)) return true;
      }
      return false;
    },

    dispatchMouse(event: MouseEvent, ctx: ScreenContext): boolean {
      const snapshot = scopes.slice();
      for (let i = snapshot.length - 1; i >= 0; i--) {
        const scope = snapshot[i];
        if (!isActive(scope)) continue;
        if (scope.onMouse && scope.onMouse(event, ctx)) return true;
      }
      return false;
    },
  };
}
