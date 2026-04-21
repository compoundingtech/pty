// Confirm modal — convenience over `overlay()` for yes/no dialogs.
//
// Usage: own a `signal<ConfirmState | null>`. When you want to ask, set it
// to a fresh state via `createConfirm(...)`. Route its overlay through
// your app config's `overlay: () => ...`. On the `action: "yes" | "no"`
// from `handleConfirmKey`, clear the signal and run whatever the answer
// means in your app.

import { row, column, text, panel, separator } from "../builders.ts";
import type { UINode } from "../nodes.ts";
import type { KeyEvent } from "../input.ts";

export interface ConfirmState {
  title: string;
  message: string;
  yesLabel: string;
  noLabel: string;
  focused: "yes" | "no";
}

export interface CreateConfirmOptions {
  title: string;
  message: string;
  yesLabel?: string;
  noLabel?: string;
  /** Which button starts focused. Default "no" — safer for destructive ops. */
  defaultFocus?: "yes" | "no";
}

export function createConfirm(opts: CreateConfirmOptions): ConfirmState {
  return {
    title: opts.title,
    message: opts.message,
    yesLabel: opts.yesLabel ?? "Yes",
    noLabel: opts.noLabel ?? "No",
    focused: opts.defaultFocus ?? "no",
  };
}

export interface HandleConfirmResult {
  state: ConfirmState;
  action: "yes" | "no" | "pending";
}

/** Default bindings:
 *    - left / right / tab / backtab  -> toggle focus
 *    - return                        -> commit the focused button
 *    - escape                        -> always "no"
 *    - y / n                         -> shortcut commit (case-insensitive) */
export function handleConfirmKey(state: ConfirmState, key: KeyEvent): HandleConfirmResult {
  if (key.name === "left" || key.name === "right" ||
      key.name === "tab"  || key.name === "backtab") {
    return { state: { ...state, focused: state.focused === "yes" ? "no" : "yes" }, action: "pending" };
  }
  if (key.name === "return") {
    return { state, action: state.focused };
  }
  if (key.name === "escape") {
    return { state, action: "no" };
  }
  if (key.char === "y" || key.char === "Y") return { state, action: "yes" };
  if (key.char === "n" || key.char === "N") return { state, action: "no" };
  return { state, action: "pending" };
}

/** Render the confirm dialog body — wrap in an `overlay()` screen. */
export function confirmPanel(state: ConfirmState): UINode {
  const y = state.focused === "yes"
    ? text(` ${state.yesLabel} `, "accent", { bold: true })
    : text(` ${state.yesLabel} `, "muted");
  const n = state.focused === "no"
    ? text(` ${state.noLabel} `, "accent", { bold: true })
    : text(` ${state.noLabel} `, "muted");
  return panel(state.title, [
    row(text(state.message, "primary")),
    separator(),
    row(
      text("  ", "muted"),
      y,
      text("   ", "muted"),
      n,
    ),
    row(text("  y / n / enter / esc", "muted", { dim: true })),
  ]);
}
