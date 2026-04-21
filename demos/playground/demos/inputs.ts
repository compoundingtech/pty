// Inputs: text-input (single line), text-area (multi line), form (focus ring),
// date-picker.

import {
  text, row, column, panel, separator, canvas,
  signal,
  applyTextKey, renderFieldText,
  createTextArea, applyTextAreaKey, renderTextArea, textAreaToString,
  createFormState, handleFormKey, renderFieldText as rft,
  datePickerFromDate, handleDatePickerKey, datePickerBody,
  type TextFieldState, type FormState, type DatePickerState, type TextAreaState,
} from "../../../src/tui/index.ts";
import type { Demo } from "../types.ts";

// --- single-line text input ---
const singleField = signal<TextFieldState>({ text: "", cursor: 0 });

export const textInputDemo: Demo = {
  id: "text-input",
  category: "inputs",
  name: "single-line input",
  blurb: "applyTextKey handles printable chars + arrows + home/end + ctrl+u.",
  render() {
    const s = singleField.get();
    return [
      row(text("  Type something: ", "muted")),
      row(text("  [ " + renderFieldText(s.text, s.cursor, true) + " ]", "primary")),
      separator(),
      row(text(`  text   = ${JSON.stringify(s.text)}`, "muted", { dim: true })),
      row(text(`  cursor = ${s.cursor}`, "muted", { dim: true })),
    ];
  },
  handleKey(key) {
    const next = applyTextKey(singleField.peek(), key);
    if (next) { singleField.set(next); return true; }
    return false;
  },
  source: String.raw`const field = signal({ text: "", cursor: 0 });
// in handleKey:
const next = applyTextKey(field.peek(), key);
if (next) field.set(next);
// in render:
row(text(renderFieldText(field.get().text, field.get().cursor, true)))`,
};

// --- multi-line text area ---
const composer = signal<TextAreaState>(createTextArea(
  "Write notes, logs, emails, chat messages...\n\nreturn for newline, ctrl+return to submit, esc to cancel."
));

export const textAreaDemo: Demo = {
  id: "text-area",
  category: "inputs",
  name: "multi-line composer",
  blurb: "createTextArea + applyTextAreaKey. Return for newline; ctrl+return and esc return null so the caller can submit/cancel.",
  render() {
    return [
      panel("compose", [renderTextArea(composer.get(), true)]),
      row(text(`  ${composer.get().lines.length} lines, cursor (${composer.get().row}, ${composer.get().col})`, "muted", { dim: true })),
    ];
  },
  handleKey(key) {
    const next = applyTextAreaKey(composer.peek(), key);
    if (next) { composer.set(next); return true; }
    return false;
  },
  source: String.raw`const state = signal(createTextArea());
// in render:
panel("compose", [renderTextArea(state.get(), true)])
// in handleKey:
const next = applyTextAreaKey(state.peek(), key);
if (next) state.set(next); // null = ctrl+return / esc / tab — handle externally`,
};

// --- form with focus ring ---
type Field = "title" | "notes" | "due";
const formState = signal<FormState<Field>>(createFormState(
  ["title", "notes", "due"] as const,
  { title: "Buy milk", notes: "on the way home", due: "tomorrow 5pm" },
));
const formStatus = signal<string>("edit me — tab / shift-tab to change fields");

export const formDemo: Demo = {
  id: "form",
  category: "inputs",
  name: "form with focus ring",
  blurb: "Tab/backtab walk the fields; enter on the last is 'submit'; escape is 'cancel'.",
  render() {
    const f = formState.get();
    const renderField = (id: Field, label: string) => {
      const field = f.values[id];
      const active = f.focused === id;
      return row(
        text(`  ${label.padEnd(8)}`, active ? "accent" : "muted", { bold: active }),
        text(rft(field.text, field.cursor, active), active ? "primary" : "secondary"),
      );
    };
    return [
      renderField("title", "title"),
      renderField("notes", "notes"),
      renderField("due", "due"),
      separator(),
      row(text(`  ${formStatus.get()}`, "muted", { dim: true })),
    ];
  },
  handleKey(key) {
    const r = handleFormKey(formState.peek(), key);
    formState.set(r.state);
    if (r.action === "submit") formStatus.set(`submitted: ${JSON.stringify(Object.fromEntries(
      Object.entries(r.state.values).map(([k, v]) => [k, v.text])
    ))}`);
    else if (r.action === "cancel") formStatus.set("cancelled");
    else if (r.action === "moved") formStatus.set(`focused: ${r.state.focused}`);
    else if (r.action === "edited") formStatus.set("editing...");
    else if (r.action === "activate") formStatus.set(`enter on ${r.state.focused} — trigger a picker overlay here`);
    return r.action !== "none";
  },
  source: String.raw`const state = signal(createFormState(
  ["title", "notes", "due"] as const,
  { title: "", notes: "", due: "" },
));
// in handleKey:
const r = handleFormKey(state.peek(), key);
state.set(r.state);
switch (r.action) {
  case "submit":   /* save the form */ break;
  case "cancel":   /* close the overlay */ break;
  case "activate": /* open a picker for this field */ break;
}`,
};

// --- date picker ---
const picker = signal<DatePickerState>(datePickerFromDate(new Date()));

export const datePickerDemo: Demo = {
  id: "date-picker",
  category: "inputs",
  name: "date picker",
  blurb: "Arrow keys navigate days; [ ] change month; h/H hour; m/M minute; selection highlighted.",
  render() {
    return datePickerBody(picker.get());
  },
  handleKey(key) {
    const next = handleDatePickerKey(picker.peek(), key);
    if (next) { picker.set(next); return true; }
    return false;
  },
  source: String.raw`const picker = signal(datePickerFromDate(new Date()));
// in render:
return datePickerBody(picker.get());
// in handleKey:
const next = handleDatePickerKey(picker.peek(), key);
if (next) picker.set(next);`,
};
