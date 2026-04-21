import { describe, it, expect } from "vitest";
import {
  shiftDay, shiftMonth, shiftTime, clampDay, daysInMonth,
  datePickerFromDate, toDate, handleDatePickerKey,
  type DatePickerState,
} from "../src/tui/widgets/date-picker.ts";
import type { KeyEvent } from "../src/tui/input.ts";

function k(name: string): KeyEvent {
  return { name, ctrl: false, alt: false, shift: false };
}

const apr20 = (): DatePickerState => ({ year: 2026, month: 3, day: 20, hour: 9, minute: 0 });

describe("date-picker math", () => {
  it("daysInMonth handles leap years", () => {
    expect(daysInMonth(2024, 1)).toBe(29);
    expect(daysInMonth(2025, 1)).toBe(28);
    expect(daysInMonth(2026, 1)).toBe(28);
  });

  it("shiftDay rolls over month and year", () => {
    const jan30 = { year: 2026, month: 0, day: 30, hour: 0, minute: 0 };
    const plus3 = shiftDay(jan30, 3);
    expect(plus3).toEqual({ year: 2026, month: 1, day: 2, hour: 0, minute: 0 });

    const dec31 = { year: 2026, month: 11, day: 31, hour: 0, minute: 0 };
    const plus1 = shiftDay(dec31, 1);
    expect(plus1).toEqual({ year: 2027, month: 0, day: 1, hour: 0, minute: 0 });
  });

  it("shiftMonth clamps the day when overflowing (Jan 31 -> Feb 28)", () => {
    const jan31 = { year: 2026, month: 0, day: 31, hour: 0, minute: 0 };
    const feb = shiftMonth(jan31, 1);
    expect(feb.month).toBe(1);
    expect(feb.day).toBe(28); // 2026 is not a leap year
  });

  it("shiftTime wraps hour across midnight", () => {
    const base = apr20();
    expect(shiftTime(base, "h", -10)).toEqual({ ...base, hour: 23 });
    expect(shiftTime(base, "h", 25)).toEqual({ ...base, hour: 10 });
  });

  it("shiftTime wraps minute across the hour", () => {
    const base = apr20();
    expect(shiftTime(base, "m", -5)).toEqual({ ...base, minute: 55 });
    expect(shiftTime(base, "m", 65)).toEqual({ ...base, minute: 5 });
  });

  it("clampDay clamps up and down", () => {
    expect(clampDay({ year: 2026, month: 1, day: 30, hour: 0, minute: 0 }).day).toBe(28);
    expect(clampDay({ year: 2026, month: 1, day: 0, hour: 0, minute: 0 }).day).toBe(1);
  });

  it("round-trips through Date", () => {
    const s = apr20();
    const d = toDate(s);
    expect(datePickerFromDate(d)).toEqual(s);
  });
});

describe("handleDatePickerKey default bindings", () => {
  it("arrow left/right move by one day, up/down by a week", () => {
    const s = apr20();
    expect(handleDatePickerKey(s, k("right"))?.day).toBe(21);
    expect(handleDatePickerKey(s, k("left"))?.day).toBe(19);
    expect(handleDatePickerKey(s, k("down"))?.day).toBe(27);
    expect(handleDatePickerKey(s, k("up"))?.day).toBe(13);
  });

  it("[ and ] shift month", () => {
    const s = apr20();
    expect(handleDatePickerKey(s, k("["))?.month).toBe(2);
    expect(handleDatePickerKey(s, k("]"))?.month).toBe(4);
  });

  it("h/H shift hour; m/M shift minute by 5", () => {
    const s = apr20();
    expect(handleDatePickerKey(s, k("h"))?.hour).toBe(8);
    expect(handleDatePickerKey(s, k("H"))?.hour).toBe(10);
    expect(handleDatePickerKey(s, k("M"))?.minute).toBe(5);
    expect(handleDatePickerKey(s, k("m"))?.minute).toBe(55);
  });

  it("returns null for unhandled keys", () => {
    const s = apr20();
    expect(handleDatePickerKey(s, k("x"))).toBeNull();
    expect(handleDatePickerKey(s, k("return"))).toBeNull();
    expect(handleDatePickerKey(s, k("escape"))).toBeNull();
  });
});
