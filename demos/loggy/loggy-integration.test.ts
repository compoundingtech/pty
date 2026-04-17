// End-to-end integration tests for loggy: spawn the demo as a real PTY
// process, let it wrap a deterministic emitter fixture, and drive the TUI
// via keystrokes.

import { describe, it, expect, afterEach } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { Session } from "../../src/testing/index.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const mainScript = path.join(__dirname, "main.ts");
const emitter = path.join(__dirname, "fixtures", "emitter.js");
const firehose = path.join(__dirname, "fixtures", "firehose.js");

let session: Session | null = null;
const tmpPaths: string[] = [];

async function startLoggy(extraArgs: readonly string[] = [], rows = 24, cols = 100): Promise<Session> {
  return spawnLoggyWrapping(emitter, extraArgs, rows, cols);
}

async function spawnLoggyWrapping(
  childScript: string,
  extraArgs: readonly string[] = [],
  rows = 24,
  cols = 100,
): Promise<Session> {
  const args = [
    "--experimental-strip-types",
    "--no-warnings",
    mainScript,
    ...extraArgs,
    "--",
    process.execPath,
    childScript,
  ];
  session = Session.spawn("node", args, { rows, cols, env: { TERM: "xterm-256color" } });
  await session.waitForText("loggy", 15_000);
  return session;
}

afterEach(async () => {
  if (session) {
    await session.close();
    session = null;
  }
  for (const p of tmpPaths) {
    try { fs.unlinkSync(p); } catch {}
  }
  tmpPaths.length = 0;
});

function tmpFile(suffix: string): string {
  const p = path.join(os.tmpdir(), `loggy-test-${Date.now()}-${Math.random().toString(36).slice(2, 6)}-${suffix}`);
  tmpPaths.push(p);
  return p;
}

// ===========================================================================

describe("loggy integration", () => {
  it("renders the rounded panel with command in the title", async () => {
    const s = await startLoggy();
    const ss = await s.waitForText("loggy \u2014", 10_000);
    // Rounded corners come from panel's default box style.
    expect(ss.text).toMatch(/[╭╮╰╯]/);
  }, 20_000);

  it("shows both stdout and stderr lines with source tags", async () => {
    const s = await startLoggy();
    await s.waitForText("stdout one", 10_000);
    await s.waitForText("stderr one", 10_000);
    const ss = s.screenshot();
    // Both labels appear in the rendered list (our format is "● out" / "● err").
    expect(ss.text).toContain("out");
    expect(ss.text).toContain("err");
    expect(ss.text).toContain("stdout one");
    expect(ss.text).toContain("stderr one");
  }, 20_000);

  it("'o' filters to stdout only", async () => {
    const s = await startLoggy();
    await s.waitForText("stderr one", 10_000);
    s.press("o");
    await s.waitFor((ss) => !ss.text.includes("stderr one"), 5_000, "stderr hidden");
    const ss = s.screenshot();
    expect(ss.text).toContain("stdout one");
    expect(ss.text).not.toContain("stderr one");
  }, 20_000);

  it("'e' filters to stderr only", async () => {
    const s = await startLoggy();
    await s.waitForText("stdout one", 10_000);
    s.press("e");
    await s.waitFor((ss) => !ss.text.includes("stdout one"), 5_000, "stdout hidden");
    const ss = s.screenshot();
    expect(ss.text).not.toContain("stdout one");
    expect(ss.text).toContain("stderr one");
  }, 20_000);

  it("'b' restores both streams after a filter", async () => {
    const s = await startLoggy();
    await s.waitForText("stderr one", 10_000);
    s.press("o");
    await s.waitFor((ss) => !ss.text.includes("stderr one"), 5_000, "stderr hidden");
    s.press("b");
    await s.waitFor((ss) => ss.text.includes("stderr one") && ss.text.includes("stdout one"), 5_000, "both restored");
  }, 20_000);

  it("'/' opens search and typed query filters to matching lines", async () => {
    const s = await startLoggy();
    await s.waitForText("ERROR: something broke", 10_000);
    s.type("/");
    // Search prompt appears in the status bar.
    await s.waitForText("/", 5_000);
    s.type("ERROR");
    await s.waitFor(
      (ss) => ss.text.includes("ERROR") && !ss.text.includes("stdout one"),
      5_000,
      "only ERROR line visible",
    );
    const ss = s.screenshot();
    expect(ss.text).toContain("ERROR: something broke");
    expect(ss.text).not.toContain("stdout one");
    expect(ss.text).not.toContain("stderr one");
  }, 20_000);

  it("escape clears the active search", async () => {
    const s = await startLoggy();
    await s.waitForText("ERROR:", 10_000);
    s.type("/");
    s.type("ERROR");
    await s.waitFor((ss) => !ss.text.includes("stdout one"), 5_000, "search active");
    s.press("escape");
    await s.waitFor((ss) => ss.text.includes("stdout one"), 5_000, "search cleared");
  }, 20_000);

  it("child status shows 'running' while child is alive", async () => {
    const s = await startLoggy();
    await s.waitForText("stdout one", 10_000);
    const ss = s.screenshot();
    expect(ss.text).toContain("running");
  }, 20_000);

  it("'q' alone does NOT quit (requires Ctrl+C twice now)", async () => {
    const s = await startLoggy();
    await s.waitForText("stdout one", 10_000);
    s.type("q");
    // Give it a moment; loggy should still be alive and rendering
    await new Promise((r) => setTimeout(r, 300));
    const ss = s.screenshot();
    expect(ss.text).toContain("loggy");
    expect(ss.text).toContain("running");
  }, 20_000);

  it("first Ctrl+C arms a confirmation prompt in the footer", async () => {
    const s = await startLoggy();
    await s.waitForText("stdout one", 10_000);
    s.press("ctrl+c");
    await s.waitForText("Press Ctrl+C again to quit", 3_000);
    const ss = s.screenshot();
    expect(ss.text).toContain("Press Ctrl+C again");
  }, 20_000);

  it("stays interactive under a firehose of output (no event-loop starvation)", async () => {
    // Regression test for an O(n²) append bug: wrapping a child that
    // emits 20,000 lines would lock up the TUI. After the store fix
    // (mutable buffer + version signal) and render throttling, filter
    // keys should still register and redraw within a reasonable window.
    const s = await spawnLoggyWrapping(firehose);

    // Wait for late output so we know the firehose is well underway.
    await s.waitForText("line 49", 15_000);

    // The TUI must still register keystrokes while the firehose is
    // streaming / has just finished — press a filter key and confirm
    // the footer reflects it. Before the fix, the filter keypress
    // would not register because the event loop was saturated.
    s.press("o");
    await s.waitFor(
      (ss) => ss.text.includes("[O]ut"),
      5_000,
      "filter=out registered under load",
    );

    // Every 13th line was routed to stderr (seq 0, 13, 26, …). Switch
    // to err-only and verify one of those shows up.
    s.press("e");
    await s.waitFor(
      (ss) => ss.text.includes("[E]rr"),
      5_000,
      "filter=err registered under load",
    );
  }, 30_000);

  it("any other key cancels the pending quit", async () => {
    const s = await startLoggy();
    await s.waitForText("stdout one", 10_000);
    s.press("ctrl+c");
    await s.waitForText("Press Ctrl+C again", 3_000);
    s.press("b"); // filter key, but any key cancels
    await s.waitFor(
      (ss) => !ss.text.includes("Press Ctrl+C again") && ss.text.includes("quit"),
      3_000,
      "prompt cleared, normal footer back",
    );
  }, 20_000);

  it("--out tees stdout lines to the given file", async () => {
    const outFile = tmpFile("out.log");
    const s = await startLoggy(["--out", outFile]);
    // Wait for the last emitted line so we know the child has written
    // everything before we read the file.
    await s.waitForText("stderr four", 10_000);
    const content = fs.readFileSync(outFile, "utf-8");
    expect(content).toContain("stdout one");
    expect(content).toContain("stdout four");
    // stderr lines must NOT be in the --out file
    expect(content).not.toContain("stderr one");
    expect(content).not.toContain("ERROR: something broke");
  }, 20_000);

  it("--err tees stderr lines to the given file", async () => {
    const errFile = tmpFile("err.log");
    const s = await startLoggy(["--err", errFile]);
    await s.waitForText("stderr four", 10_000);
    const content = fs.readFileSync(errFile, "utf-8");
    expect(content).toContain("stderr one");
    expect(content).toContain("ERROR: something broke");
    expect(content).not.toContain("stdout one");
  }, 20_000);

  it("--log writes combined tagged output", async () => {
    const logFile = tmpFile("combined.log");
    const s = await startLoggy(["--log", logFile]);
    await s.waitForText("stderr four", 10_000);
    const content = fs.readFileSync(logFile, "utf-8");
    // Each line has the [HH:MM:SS.mmm source] prefix
    const lines = content.trim().split("\n");
    for (const line of lines) {
      expect(line).toMatch(/^\[\d{2}:\d{2}:\d{2}\.\d{3} (out|err)\] /);
    }
    // All expected content present
    expect(content).toMatch(/ out\] stdout one/);
    expect(content).toMatch(/ err\] stderr one/);
    expect(content).toMatch(/ err\] ERROR: something broke/);
  }, 20_000);
});
