// Global, signal-backed command registry.
//
// The bare `command-palette.ts` takes `Command[]` at render time — fine for
// small apps, awkward for anything where commands should be "contributed"
// from all corners of the codebase. This registry fixes that.
//
// Two entry points:
//
//   registerGlobalCommand(cmd) -> dispose
//     Registers a command for the lifetime of the app. Returns a disposer
//     that removes it.
//
//   useCommandScope(scopeId, commands) -> dispose
//     Registers commands under a named scope. Use this inside a screen's
//     mount/unmount lifecycle to surface commands only while the screen is
//     active. The common contextual use-case is binding to a focused item:
//     "useCommandScope(`focused:${id}`, [{ ... }])".
//
// `allCommands` is a computed signal that flattens everything currently
// registered. The `command-palette.ts` widget already takes a `Command[]`
// array — just pass `allCommands.get()` to get all live commands.

import { signal, computed, type Signal, type Computed } from "../signals.ts";
import type { Command } from "./command-palette.ts";

/** Internal: rev-counter that bumps on every change so `allCommands`
 *  recomputes. We store commands in a Map keyed by scopeId; the Map itself
 *  is the state, the rev signal is how we propagate through the reactive
 *  graph without allocating a new Map on every registration. */
const rev = signal(0);
const scopes = new Map<string, Command[]>();
const GLOBAL = "__global__";

function touch(): void {
  rev.set(rev.peek() + 1);
}

/** Register a single command globally. Returns a disposer. */
export function registerGlobalCommand(cmd: Command): () => void {
  const existing = scopes.get(GLOBAL) ?? [];
  scopes.set(GLOBAL, [...existing, cmd]);
  touch();
  return () => {
    const list = scopes.get(GLOBAL);
    if (!list) return;
    const next = list.filter(c => c.id !== cmd.id);
    if (next.length === 0) scopes.delete(GLOBAL);
    else scopes.set(GLOBAL, next);
    touch();
  };
}

/** Register a batch of commands under `scopeId`. Calling again with the
 *  same `scopeId` REPLACES the previous batch — convenient for contextual
 *  ("focused thing") commands that change with selection. Returns a
 *  disposer that removes the whole scope. */
export function useCommandScope(scopeId: string, commands: Command[]): () => void {
  if (scopeId === GLOBAL) {
    throw new Error(`scope id "${GLOBAL}" is reserved`);
  }
  if (commands.length === 0) {
    scopes.delete(scopeId);
  } else {
    scopes.set(scopeId, commands);
  }
  touch();
  return () => {
    if (scopes.delete(scopeId)) touch();
  };
}

/** Remove every command under `scopeId` (no-op if unknown). Useful when
 *  you want to clear a scope without holding a disposer. */
export function clearCommandScope(scopeId: string): void {
  if (scopes.delete(scopeId)) touch();
}

/** Look up a command by id across all scopes. Returns the first match. */
export function findCommand(id: string): Command | undefined {
  for (const list of scopes.values()) {
    const m = list.find(c => c.id === id);
    if (m) return m;
  }
  return undefined;
}

/** Reactive view of every registered command, flattened. Scope order is
 *  insertion-order of the underlying Map. Global commands stay at the
 *  front because they're inserted with the sentinel key at registration. */
export const allCommands: Computed<Command[]> = computed(() => {
  // Touch the rev signal so this recomputes on every registration change.
  rev.get();
  const out: Command[] = [];
  // Iterate insertion-ordered entries.
  for (const list of scopes.values()) out.push(...list);
  return out;
});

/** Run a command by id. Silently does nothing if the id is not registered.
 *  Convenience for keybindings that trigger a named command without
 *  opening the palette. */
export function runCommand(id: string): boolean {
  const cmd = findCommand(id);
  if (!cmd) return false;
  cmd.run();
  return true;
}

/** For tests / debugging — wipes the entire registry. */
export function _resetCommandRegistry(): void {
  scopes.clear();
  touch();
}
