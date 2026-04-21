// Date picker widget — calendar grid + time shift helpers + overlay factory.
//
// State-first: the consumer owns a `DatePickerState` signal and passes it in
// on every render. Key handling is a pure function that returns a new state
// (or null when the key wasn't consumed so the caller can escape/submit).
//
// Promoted from demos/reminders/tui/widgets/date-picker.ts with a cleaner API.

import { canvas, row, text, column, panel } from "../builders.ts";
import type { UINode } from "../nodes.ts";
import type { KeyEvent } from "../input.ts";

export interface DatePickerState {
  year: number;
  month: number;       // 0-11
  day: number;         // 1-31
  hour: number;        // 0-23
  minute: number;      // 0-59
}

const DAY_HEADERS = ["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"];

export const MONTH_NAMES = [
  "January", "February", "March", "April", "May", "June",
  "July", "August", "September", "October", "November", "December",
];

export function daysInMonth(year: number, month: number): number {
  return new Date(year, month + 1, 0).getDate();
}

/** Anchored "now" as a DatePickerState, useful as a default. */
export function datePickerFromDate(d: Date): DatePickerState {
  return {
    year: d.getFullYear(),
    month: d.getMonth(),
    day: d.getDate(),
    hour: d.getHours(),
    minute: d.getMinutes(),
  };
}

/** Clamp the day into the range for the current year/month. */
export function clampDay(state: DatePickerState): DatePickerState {
  const max = daysInMonth(state.year, state.month);
  if (state.day > max) return { ...state, day: max };
  if (state.day < 1) return { ...state, day: 1 };
  return state;
}

export function shiftDay(state: DatePickerState, delta: number): DatePickerState {
  const d = new Date(state.year, state.month, state.day);
  d.setDate(d.getDate() + delta);
  return {
    ...state,
    year: d.getFullYear(),
    month: d.getMonth(),
    day: d.getDate(),
  };
}

export function shiftMonth(state: DatePickerState, delta: number): DatePickerState {
  const d = new Date(state.year, state.month + delta, 1);
  return clampDay({
    ...state,
    year: d.getFullYear(),
    month: d.getMonth(),
  });
}

export function shiftTime(state: DatePickerState, unit: "h" | "m", delta: number): DatePickerState {
  if (unit === "h") {
    return { ...state, hour: (state.hour + delta + 24) % 24 };
  }
  return { ...state, minute: (state.minute + delta + 60) % 60 };
}

export function toDate(state: DatePickerState): Date {
  return new Date(state.year, state.month, state.day, state.hour, state.minute, 0, 0);
}

/** Default key bindings. Returns a new state when the key was consumed,
 *  or `null` when the caller should handle it (escape / enter to commit). */
export function handleDatePickerKey(
  state: DatePickerState,
  key: KeyEvent,
): DatePickerState | null {
  switch (key.name) {
    case "up":    return shiftDay(state, -7);
    case "down":  return shiftDay(state, 7);
    case "left":  return shiftDay(state, -1);
    case "right": return shiftDay(state, 1);
    case "[":     return shiftMonth(state, -1);
    case "]":     return shiftMonth(state, 1);
    case "h":     return shiftTime(state, "h", -1);
    case "H":     return shiftTime(state, "h", 1);
    case "m":     return shiftTime(state, "m", -5);
    case "M":     return shiftTime(state, "m", 5);
  }
  return null;
}

/** Render a month grid as a Canvas node, selection highlighted. */
export function calendarCanvas(state: DatePickerState): UINode {
  const first = new Date(state.year, state.month, 1);
  const firstDow = first.getDay();
  const max = daysInMonth(state.year, state.month);

  // 6 week rows worst case + 1 header row = 7 rows.
  return canvas((ctx) => {
    const cellW = 4;
    for (let d = 0; d < 7; d++) {
      ctx.write(d * cellW, 0, DAY_HEADERS[d], "accent", undefined, true);
    }
    let rowIdx = 1;
    let col = firstDow;
    for (let day = 1; day <= max; day++) {
      const x = col * cellW;
      const y = rowIdx;
      const isSelected = day === state.day;
      if (isSelected) {
        ctx.write(x, y, String(day).padStart(2), "accent", "accent", true);
      } else {
        ctx.write(x, y, String(day).padStart(2), "primary");
      }
      col++;
      if (col > 6) { col = 0; rowIdx++; }
    }
  }, { height: 7 });
}

/** Full overlay body: month/year header, calendar grid, time line, hints.
 *  Returns a UINode[] suitable for putting inside an `overlay()` screen. */
export function datePickerBody(state: DatePickerState): UINode[] {
  const pad2 = (n: number) => String(n).padStart(2, "0");
  const timeStr = `${pad2(state.hour)}:${pad2(state.minute)}`;
  const heading = `${MONTH_NAMES[state.month]} ${state.year}`;
  return [
    row(text(heading, "accent", { bold: true })),
    calendarCanvas(state),
    row(text(`time  ${timeStr}`, "muted")),
    row(text("\u2190\u2192\u2191\u2193 day    [ ] month    h/H hour    m/M \u00b15 min    enter ok    esc cancel", "muted", { dim: true })),
  ];
}

/** Convenience: wrap the body in a panel — useful when embedding inside an
 *  app's own overlay render() rather than as a full-fledged screen. */
export function datePickerPanel(state: DatePickerState, title = "pick a date"): UINode {
  return panel(title, datePickerBody(state));
}
