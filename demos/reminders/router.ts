// Simple router for the reminders demo: view switching + overlay management
import * as fs from "node:fs";
import { signal, batch } from "../../src/tui/index.ts";
import type { KeyEvent, ScreenContext } from "../../src/tui/index.ts";
import {
  cycleTheme, cycleView, currentView,
  moveUp, moveDown, toggleComplete, deleteReminder,
  selectedReminder, boardColumn, boardScroll, boardColumns,
  calendarMonth, calendarYear, calendarSelectedDay,
  createReminder, loadReminders, serializeReminder,
} from "./state.ts";
import { scrollUp, scrollDown, updateScrollRegion } from "../../src/tui/index.ts";

// --- Overlay state ---
export type OverlayType = "new" | "edit" | "confirm-delete" | null;
export const activeOverlay = signal<OverlayType>(null);

// --- Form state ---
export const formTitle = signal("");
export const formDue = signal("");
export const formPriority = signal<"low" | "medium" | "high">("medium");
export const formTags = signal("");
export const formBody = signal("");
export const formField = signal(0); // which field is focused

const FIELD_COUNT = 5;
const PRIORITY_ORDER: Array<"low" | "medium" | "high"> = ["low", "medium", "high"];

export function openNewOverlay(): void {
  batch(() => {
    formTitle.set("");
    formDue.set(new Date().toISOString().split("T")[0]);
    formPriority.set("medium");
    formTags.set("");
    formBody.set("");
    formField.set(0);
    activeOverlay.set("new");
  });
}

export function openEditOverlay(): void {
  const r = selectedReminder.get();
  if (!r) return;
  batch(() => {
    formTitle.set(r.title);
    formDue.set(r.due);
    formPriority.set(r.priority);
    formTags.set(r.tags.join(", "));
    formBody.set(r.body);
    formField.set(0);
    activeOverlay.set("edit");
  });
}

export function openDeleteOverlay(): void {
  if (!selectedReminder.get()) return;
  activeOverlay.set("confirm-delete");
}

export function closeOverlay(): void {
  activeOverlay.set(null);
}

// --- Key handling ---
export function handleGlobalKey(key: KeyEvent, ctx: ScreenContext): boolean {
  // Ctrl+C always quits
  if (key.name === "c" && key.ctrl) return false;
  if (key.char === "q") return false;

  const overlay = activeOverlay.peek();

  // Handle overlay keys
  if (overlay === "confirm-delete") {
    if (key.char === "y" || key.char === "Y") {
      deleteReminder();
      closeOverlay();
      return true;
    }
    if (key.char === "n" || key.char === "N" || key.name === "escape") {
      closeOverlay();
      return true;
    }
    return true;
  }

  if (overlay === "new" || overlay === "edit") {
    return handleFormKey(key);
  }

  // Global keys
  if (key.char === "T") { cycleTheme(); return true; }
  if (key.char === "v") { cycleView(); return true; }
  if (key.char === "n") { openNewOverlay(); return true; }
  if (key.name === "escape") { closeOverlay(); return true; }

  // View-specific keys
  const view = currentView.peek();
  if (view === "list") return handleListKey(key);
  if (view === "board") return handleBoardKey(key);
  if (view === "calendar") return handleCalendarKey(key);

  return true;
}

function handleListKey(key: KeyEvent): boolean {
  if (key.name === "up") { moveUp(); return true; }
  if (key.name === "down") { moveDown(); return true; }
  if (key.char === " ") { toggleComplete(); return true; }
  if (key.char === "e") { openEditOverlay(); return true; }
  if (key.char === "d") { openDeleteOverlay(); return true; }
  if (key.name === "return") { openEditOverlay(); return true; }
  return true;
}

function handleBoardKey(key: KeyEvent): boolean {
  const col = boardColumn.peek();
  const cols = boardColumns.get();
  const scrolls = boardScroll.peek();

  if (key.name === "left") {
    if (col > 0) boardColumn.set(col - 1);
    return true;
  }
  if (key.name === "right") {
    if (col < 2) boardColumn.set(col + 1);
    return true;
  }
  if (key.name === "up") {
    const s = scrolls[col];
    scrolls[col] = scrollUp(s);
    boardScroll.set([...scrolls]);
    return true;
  }
  if (key.name === "down") {
    const s = scrolls[col];
    scrolls[col] = scrollDown(s);
    boardScroll.set([...scrolls]);
    return true;
  }
  if (key.char === " ") { toggleComplete(); return true; }
  return true;
}

function handleCalendarKey(key: KeyEvent): boolean {
  const day = calendarSelectedDay.peek();
  const month = calendarMonth.peek();
  const year = calendarYear.peek();
  const daysInMonth = new Date(year, month + 1, 0).getDate();

  if (key.name === "left") {
    if (day > 1) calendarSelectedDay.set(day - 1);
    return true;
  }
  if (key.name === "right") {
    if (day < daysInMonth) calendarSelectedDay.set(day + 1);
    return true;
  }
  if (key.name === "up") {
    if (day > 7) calendarSelectedDay.set(day - 7);
    return true;
  }
  if (key.name === "down") {
    if (day + 7 <= daysInMonth) calendarSelectedDay.set(day + 7);
    return true;
  }
  // Previous/next month
  if (key.char === "[") {
    if (month === 0) { calendarMonth.set(11); calendarYear.set(year - 1); }
    else calendarMonth.set(month - 1);
    calendarSelectedDay.set(1);
    return true;
  }
  if (key.char === "]") {
    if (month === 11) { calendarMonth.set(0); calendarYear.set(year + 1); }
    else calendarMonth.set(month + 1);
    calendarSelectedDay.set(1);
    return true;
  }
  return true;
}

// --- Form key handling ---
function handleFormKey(key: KeyEvent): boolean {
  if (key.name === "escape") {
    closeOverlay();
    return true;
  }

  const field = formField.peek();

  // Tab / Shift+Tab to switch fields
  if (key.name === "tab") {
    formField.set((field + 1) % FIELD_COUNT);
    return true;
  }

  // Enter = save
  if (key.name === "return") {
    const title = formTitle.peek();
    if (title.length > 0) {
      const isEdit = activeOverlay.peek() === "edit";
      if (isEdit) {
        const r = selectedReminder.get();
        if (r) {
          r.title = title;
          r.due = formDue.peek();
          r.priority = formPriority.peek();
          r.tags = formTags.peek().split(",").map(t => t.trim()).filter(Boolean);
          r.body = formBody.peek();
          fs.writeFileSync(r.filePath, serializeReminder(r));
          loadReminders();
        }
      } else {
        createReminder(
          title,
          formDue.peek(),
          formPriority.peek(),
          formTags.peek().split(",").map(t => t.trim()).filter(Boolean),
          formBody.peek(),
        );
      }
      closeOverlay();
    }
    return true;
  }

  // Field-specific editing
  if (field === 2) {
    // Priority: left/right to cycle
    if (key.name === "left" || key.name === "right") {
      const idx = PRIORITY_ORDER.indexOf(formPriority.peek());
      const dir = key.name === "right" ? 1 : -1;
      const newIdx = Math.max(0, Math.min(PRIORITY_ORDER.length - 1, idx + dir));
      formPriority.set(PRIORITY_ORDER[newIdx]);
      return true;
    }
  }

  // Text input for all fields
  if (key.name === "backspace") {
    const current = getFieldValue(field);
    if (current.length > 0) {
      setFieldValue(field, current.slice(0, -1));
    }
    return true;
  }

  if (key.char && !key.ctrl && !key.alt) {
    const current = getFieldValue(field);
    setFieldValue(field, current + key.char);
    return true;
  }

  return true;
}

function getFieldValue(field: number): string {
  switch (field) {
    case 0: return formTitle.peek();
    case 1: return formDue.peek();
    case 2: return formPriority.peek();
    case 3: return formTags.peek();
    case 4: return formBody.peek();
    default: return "";
  }
}

function setFieldValue(field: number, value: string): void {
  switch (field) {
    case 0: formTitle.set(value); break;
    case 1: formDue.set(value); break;
    case 3: formTags.set(value); break;
    case 4: formBody.set(value); break;
  }
}
