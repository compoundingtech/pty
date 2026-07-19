// select — an SRCL-style dropdown select.
//
// SRCL's Select is a button showing the current value that opens a list of
// options; arrow keys move a highlight, Enter picks, Escape closes. This
// mirrors it as a state-first widget (the caller owns the open/highlight
// state and the chosen index) with a pure render + a pure key reducer — the
// same shape as the other interactive widgets (`tabs`, `command-palette`).
//
// It renders with existing nodes (a caret + label button; an option list when
// open) — no renderer surgery. ComboBox (search + filter + pick) is already
// served by the `command-palette` widget, so this covers the remaining
// dropdown role. Styled with semantic tokens only (B-ready).

import { row, column, text } from "../builders.ts";
import type { ColumnNode, UINode } from "../nodes.ts";
import type { KeyEvent } from "../input.ts";

/** Open/closed + highlighted-option state. Caller-owned. */
export interface SelectState {
  open: boolean;
  /** Highlighted option while open (0-based). */
  index: number;
}

export function createSelectState(index = 0): SelectState {
  return { open: false, index };
}

export interface SelectOptions {
  /** Shown when no option is selected (selectedIndex out of range). */
  placeholder?: string;
  /** Highlight the closed button (accent + bold) — e.g. when it has focus. */
  focused?: boolean;
  /** Caret glyph when open / closed. Default "▾" / "▸". */
  openCaret?: string;
  closedCaret?: string;
}

/** Render the dropdown: a caret + value button, plus the option list when open.
 *  `selectedIndex` is the chosen value; `state.index` is the open highlight. */
export function renderSelect(
  options: string[],
  selectedIndex: number,
  state: SelectState,
  opts: SelectOptions = {},
): ColumnNode {
  const focused = opts.focused ?? false;
  const value = options[selectedIndex] ?? opts.placeholder ?? "(none)";
  const caret = state.open ? (opts.openCaret ?? "▾") : (opts.closedCaret ?? "▸");

  const button = row(
    text(`${caret} `, { fg: focused ? "accent" : "muted" }),
    text(value, { fg: focused ? "accent" : "primary", ...(focused ? { bold: true } : {}) }),
  );

  const nodes: UINode[] = [button];
  if (state.open) {
    options.forEach((opt, i) => {
      const active = i === state.index;
      nodes.push(
        row(
          text("  "),
          text(active ? `› ${opt}` : `  ${opt}`, {
            fg: active ? "accent" : "secondary",
            ...(active ? { bold: true } : {}),
          }),
        ),
      );
    });
  }
  return column({}, nodes);
}

export interface HandleSelectKeyResult {
  state: SelectState;
  /** Set to the chosen index when the user commits a selection (Enter while open). */
  selectedIndex?: number;
}

/** Pure key reducer. Closed: Enter/Down opens. Open: Up/Down move the
 *  highlight, Enter commits (and closes), Escape closes without committing. */
export function handleSelectKey(
  state: SelectState,
  optionsLength: number,
  key: KeyEvent,
): HandleSelectKeyResult {
  if (!state.open) {
    if (key.name === "return" || key.name === "down") {
      return { state: { open: true, index: state.index } };
    }
    return { state };
  }
  switch (key.name) {
    case "up":
      return { state: { ...state, index: Math.max(0, state.index - 1) } };
    case "down":
      return { state: { ...state, index: Math.min(optionsLength - 1, state.index + 1) } };
    case "return":
      return { state: { open: false, index: state.index }, selectedIndex: state.index };
    case "escape":
      return { state: { ...state, open: false } };
    default:
      return { state };
  }
}
