// Regression tests for the CellBuffer diff() renderer's handling of
// width-2 (emoji / CJK) characters. Filed by tui-sup while building
// agent-viz on top of @compoundingtech/pty/tui: a `📬` on an interactive tree
// would fossilize into `📬📬` when navigating shifted the glyph to a
// different column. Two failure modes:
//
//   1. `lastCol = c + 1` in diff() (buffer.ts:338) desyncs cursor
//      tracking by one after every wide-char emit, since the terminal
//      cursor lands at c+2 for a wide glyph. Downstream: adjacency
//      optimization mispredicts, cursor moves may land off by one.
//   2. The `nc.char === "" ⇒ skip` guard (buffer.ts:298) leaves the
//      right half of a prev-frame wide glyph on screen when the next
//      frame's placeholder-column cell happens to also be flagged as
//      a placeholder (via a genuine writeAnsi placeholder in `next`)
//      but the underlying terminal position still holds the fossil.
//
// The oracle: apply fullRender(prev) + diff(prev, next) to a fresh
// CellBuffer via writeAnsi and check every cell matches `next`. If the
// diff is correct, this round-trip is lossless.

import { describe, it, expect } from "vitest";
import xterm from "@xterm/headless";
import * as xtermSerialize from "@xterm/addon-serialize";
import * as xtermUnicode11 from "@xterm/addon-unicode11";
import { CellBuffer, fullRender, diff } from "../src/tui/buffer.ts";

/** Build a fresh xterm-headless terminal with Unicode 11 wide-char widths
 *  so emoji like `📬` (U+1F4EC) are treated as width-2 — matching what a
 *  modern host terminal (kitty, iTerm2, Ghostty) does. Without this, the
 *  default xterm.js width tables treat all astral-plane codepoints as
 *  width-1 and the emoji collapses, hiding our CellBuffer bugs behind an
 *  environment quirk. */
function makeTerminal(rows: number, cols: number): any {
  const t = new xterm.Terminal({ rows, cols, scrollback: 0, allowProposedApi: true });
  const u11 = new xtermUnicode11.Unicode11Addon();
  t.loadAddon(u11);
  t.unicode.activeVersion = "11";
  return t;
}

/** Serialize an xterm-headless terminal's viewport to a plain string so
 *  we can diff two terminals' rendered state. */
function terminalToString(term: any, cols: number, rows: number): string {
  const out: string[] = [];
  for (let r = 0; r < rows; r++) {
    const line = term.buffer.active.getLine(r);
    if (!line) { out.push(""); continue; }
    // translateToString(trimRight=false) gives us the raw viewport text
    // including trailing spaces — important because a fossil (a
    // wide-char right half surviving into the "space" region) would be
    // hidden by trimming.
    out.push(line.translateToString(false, 0, cols));
  }
  return out.join("\n");
}

/** Round-trip an intended `next` state through fullRender + diff against
 *  `prev` and assert an xterm-headless terminal ends up EQUIVALENT to
 *  one that got only `fullRender(next)`. This is the strong oracle:
 *  xterm-headless emulates real terminal semantics, including "narrow
 *  char overwriting a wide char's left half leaves the right half as a
 *  visible fossil until explicitly cleared." A pure CellBuffer receiver
 *  would miss that class of bug (its placeholder logic is symmetric
 *  with our own — so it papers over the divergence). */
function assertDiffLossless(prev: CellBuffer, next: CellBuffer): void {
  const initial = fullRender(prev);
  const d = diff(prev, next);

  const applied = makeTerminal(prev.rows, prev.cols);
  applied.loadAddon(new xtermSerialize.SerializeAddon());
  applied.write(initial);
  applied.write(d);

  const oracle = makeTerminal(prev.rows, prev.cols);
  oracle.loadAddon(new xtermSerialize.SerializeAddon());
  oracle.write(fullRender(next));

  return new Promise<void>((resolve, reject) => {
    // xterm.write is async. Both writes above enqueue behind a shared
    // parser microtask; give them a tick to drain, then compare.
    setTimeout(() => {
      const appliedText = terminalToString(applied, prev.cols, prev.rows);
      const oracleText = terminalToString(oracle, prev.cols, prev.rows);
      applied.dispose();
      oracle.dispose();
      if (appliedText !== oracleText) {
        reject(new Error(
          `Diff drift:\n` +
          `  applied (prev + diff):\n    ${JSON.stringify(appliedText)}\n` +
          `  oracle  (fullRender(next)):\n    ${JSON.stringify(oracleText)}\n`,
        ));
      } else {
        resolve();
      }
    }, 20);
  }) as any;
}

describe("CellBuffer diff() — wide-char (2-cell) glyph handling", () => {
  it("does not fossilize a wide char that shifts column between frames", async () => {
    // Prev frame: `📬` at column 2 (occupies 2,3). Rest is space.
    // Next frame: `📬` at column 5 (occupies 5,6). Column 2 must be
    // fully cleared — both the left half AND the fossil right half at
    // column 3.
    const prev = new CellBuffer(1, 10);
    prev.writeAnsi("  📬      ");

    const next = new CellBuffer(1, 10);
    next.writeAnsi("     📬   ");

    await assertDiffLossless(prev, next);
  });

  it("does not fossilize when a wide char is replaced by a narrow char at the same start column", async () => {
    // Prev: `📬` at (0,2). Next: `A` at (0,2). Column 3 (formerly the
    // wide-char placeholder) must be cleared to a space, not left with
    // the fossil right half of `📬`.
    const prev = new CellBuffer(1, 10);
    prev.writeAnsi("  📬      ");

    const next = new CellBuffer(1, 10);
    next.writeAnsi("  A       ");

    await assertDiffLossless(prev, next);
  });

  it("does not fossilize when a wide char shifts one column right", async () => {
    // Prev: `📬` at (0,2). Next: `📬` at (0,3). The 1-column shift means
    // prev's left half (col 2) needs clearing, and prev's right half
    // (col 3) is now the left half of next's `📬`.
    const prev = new CellBuffer(1, 10);
    prev.writeAnsi("  📬      ");

    const next = new CellBuffer(1, 10);
    next.writeAnsi("   📬     ");

    await assertDiffLossless(prev, next);
  });

  it("does not fossilize two adjacent wide chars when both shift together", async () => {
    // Prev: `📬📭` at (0,2)-(0,5). Next: shifted to (0,4)-(0,7). All four
    // cells in the prev range need clearing except where they overlap
    // with the next range.
    const prev = new CellBuffer(1, 12);
    prev.writeAnsi("  📬📭      ");

    const next = new CellBuffer(1, 12);
    next.writeAnsi("    📬📭    ");

    await assertDiffLossless(prev, next);
  });

  it("preserves alignment when a wide char is followed by digits (tui-sup's original bug)", async () => {
    // The exact shape from tui-sup's report: emoji + digits, where the
    // digits after must land in the correct column even after the emoji
    // shifts. Before the fix, prev's 📬 at col 2 and next's 📬 at col 4
    // followed by `99` would result in `📬📬99` visible in the terminal
    // instead of the intended `    📬99`.
    const prev = new CellBuffer(1, 14);
    prev.writeAnsi("  📬  1       ");

    const next = new CellBuffer(1, 14);
    next.writeAnsi("    📬 99     ");

    await assertDiffLossless(prev, next);
  });

  it("cursor tracking stays aligned across a run of wide chars", async () => {
    // Multiple wide chars in a row exercise the `lastCol = c + 1` vs.
    // `c + charWidth` code path. Without the fix, the adjacency check
    // fires an unnecessary cursor move after each wide char (not a
    // correctness bug per se, but the compounded off-by-one interacts
    // with the placeholder-skip in the fossil scenarios above).
    const prev = new CellBuffer(1, 12);
    prev.writeAnsi("            ");

    const next = new CellBuffer(1, 12);
    next.writeAnsi("📬📭📫✉XY");

    await assertDiffLossless(prev, next);
  });

  it("clears wide-char right halves when the surrounding row also mutates", async () => {
    // Realistic tui-nav shape: a row where a status column changes AND
    // an emoji shifts. Exercises the interaction between the two fix
    // sites (lastCol tracking + placeholder-clear on shift).
    const prev = new CellBuffer(1, 20);
    prev.writeAnsi(" • 📬  1    09m │  ");

    const next = new CellBuffer(1, 20);
    next.writeAnsi(" ▸  📬 99   41m │  ");

    await assertDiffLossless(prev, next);
  });

  it("handles a wide char moving into a position that was a narrow char", async () => {
    const prev = new CellBuffer(1, 10);
    prev.writeAnsi("  AB      ");

    const next = new CellBuffer(1, 10);
    next.writeAnsi("  📬      ");

    await assertDiffLossless(prev, next);
  });

  it("handles a wide char being removed entirely (next has only narrows)", async () => {
    const prev = new CellBuffer(1, 10);
    prev.writeAnsi("  📬  1   ");

    const next = new CellBuffer(1, 10);
    next.writeAnsi("  xx  1   ");

    await assertDiffLossless(prev, next);
  });

  it("handles narrow → wide → narrow toggle at the same start column", async () => {
    // Round-trip: narrow to wide to narrow at the same column. Both
    // transitions must clear cleanly.
    const start = new CellBuffer(1, 10);
    start.writeAnsi("  AB      ");

    const middle = new CellBuffer(1, 10);
    middle.writeAnsi("  📬      ");

    const end = new CellBuffer(1, 10);
    end.writeAnsi("  CD      ");

    await assertDiffLossless(start, middle);
    await assertDiffLossless(middle, end);
  });
});
