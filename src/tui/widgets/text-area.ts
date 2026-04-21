// Multi-line text input widget (a.k.a. text area / composer).
//
// State-first: the consumer owns `TextAreaState`. Key handling is a pure
// function returning a new state or `null` when the key should escape the
// widget (unknown, or `ctrl+enter` — used by most shells to mean "submit"
// from a multi-line prompt).
//
// Model: `lines: string[]` (each is the logical line; not yet visually
// wrapped). `row`/`col` index into lines + character offset.
// Rendering walks each line and inserts a block cursor at the active cell
// for the focused row.

import { text, row as uiRow } from "../builders.ts";
import type { ColumnNode } from "../nodes.ts";
import type { UINode } from "../nodes.ts";
import type { KeyEvent } from "../input.ts";
import { prevWordBoundary, nextWordBoundary } from "./form.ts";

export interface TextAreaState {
  lines: string[];
  /** 0-based line index of the cursor. Always valid: >= 0 and < lines.length. */
  row: number;
  /** 0-based column offset into lines[row]. 0 ≤ col ≤ lines[row].length. */
  col: number;
}

export function createTextArea(initial = ""): TextAreaState {
  const lines = initial.length === 0 ? [""] : initial.split("\n");
  return { lines, row: 0, col: 0 };
}

export function textAreaToString(state: TextAreaState): string {
  return state.lines.join("\n");
}

/** Apply one key to the text area. Returns a new state when consumed, else
 *  `null` — consumers interpret `null` as "use this key for something else"
 *  (e.g. submit the form, cancel, etc). Specifically:
 *    - tab / backtab / escape always return null (owned by the outer form)
 *    - ctrl+return returns null (conventional "submit" from multi-line input)
 *    - all other editing keys return a new state */
export function applyTextAreaKey(state: TextAreaState, key: KeyEvent): TextAreaState | null {
  // Reserved outer-form keys.
  if (key.name === "tab" || key.name === "backtab" || key.name === "escape") return null;
  if (key.name === "return" && key.ctrl) return null;

  // Newline.
  if (key.name === "return") {
    const curLine = state.lines[state.row] ?? "";
    const before = curLine.slice(0, state.col);
    const after = curLine.slice(state.col);
    const nextLines = [
      ...state.lines.slice(0, state.row),
      before,
      after,
      ...state.lines.slice(state.row + 1),
    ];
    return { lines: nextLines, row: state.row + 1, col: 0 };
  }

  // Deletions.
  if (key.name === "backspace") {
    // At col 0 of a non-first line: merge up.
    if (state.col === 0) {
      if (state.row === 0) return state;
      const prev = state.lines[state.row - 1];
      const cur = state.lines[state.row];
      const mergedCol = prev.length;
      const merged = prev + cur;
      const nextLines = [
        ...state.lines.slice(0, state.row - 1),
        merged,
        ...state.lines.slice(state.row + 1),
      ];
      return { lines: nextLines, row: state.row - 1, col: mergedCol };
    }
    // Mid-line backspace.
    const curLine = state.lines[state.row];
    const nextLine = curLine.slice(0, state.col - 1) + curLine.slice(state.col);
    return {
      lines: state.lines.map((l, i) => (i === state.row ? nextLine : l)),
      row: state.row,
      col: state.col - 1,
    };
  }
  if (key.name === "delete") {
    const curLine = state.lines[state.row];
    // At end of line: merge with next line.
    if (state.col >= curLine.length) {
      if (state.row === state.lines.length - 1) return state;
      const merged = curLine + state.lines[state.row + 1];
      const nextLines = [
        ...state.lines.slice(0, state.row),
        merged,
        ...state.lines.slice(state.row + 2),
      ];
      return { lines: nextLines, row: state.row, col: state.col };
    }
    // Mid-line delete.
    const nextLine = curLine.slice(0, state.col) + curLine.slice(state.col + 1);
    return {
      lines: state.lines.map((l, i) => (i === state.row ? nextLine : l)),
      row: state.row,
      col: state.col,
    };
  }

  // Cursor movement.
  if (key.name === "left") {
    if (key.alt) {
      const cur = state.lines[state.row];
      if (state.col > 0) return { ...state, col: prevWordBoundary(cur, state.col) };
      if (state.row > 0) {
        const prev = state.lines[state.row - 1];
        return { ...state, row: state.row - 1, col: prev.length };
      }
      return state;
    }
    if (state.col > 0) return { ...state, col: state.col - 1 };
    if (state.row > 0) {
      const prev = state.lines[state.row - 1];
      return { ...state, row: state.row - 1, col: prev.length };
    }
    return state;
  }
  if (key.name === "right") {
    const cur = state.lines[state.row];
    if (key.alt) {
      if (state.col < cur.length) return { ...state, col: nextWordBoundary(cur, state.col) };
      if (state.row < state.lines.length - 1) {
        return { ...state, row: state.row + 1, col: 0 };
      }
      return state;
    }
    if (state.col < cur.length) return { ...state, col: state.col + 1 };
    if (state.row < state.lines.length - 1) {
      return { ...state, row: state.row + 1, col: 0 };
    }
    return state;
  }
  if (key.alt && key.char === "b") {
    const cur = state.lines[state.row];
    return { ...state, col: prevWordBoundary(cur, state.col) };
  }
  if (key.alt && key.char === "f") {
    const cur = state.lines[state.row];
    return { ...state, col: nextWordBoundary(cur, state.col) };
  }
  if (key.name === "up") {
    if (state.row === 0) return state;
    const prev = state.lines[state.row - 1];
    return { ...state, row: state.row - 1, col: Math.min(state.col, prev.length) };
  }
  if (key.name === "down") {
    if (state.row === state.lines.length - 1) return state;
    const next = state.lines[state.row + 1];
    return { ...state, row: state.row + 1, col: Math.min(state.col, next.length) };
  }
  if (key.name === "home" || (key.name === "a" && key.ctrl)) {
    return { ...state, col: 0 };
  }
  if (key.name === "end" || (key.name === "e" && key.ctrl)) {
    return { ...state, col: state.lines[state.row].length };
  }

  // Printable character.
  if (key.char && !key.ctrl && !key.alt) {
    const cur = state.lines[state.row];
    const next = cur.slice(0, state.col) + key.char + cur.slice(state.col);
    return {
      lines: state.lines.map((l, i) => (i === state.row ? next : l)),
      row: state.row,
      col: state.col + key.char.length,
    };
  }

  return null;
}

/** Render the text area as a column of rows. When `active`, the focused row
 *  paints an inverse-styled cell at the cursor position — the character
 *  UNDER the cursor gets its fg/bg swapped so neighbors don't shift. */
export function renderTextArea(state: TextAreaState, active: boolean): UINode {
  const children: UINode[] = state.lines.map((line, i) => {
    if (!active || i !== state.row) {
      return uiRow(text(line.length === 0 ? " " : line, "primary"));
    }
    const before = line.slice(0, state.col);
    const under  = line.slice(state.col, state.col + 1) || " ";
    const after  = line.slice(state.col + 1);
    return uiRow(
      text(before, "primary"),
      text(under,  "primary", { inverse: true }),
      text(after,  "primary"),
    );
  });
  const node: ColumnNode = { type: "column", children };
  return node;
}
