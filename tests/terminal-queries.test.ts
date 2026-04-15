import { describe, it, expect, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import { Session } from "../src/testing/index.ts";
import { stripTerminalQueries } from "../src/server.ts";

// Isolate from real session directory
const testSessionDir = fs.mkdtempSync(os.tmpdir() + "/pty-queries-");
process.env.PTY_SESSION_DIR = testSessionDir;
afterAll(() => {
  return new Promise((resolve) => {
    setTimeout(() => {
      try { fs.rmSync(testSessionDir, { recursive: true, force: true }); } catch {}
      resolve(undefined);
    }, 500);
  });
});

describe("stripTerminalQueries", () => {
  it("strips OSC 10 query with BEL terminator", () => {
    expect(stripTerminalQueries("\x1b]10;?\x07")).toBe("");
  });

  it("strips OSC 10 query with ST terminator", () => {
    expect(stripTerminalQueries("\x1b]10;?\x1b\\")).toBe("");
  });

  it("strips OSC 11 query with BEL terminator", () => {
    expect(stripTerminalQueries("\x1b]11;?\x07")).toBe("");
  });

  it("strips OSC 11 query with ST terminator", () => {
    expect(stripTerminalQueries("\x1b]11;?\x1b\\")).toBe("");
  });

  it("strips OSC 4 palette query with BEL", () => {
    expect(stripTerminalQueries("\x1b]4;7;?\x07")).toBe("");
    expect(stripTerminalQueries("\x1b]4;255;?\x07")).toBe("");
  });

  it("strips OSC 4 palette query with ST", () => {
    expect(stripTerminalQueries("\x1b]4;0;?\x1b\\")).toBe("");
  });

  it("strips DA1 query", () => {
    expect(stripTerminalQueries("\x1b[c")).toBe("");
  });

  it("strips DA2 query", () => {
    expect(stripTerminalQueries("\x1b[>c")).toBe("");
  });

  it("strips DSR cursor position query", () => {
    expect(stripTerminalQueries("\x1b[6n")).toBe("");
  });

  it("strips XTVERSION query", () => {
    expect(stripTerminalQueries("\x1b[>0q")).toBe("");
  });

  it("preserves normal text", () => {
    expect(stripTerminalQueries("hello world")).toBe("hello world");
  });

  it("preserves normal ANSI sequences", () => {
    const ansi = "\x1b[1;31mred bold\x1b[0m";
    expect(stripTerminalQueries(ansi)).toBe(ansi);
  });

  it("strips queries embedded in normal output", () => {
    expect(stripTerminalQueries("before\x1b]11;?\x07after")).toBe("beforeafter");
  });

  it("strips multiple queries in one chunk", () => {
    const data = "\x1b]10;?\x07\x1b]11;?\x07\x1b[c";
    expect(stripTerminalQueries(data)).toBe("");
  });

  it("preserves OSC sequences that are not queries", () => {
    // OSC 0 (set title) should pass through
    const title = "\x1b]0;my title\x07";
    expect(stripTerminalQueries(title)).toBe(title);
  });

  it("does not strip OSC 10/11 set commands (only queries)", () => {
    // Setting foreground color (not a query — no "?")
    const set = "\x1b]10;rgb:ffff/0000/0000\x07";
    expect(stripTerminalQueries(set)).toBe(set);
  });
});

describe("terminal query responses", () => {
  it("responds to DA1 (ESC[c)", async () => {
    // Fish uses this — already tested in shells.test.ts, but verify directly
    const session = await Session.server("sh", ["-c", "printf '\\033[c'; cat"], {
      rows: 24,
      cols: 80,
    });
    await session.attach();
    // The response (\x1b[?62;22c) goes to the PTY's stdin, cat echoes it
    await session.waitForText("62;22", 3000);
    await session.close();
  }, 15000);

  it("responds to OSC 11 background color query", async () => {
    // less sends this — the response should appear as echoed output from cat
    const session = await Session.server("sh", ["-c", "printf '\\033]11;?\\033\\\\'; cat"], {
      rows: 24,
      cols: 80,
    });
    await session.attach();
    // The response contains rgb:0000/0000/0000
    await session.waitForText("0000/0000/0000", 3000);
    await session.close();
  }, 15000);

  it("responds to OSC 10 foreground color query", async () => {
    const session = await Session.server("sh", ["-c", "printf '\\033]10;?\\033\\\\'; cat"], {
      rows: 24,
      cols: 80,
    });
    await session.attach();
    await session.waitForText("c0c0/c0c0/c0c0", 3000);
    await session.close();
  }, 15000);

  it("responds to DSR cursor position query (ESC[6n)", async () => {
    const session = await Session.server("sh", ["-c", "printf '\\033[6n'; cat"], {
      rows: 24,
      cols: 80,
    });
    await session.attach();
    // Response: ESC[row;colR — cursor should be at row 1 or 2
    await session.waitForText(";", 3000);
    const ss = session.screenshot();
    expect(ss.text).toMatch(/\d+;\d+R/);
    await session.close();
  }, 15000);

  it("responds to DA2 (ESC[>c)", async () => {
    const session = await Session.server("sh", ["-c", "printf '\\033[>c'; cat"], {
      rows: 24,
      cols: 80,
    });
    await session.attach();
    // Response: ESC[>0;382;0c
    await session.waitForText("382", 3000);
    await session.close();
  }, 15000);
});
