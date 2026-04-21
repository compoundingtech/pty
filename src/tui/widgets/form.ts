// Form widget — multi-field focus management on top of simple text fields.
//
// State-first: you own a `FormState<T>` signal. Key handling is a pure
// function that delegates to `applyTextKey` for printable/edit keys and to
// a focus-ring walker for tab/backtab.
//
// Promoted from demos/reminders/tui/widgets/form.ts with:
//   - Generic over a field-id enum or string union.
//   - Dedicated backtab handling (pty's input parser now emits it).
//   - A `handleFormKey` umbrella that consumers can delegate to.

import type { KeyEvent } from "../input.ts";
import { text as textBuilder } from "../builders.ts";

export interface TextFieldState {
  text: string;
  cursor: number;
}

/** Word-class predicates: alphanumerics and underscores are "word" chars,
 *  everything else is "non-word". Matches `\w` in most regex flavours and
 *  feels right for typical CLI / prose input. */
function isWordChar(ch: string): boolean {
  return /[\p{L}\p{N}_]/u.test(ch);
}

/** Find the index of the start of the previous word, searching backward
 *  from `pos`. Skips any trailing whitespace/punctuation first (so the
 *  first backward keystroke always feels like it did something). */
export function prevWordBoundary(text: string, pos: number): number {
  let i = pos;
  // Skip non-word chars immediately behind the cursor.
  while (i > 0 && !isWordChar(text[i - 1])) i--;
  // Skip the word itself.
  while (i > 0 && isWordChar(text[i - 1])) i--;
  return i;
}

/** Find the index one past the end of the next word, searching forward. */
export function nextWordBoundary(text: string, pos: number): number {
  let i = pos;
  while (i < text.length && !isWordChar(text[i])) i++;
  while (i < text.length && isWordChar(text[i])) i++;
  return i;
}

/** Handle a keystroke against a simple single-line text field.
 *  Returns a new state if the key was consumed, else `null` so the caller
 *  can dispatch it elsewhere (e.g. treat Enter as submit at the form level). */
export function applyTextKey(state: TextFieldState, key: KeyEvent): TextFieldState | null {
  if (key.name === "backspace") {
    if (state.cursor === 0) return state;
    return {
      text: state.text.slice(0, state.cursor - 1) + state.text.slice(state.cursor),
      cursor: state.cursor - 1,
    };
  }
  if (key.name === "delete") {
    if (state.cursor >= state.text.length) return state;
    return {
      text: state.text.slice(0, state.cursor) + state.text.slice(state.cursor + 1),
      cursor: state.cursor,
    };
  }
  if (key.name === "left") {
    if (key.alt) return { ...state, cursor: prevWordBoundary(state.text, state.cursor) };
    if (state.cursor === 0) return state;
    return { ...state, cursor: state.cursor - 1 };
  }
  if (key.name === "right") {
    if (key.alt) return { ...state, cursor: nextWordBoundary(state.text, state.cursor) };
    if (state.cursor >= state.text.length) return state;
    return { ...state, cursor: state.cursor + 1 };
  }
  // emacs-style alt+b / alt+f word motion, same semantics as alt+arrows.
  if (key.alt && key.char === "b") {
    return { ...state, cursor: prevWordBoundary(state.text, state.cursor) };
  }
  if (key.alt && key.char === "f") {
    return { ...state, cursor: nextWordBoundary(state.text, state.cursor) };
  }
  if (key.name === "home" || (key.name === "a" && key.ctrl)) {
    return { ...state, cursor: 0 };
  }
  if (key.name === "end" || (key.name === "e" && key.ctrl)) {
    return { ...state, cursor: state.text.length };
  }
  if (key.name === "u" && key.ctrl) {
    return { text: state.text.slice(state.cursor), cursor: 0 };
  }
  // Printable character — ignore ctrl/alt-modified keys so shortcuts don't
  // leak as text into the field.
  if (key.char && !key.ctrl && !key.alt) {
    return {
      text: state.text.slice(0, state.cursor) + key.char + state.text.slice(state.cursor),
      cursor: state.cursor + key.char.length,
    };
  }
  return null;
}

/** Compose a field display string with a block cursor when the field is
 *  active. This is the legacy "insert a block character" API — it pushes
 *  neighbors sideways and is kept only for consumers that want a single
 *  string back. For proper cursor-on-top-of-character rendering, use
 *  `renderFieldNodes` which returns 3 TextNodes (before / inverse-cursor /
 *  after). */
export function renderFieldText(text: string, cursor: number, active: boolean): string {
  if (!active) return text || "";
  const before = text.slice(0, cursor);
  const after = text.slice(cursor);
  return `${before}\u2588${after}`;
}

/** Render a text field as three inline TextNodes (before, cursor, after)
 *  so the cursor paints ON TOP of the character at `cursor` instead of
 *  shoving it sideways. Pass through `row(...renderFieldNodes(...))`. */
export function renderFieldNodes(
  text: string,
  cursor: number,
  active: boolean,
  opts: { color?: import("../nodes.ts").Color; bold?: boolean } = {},
): import("../nodes.ts").UINode[] {
  const color = opts.color ?? "primary";
  const bold = opts.bold ?? false;
  if (!active) {
    return [textBuilder(text || "", color, { bold })];
  }
  const before = text.slice(0, cursor);
  const under  = text.slice(cursor, cursor + 1) || " ";
  const after  = text.slice(cursor + 1);
  return [
    textBuilder(before, color, { bold }),
    textBuilder(under,  color, { bold, inverse: true }),
    textBuilder(after,  color, { bold }),
  ];
}

/** A form's per-field state. The consumer provides the field ids; the form
 *  tracks which one is focused and walks between them on tab/backtab. */
export interface FormState<Id extends string> {
  values: Record<Id, TextFieldState>;
  /** null only when the field set is empty — otherwise always one of `order`. */
  focused: Id | null;
  order: readonly Id[];
}

export function createFormState<Id extends string>(
  order: readonly Id[],
  initial: Record<Id, string>,
): FormState<Id> {
  const values = {} as Record<Id, TextFieldState>;
  for (const id of order) {
    const t = initial[id] ?? "";
    values[id] = { text: t, cursor: t.length };
  }
  return {
    values,
    focused: order[0] ?? null,
    order,
  };
}

function walkFocus<Id extends string>(state: FormState<Id>, delta: 1 | -1): FormState<Id> {
  if (state.focused == null || state.order.length === 0) return state;
  const idx = state.order.indexOf(state.focused);
  if (idx < 0) return state;
  const nextIdx = (idx + delta + state.order.length) % state.order.length;
  return { ...state, focused: state.order[nextIdx] };
}

export function focusField<Id extends string>(state: FormState<Id>, id: Id): FormState<Id> {
  if (!state.order.includes(id)) return state;
  return { ...state, focused: id };
}

export function setFieldText<Id extends string>(
  state: FormState<Id>,
  id: Id,
  text: string,
): FormState<Id> {
  const current = state.values[id];
  if (!current) return state;
  return {
    ...state,
    values: { ...state.values, [id]: { text, cursor: text.length } },
  };
}

export interface HandleFormKeyResult<Id extends string> {
  state: FormState<Id>;
  /** What happened — lets the consumer react (e.g. open a picker on Enter
   *  in a "due" field). "submit" means Enter in the last field, "cancel"
   *  means Escape anywhere, "activate" means Enter in a non-last field. */
  action: "edited" | "moved" | "submit" | "cancel" | "activate" | "none";
}

/** Default form key dispatch:
 *    - tab       -> focus next field
 *    - backtab   -> focus previous field (requires pty's backtab support)
 *    - enter     -> "activate" if a non-last field, "submit" if the last
 *    - escape    -> "cancel"
 *    - other     -> delegate to `applyTextKey` on the focused field
 *  Consumers that want a custom behaviour (e.g. enter-opens-picker for the
 *  "due" field) should check `action` / `state.focused` and react BEFORE
 *  calling this, or construct their own dispatcher using the helpers. */
export function handleFormKey<Id extends string>(
  state: FormState<Id>,
  key: KeyEvent,
): HandleFormKeyResult<Id> {
  if (key.name === "tab") {
    return { state: walkFocus(state, 1), action: "moved" };
  }
  if (key.name === "backtab") {
    return { state: walkFocus(state, -1), action: "moved" };
  }
  if (key.name === "escape") {
    return { state, action: "cancel" };
  }
  if (key.name === "return") {
    if (!state.focused) return { state, action: "none" };
    const idx = state.order.indexOf(state.focused);
    const isLast = idx === state.order.length - 1;
    return { state, action: isLast ? "submit" : "activate" };
  }

  if (!state.focused) return { state, action: "none" };
  const field = state.values[state.focused];
  const updated = applyTextKey(field, key);
  if (updated === null) return { state, action: "none" };
  return {
    state: {
      ...state,
      values: { ...state.values, [state.focused]: updated },
    },
    action: "edited",
  };
}
