// New/Edit reminder overlay
import {
  overlay, text, row, separator,
  type KeyEvent, type ScreenContext, type UINode,
} from "../../../src/tui/index.ts";
import {
  formTitle, formDue, formPriority, formBody, formTags, formField,
  activeOverlay,
} from "../router.ts";

function fieldLabel(idx: number, label: string, value: string, active: boolean): UINode[] {
  const indicator = active ? "\u25b8 " : "  ";
  const color = active ? "accent" : "muted";
  const display = value + (active ? "\u2588" : "");
  return [
    row(
      text(indicator + label + ": ", color, { bold: active }),
      text(display, active ? "primary" : "secondary"),
    ),
  ];
}

function renderFormContent(): UINode[] {
  const field = formField.get();
  const isEdit = activeOverlay.get() === "edit";

  return [
    text(isEdit ? "Edit Reminder" : "New Reminder", "accent", { bold: true }),
    separator(),
    ...fieldLabel(0, "Title", formTitle.get(), field === 0),
    ...fieldLabel(1, "Due", formDue.get(), field === 1),
    row(
      text(field === 2 ? "\u25b8 " : "  ", field === 2 ? "accent" : "muted"),
      text("Priority: ", field === 2 ? "accent" : "muted", { bold: field === 2 }),
      text("\u25c0 ", "muted"),
      text(formPriority.get(), formPriority.get() === "high" ? "error" : formPriority.get() === "medium" ? "warn" : "ok", { bold: true }),
      text(" \u25b6", "muted"),
    ),
    ...fieldLabel(3, "Tags", formTags.get(), field === 3),
    ...fieldLabel(4, "Notes", formBody.get(), field === 4),
    separator(),
    text("tab next  \u2190\u2192 priority  enter save  esc cancel", "muted"),
  ];
}

export const newReminderOverlay = overlay({
  id: "new-reminder",
  title: "New Reminder",
  width: (cols) => Math.min(60, cols - 8),
  height: (rows) => Math.min(16, rows - 4),
  render(_ctx: ScreenContext): UINode[] {
    return renderFormContent();
  },
  handleKey(_key: KeyEvent, _ctx: ScreenContext): boolean {
    return true; // All keys handled by router's handleFormKey
  },
});
