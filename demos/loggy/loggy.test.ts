import { describe, it, expect } from "vitest";
import { parseArgs, UsageError, DEFAULT_SCROLLBACK } from "./cli.ts";
import { createStore, applyView, type LogEntry } from "./store.ts";
import { formatTaggedLine } from "./sinks.ts";

// ============================================================
// cli.parseArgs
// ============================================================

describe("parseArgs", () => {
  it("parses command + args with no flags", () => {
    const p = parseArgs(["npm", "run", "build"]);
    expect(p.command).toBe("npm");
    expect(p.args).toEqual(["run", "build"]);
    expect(p.out).toBeNull();
    expect(p.err).toBeNull();
    expect(p.log).toBeNull();
    expect(p.noColor).toBe(false);
    expect(p.scrollback).toBe(DEFAULT_SCROLLBACK);
  });

  it("treats first non-flag positional as the command", () => {
    const p = parseArgs(["--out", "/tmp/o.log", "my-cmd", "--my-flag"]);
    expect(p.command).toBe("my-cmd");
    expect(p.args).toEqual(["--my-flag"]);
    expect(p.out).toBe("/tmp/o.log");
  });

  it("supports -- as a separator", () => {
    const p = parseArgs(["--log", "/tmp/a.log", "--", "tsc", "--watch"]);
    expect(p.command).toBe("tsc");
    expect(p.args).toEqual(["--watch"]);
    expect(p.log).toBe("/tmp/a.log");
  });

  it("parses --out, --err, and --log paths", () => {
    const p = parseArgs(["--out", "o", "--err", "e", "--log", "l", "cat"]);
    expect(p.out).toBe("o");
    expect(p.err).toBe("e");
    expect(p.log).toBe("l");
  });

  it("parses --no-color", () => {
    const p = parseArgs(["--no-color", "cat"]);
    expect(p.noColor).toBe(true);
  });

  it("parses --scrollback with a positive integer", () => {
    const p = parseArgs(["--scrollback", "500", "cat"]);
    expect(p.scrollback).toBe(500);
  });

  it("rejects --scrollback with non-numeric or non-positive values", () => {
    expect(() => parseArgs(["--scrollback", "abc", "cat"])).toThrow(UsageError);
    expect(() => parseArgs(["--scrollback", "0", "cat"])).toThrow(UsageError);
    expect(() => parseArgs(["--scrollback", "-5", "cat"])).toThrow(UsageError);
  });

  it("throws on missing command", () => {
    expect(() => parseArgs([])).toThrow(UsageError);
    expect(() => parseArgs(["--no-color"])).toThrow(UsageError);
  });

  it("throws on unknown flag", () => {
    expect(() => parseArgs(["--nope", "cat"])).toThrow(/Unknown flag/);
  });

  it("throws a special help marker on -h / --help", () => {
    expect(() => parseArgs(["--help"])).toThrow(/__help__/);
    expect(() => parseArgs(["-h"])).toThrow(/__help__/);
  });

  it("throws when --out is given without a value", () => {
    expect(() => parseArgs(["--out"])).toThrow(/requires a path/);
  });
});

// ============================================================
// store — ring buffer + reactive entries
// ============================================================

describe("createStore", () => {
  it("appends entries with monotonic seq", () => {
    const s = createStore(100);
    s.append("out", "a");
    s.append("err", "b");
    s.append("out", "c");
    const all = s.entries.get();
    expect(all.map((e) => e.seq)).toEqual([0, 1, 2]);
    expect(all.map((e) => e.source)).toEqual(["out", "err", "out"]);
    expect(all.map((e) => e.line)).toEqual(["a", "b", "c"]);
  });

  it("drops oldest when scrollback limit is reached; seq stays monotonic", () => {
    const s = createStore(3);
    s.append("out", "1");
    s.append("out", "2");
    s.append("out", "3");
    s.append("out", "4");
    s.append("out", "5");
    const all = s.entries.get();
    expect(all.length).toBe(3);
    expect(all.map((e) => e.line)).toEqual(["3", "4", "5"]);
    expect(all.map((e) => e.seq)).toEqual([2, 3, 4]);
  });

  it("rejects non-positive scrollback", () => {
    expect(() => createStore(0)).toThrow();
    expect(() => createStore(-1)).toThrow();
  });
});

// ============================================================
// applyView — filter + search
// ============================================================

function mk(source: "out" | "err", line: string, seq = 0): LogEntry {
  return { seq, ts: 0, source, line };
}

describe("applyView", () => {
  const entries: LogEntry[] = [
    mk("out", "hello world", 0),
    mk("err", "Oh no an error", 1),
    mk("out", "finished", 2),
    mk("err", "ERROR 42", 3),
  ];

  it("filter=both passes everything", () => {
    expect(applyView(entries, "both", "").length).toBe(4);
  });

  it("filter=out keeps only stdout", () => {
    const v = applyView(entries, "out", "");
    expect(v.map((e) => e.line)).toEqual(["hello world", "finished"]);
  });

  it("filter=err keeps only stderr", () => {
    const v = applyView(entries, "err", "");
    expect(v.map((e) => e.line)).toEqual(["Oh no an error", "ERROR 42"]);
  });

  it("search is case-insensitive substring", () => {
    expect(applyView(entries, "both", "error").map((e) => e.line))
      .toEqual(["Oh no an error", "ERROR 42"]);
    expect(applyView(entries, "both", "HELLO").map((e) => e.line))
      .toEqual(["hello world"]);
  });

  it("search composes with filter", () => {
    expect(applyView(entries, "err", "42").map((e) => e.line))
      .toEqual(["ERROR 42"]);
    expect(applyView(entries, "out", "error")).toEqual([]);
  });

  it("search ignores ANSI escape sequences in the line", () => {
    const colored: LogEntry[] = [
      mk("out", "\x1b[31mred error\x1b[0m", 0),
      mk("out", "plain error", 1),
    ];
    const v = applyView(colored, "both", "red error");
    expect(v).toHaveLength(1);
    expect(v[0].line).toContain("\x1b[31m"); // ANSI preserved in output
  });

  it("empty search is a no-op", () => {
    expect(applyView(entries, "both", "   ").length).toBe(4);
    expect(applyView(entries, "both", "").length).toBe(4);
  });
});

// ============================================================
// sinks.formatTaggedLine
// ============================================================

describe("formatTaggedLine", () => {
  it("produces [HH:MM:SS.mmm source] line format", () => {
    // Fix the timestamp to avoid TZ drift: 2026-04-17T15:30:45.123 local —
    // just check the structure, not the exact hour (depends on TZ).
    const ts = Date.parse("2026-04-17T15:30:45.123");
    const line = formatTaggedLine(ts, "out", "hello");
    expect(line).toMatch(/^\[\d{2}:\d{2}:\d{2}\.\d{3} out\] hello$/);
  });

  it("uses the right source tag for err", () => {
    const line = formatTaggedLine(Date.now(), "err", "boom");
    expect(line).toMatch(/ err\] boom$/);
  });

  it("zero-pads small time components", () => {
    const ts = Date.parse("2026-04-17T01:02:03.004");
    const line = formatTaggedLine(ts, "out", "x");
    expect(line).toMatch(/^\[\d{2}:02:03\.004 out\] x$/);
  });
});
