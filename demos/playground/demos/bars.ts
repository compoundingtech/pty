// Prompt bar (Claude-Code style input) + toolbar demos.

import {
  text, row, column, separator,
  signal,
  applyTextKey, type TextFieldState,
  createTextArea, applyTextAreaKey, type TextAreaState,
  promptBar, toolbar, toolbarItemFor,
  type ToolbarItem,
} from "../../../src/tui/index.ts";
import type { Demo } from "../types.ts";

// --- promptBar single-line ---
const single = signal<TextFieldState>({ text: "", cursor: 0 });

export const promptBarDemo: Demo = {
  id: "prompt-bar",
  category: "patterns",
  name: "prompt bar (single-line)",
  blurb: "Claude Code style: top/bottom rules, title overlay, optional status strip, prompt glyph.",
  render() {
    return [
      row(text("  Full-width input with left-aligned title:", "muted", { dim: true })),
      promptBar(
        { kind: "single", state: single.get() },
        {
          title: { text: "ask me anything", align: "left" },
          status: { left: " ctrl+c to cancel", right: "\u23ce submit " },
        },
      ),
      row(text("  Center-aligned title, no status:", "muted", { dim: true })),
      promptBar(
        { kind: "single", state: single.get() },
        { title: { text: "compose", align: "center" } },
      ),
      row(text("  Right-aligned title, custom glyph ($):", "muted", { dim: true })),
      promptBar(
        { kind: "single", state: single.get() },
        {
          glyph: "$",
          title: { text: "shell", align: "right" },
          status: { right: "zsh · ~/code " },
        },
      ),
    ];
  },
  handleKey(key) {
    const next = applyTextKey(single.peek(), key);
    if (next) { single.set(next); return true; }
    return false;
  },
  source: String.raw`const field = signal({ text: "", cursor: 0 });
// in render:
promptBar(
  { kind: "single", state: field.get() },
  {
    title:  { text: "ask me anything", align: "left" },
    status: { left: "ctrl+c to cancel", right: "⏎ submit" },
  },
)
// in keys:
const next = applyTextKey(field.peek(), key);
if (next) field.set(next);`,
};

// --- promptBar multi-line ---
const multi = signal<TextAreaState>(createTextArea("type here — return for newline, ctrl+return to submit"));

export const promptBarMultiDemo: Demo = {
  id: "prompt-bar-multi",
  category: "patterns",
  name: "prompt bar (multi-line)",
  blurb: "Same widget with a TextAreaState — shift+enter inserts a newline, ctrl+enter bubbles up.",
  render() {
    return [
      promptBar(
        { kind: "multi", state: multi.get() },
        {
          title: { text: "chat", align: "left" },
          status: { left: " shift+\u23ce newline", right: "ctrl+\u23ce send " },
        },
      ),
      row(text(`  lines: ${multi.get().lines.length}  cursor: (${multi.get().row}, ${multi.get().col})`, "muted", { dim: true })),
    ];
  },
  handleKey(key) {
    const next = applyTextAreaKey(multi.peek(), key);
    if (next) { multi.set(next); return true; }
    return false;
  },
  source: String.raw`const state = signal(createTextArea());
// in render:
promptBar(
  { kind: "multi", state: state.get() },
  { title: { text: "chat" }, status: { left: "shift+⏎ newline", right: "ctrl+⏎ send" } },
)
// in keys:
const next = applyTextAreaKey(state.peek(), key);
if (next) state.set(next);`,
};

// --- toolbar ---
const toolbarState = signal<string>("n");

const items: ToolbarItem[] = [
  { key: "n", label: "ew" },
  { key: "s", label: "ave" },
  { key: "/", label: "Search", hint: "fuzzy" },
  { key: "g", label: "it",  hint: "sync" },
  { key: "q", label: "uit" },
];

export const toolbarDemo: Demo = {
  id: "toolbar",
  category: "patterns",
  name: "toolbar",
  blurb: "Horizontal action bar with hotkey highlights. Press a letter to activate; [?] marks disabled.",
  render() {
    const withActive = items.map(i => ({ ...i, active: i.key === toolbarState.get() }));
    return [
      row(text("  bracket format (default):", "muted", { dim: true })),
      toolbar(withActive),
      row(text("  ", "muted")),
      row(text("  inline format (key highlighted inside the label):", "muted", { dim: true })),
      toolbar([
        { key: "n", label: "new",    active: toolbarState.get() === "n" },
        { key: "s", label: "save",   active: toolbarState.get() === "s" },
        { key: "o", label: "open",   active: toolbarState.get() === "o" },
        { key: "q", label: "quit",   active: toolbarState.get() === "q" },
      ], { format: "inline" }),
      row(text("  ", "muted")),
      row(text(`  active: ${toolbarState.get()}`, "muted")),
    ];
  },
  handleKey(key) {
    const match = toolbarItemFor(items, key.char);
    if (match) { toolbarState.set(match.key); return true; }
    const inlineKeys = ["n", "s", "o", "q"];
    if (key.char && inlineKeys.includes(key.char)) {
      toolbarState.set(key.char);
      return true;
    }
    return false;
  },
  source: String.raw`const items: ToolbarItem[] = [
  { key: "n", label: "ew" },
  { key: "s", label: "ave", active: true },
  { key: "/", label: "Search", hint: "fuzzy" },
];
// render:
toolbar(items)              // bracket format: [N]ew [S]ave [/]Search
toolbar(items, { format: "inline" })   // for natural-language labels

// in handleKey:
const match = toolbarItemFor(items, key.char);
if (match) /* run action */;`,
};
