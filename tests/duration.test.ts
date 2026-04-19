import { describe, it, expect } from "vitest";
import { parseDuration, formatDuration } from "../src/duration.ts";

describe("parseDuration", () => {
  it("parses seconds", () => {
    expect(parseDuration("30s")).toBe(30_000);
    expect(parseDuration("1s")).toBe(1000);
    expect(parseDuration("0s")).toBe(0);
  });

  it("parses minutes", () => {
    expect(parseDuration("5m")).toBe(5 * 60_000);
    expect(parseDuration("1m")).toBe(60_000);
  });

  it("parses hours", () => {
    expect(parseDuration("2h")).toBe(2 * 3600_000);
  });

  it("parses days", () => {
    expect(parseDuration("7d")).toBe(7 * 86_400_000);
  });

  it("is case-insensitive on the unit", () => {
    expect(parseDuration("2H")).toBe(2 * 3600_000);
    expect(parseDuration("30S")).toBe(30_000);
  });

  it("tolerates internal whitespace and leading/trailing spaces", () => {
    expect(parseDuration("  5m  ")).toBe(5 * 60_000);
    expect(parseDuration("5 m")).toBe(5 * 60_000);
  });

  it("rejects compound forms", () => {
    // Keep grammar strict — `--older-than 1h30m` would be ambiguous
    // without documentation, so we only accept single-unit durations.
    expect(parseDuration("1h30m")).toBeNull();
    expect(parseDuration("1h 30m")).toBeNull();
  });

  it("rejects missing unit or missing number", () => {
    expect(parseDuration("5")).toBeNull();
    expect(parseDuration("s")).toBeNull();
    expect(parseDuration("")).toBeNull();
  });

  it("rejects unknown units", () => {
    expect(parseDuration("5y")).toBeNull(); // no years
    expect(parseDuration("5w")).toBeNull(); // no weeks
    expect(parseDuration("5ms")).toBeNull(); // no milliseconds
  });

  it("rejects negative and non-integer numbers", () => {
    expect(parseDuration("-5m")).toBeNull();
    expect(parseDuration("1.5h")).toBeNull();
  });
});

describe("formatDuration", () => {
  it("renders sub-minute durations in seconds", () => {
    expect(formatDuration(0)).toBe("0s");
    expect(formatDuration(1000)).toBe("1s");
    expect(formatDuration(45_000)).toBe("45s");
    expect(formatDuration(59_999)).toBe("59s");
  });

  it("renders sub-hour durations in minutes, dropping trailing :00", () => {
    expect(formatDuration(60_000)).toBe("1m");
    expect(formatDuration(65_000)).toBe("1m5s");
    expect(formatDuration(30 * 60_000)).toBe("30m");
  });

  it("renders sub-day durations in hours, dropping trailing :00", () => {
    expect(formatDuration(3600_000)).toBe("1h");
    expect(formatDuration(3600_000 + 12 * 60_000)).toBe("1h12m");
    expect(formatDuration(23 * 3600_000)).toBe("23h");
  });

  it("renders multi-day durations in days+hours", () => {
    expect(formatDuration(86_400_000)).toBe("1d");
    expect(formatDuration(86_400_000 + 2 * 3600_000)).toBe("1d2h");
    expect(formatDuration(3 * 86_400_000)).toBe("3d");
  });

  it("treats negative values as 0", () => {
    expect(formatDuration(-1000)).toBe("0s");
  });
});
