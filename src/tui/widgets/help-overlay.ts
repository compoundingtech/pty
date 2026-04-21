// Help overlay — a keybinding reference. Consumers bind "?" to toggle a
// signal that owns a HelpSection[], then render the result via
// `helpPanel(sections)` as the body of an `overlay()` screen. Reads like a
// cheat-sheet, which is the point.

import { row, column, text, panel, separator } from "../builders.ts";
import type { UINode, ColumnNode } from "../nodes.ts";

export interface HelpBinding {
  key: string;
  desc: string;
}

export interface HelpSection {
  title: string;
  bindings: HelpBinding[];
}

function renderSection(sec: HelpSection, keyWidth: number): UINode[] {
  const out: UINode[] = [
    row(text(sec.title, "accent", { bold: true })),
  ];
  for (const b of sec.bindings) {
    out.push(row(
      text("  ", "muted"),
      text(b.key.padEnd(keyWidth + 2), "accent"),
      text(b.desc, "primary"),
    ));
  }
  return out;
}

/** Render the help as a panel body. Columns align within a section via a
 *  shared keyWidth derived from the widest key across ALL sections, so the
 *  sections line up visually when stacked. */
export function helpPanel(sections: readonly HelpSection[], title = "keybindings"): UINode {
  let keyWidth = 0;
  for (const s of sections) for (const b of s.bindings) keyWidth = Math.max(keyWidth, b.key.length);

  const children: UINode[] = [];
  sections.forEach((sec, i) => {
    if (i > 0) {
      children.push(separator());
    }
    children.push(...renderSection(sec, keyWidth));
  });
  children.push(separator());
  children.push(row(text("  press ? or esc to close", "muted", { dim: true })));

  return panel(title, children);
}
