// codeBlock — an SRCL-style numbered code/log block.
//
// SRCL's CodeBlock renders each line as `[right-aligned line number] content`
// with a subtle background. This mirrors it for terminal logs/diffs/agent
// output: a column of rows, each a muted line-number gutter + the line. An
// optional per-line `highlight` callback drives syntax coloring via the
// framework's existing span mechanism.
//
// A pure builder returning a `ColumnNode`, semantic tokens only (B-ready).

import { row, column, text } from "../builders.ts";
import type { ColumnNode, UINode, Color, Span } from "../nodes.ts";

export interface CodeBlockOptions {
  /** Line number of the first line. Default 1. */
  startLine?: number;
  /** Color of the line-number gutter. Default "muted". */
  gutterColor?: Color;
  /** Show the line-number gutter. Default true. */
  showLineNumbers?: boolean;
  /** Per-line syntax highlight: given the line text + its 0-based index,
   *  return colored spans (same shape as `text()`'s highlight). */
  highlight?: (line: string, index: number) => Span[];
}

/** Build a numbered code block from a multi-line string. */
export function codeBlock(code: string, opts: CodeBlockOptions = {}): ColumnNode {
  const start = opts.startLine ?? 1;
  const showNumbers = opts.showLineNumbers ?? true;
  const lines = code.split("\n");
  // Gutter is at least 3 cols (SRCL uses width: 3ch), wider if numbers are.
  const gutterWidth = Math.max(3, String(start + lines.length - 1).length);

  const rows: UINode[] = lines.map((line, i) => {
    const content = opts.highlight
      ? text(line, { highlight: (t) => opts.highlight!(t, i) })
      : text(line, { fg: "primary" });
    if (!showNumbers) return content;
    const num = String(start + i).padStart(gutterWidth);
    return row(text(`${num} `, { fg: opts.gutterColor ?? "muted" }), content);
  });

  return column({}, rows);
}
