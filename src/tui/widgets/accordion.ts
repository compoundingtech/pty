// accordion — an SRCL-style collapsible disclosure section.
//
// SRCL's Accordion is a header row (`▸ title` collapsed / `▾ title` expanded)
// that toggles visibility of indented content. This mirrors it as a pure,
// state-first builder: the caller owns the `expanded` boolean (and wires the
// toggle key), the widget is pure render — matching the rest of the widgets
// tier ("you own the state, widgets are pure render + pure key dispatch").
//
// Returns a `ColumnNode` (header + optional indented content), styled with
// semantic tokens only (B-ready).

import { row, column, text } from "../builders.ts";
import type { ColumnNode, UINode } from "../nodes.ts";

export interface AccordionOptions {
  /** Highlight the header (accent + bold) — e.g. when this row has focus. */
  focused?: boolean;
  /** Disclosure glyph when collapsed. Default "▸". */
  collapsedIcon?: string;
  /** Disclosure glyph when expanded. Default "▾". */
  expandedIcon?: string;
  /** Columns to indent the expanded content by. Default 2. */
  indent?: number;
}

/** Build a disclosure section. `expanded` is caller-owned state; `children`
 *  render (indented) only when expanded. */
export function accordion(
  title: string,
  expanded: boolean,
  children: UINode[] = [],
  opts: AccordionOptions = {},
): ColumnNode {
  const focused = opts.focused ?? false;
  const indent = opts.indent ?? 2;
  const icon = expanded ? (opts.expandedIcon ?? "▾") : (opts.collapsedIcon ?? "▸");

  const header = row(
    text(`${icon} `, { fg: focused ? "accent" : "muted" }),
    text(title, { fg: focused ? "accent" : "primary", ...(focused ? { bold: true } : {}) }),
  );

  const nodes: UINode[] = [header];
  if (expanded && children.length > 0) {
    // Indent the whole content block: a spacer text at x0 pushes the content
    // column to x = indent, where it flows vertically.
    nodes.push(row(text(" ".repeat(indent)), column({}, children)));
  }
  return column({}, nodes);
}
