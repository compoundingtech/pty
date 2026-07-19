// actionListItem — an SRCL-style selectable action row.
//
// SRCL's ActionListItem is `[icon] text` where the icon sits in a small
// fixed-width cell that highlights on focus/hover. This mirrors it: a row
// with a 3-cell icon chip (highlighted with the accent background when
// focused) followed by the label, plus optional right-aligned text.
//
// A pure, state-first builder — the caller owns which row is `focused` — so
// it drops straight into a `selectable`/`virtual-list` (the agent/session
// roster from the inventory). Styled with semantic tokens only (B-ready).

import { row, text, spacer } from "../builders.ts";
import type { RowNode, UINode } from "../nodes.ts";

export interface ActionListItemOptions {
  /** Single glyph shown in the 3-cell icon chip. Default blank. */
  icon?: string;
  /** Highlight the row (accent icon chip + bold label) — e.g. selected. */
  focused?: boolean;
  /** Optional right-aligned trailing text (SRCL's space-between layout). */
  right?: string;
}

/** Build a selectable action row: `actionListItem("Deploy", { icon: "▶" })`. */
export function actionListItem(label: string, opts: ActionListItemOptions = {}): RowNode {
  const focused = opts.focused ?? false;
  // 3-cell icon chip: pad the glyph to ` X ` so the chip is a stable width.
  const iconChip = text(` ${opts.icon ?? " "} `, {
    background: focused ? "accent" : "border",
    fg: focused ? "primary" : "muted",
  });
  const labelText = text(` ${label}`, { fg: "primary", ...(focused ? { bold: true } : {}) });

  const children: UINode[] = [iconChip, labelText];
  if (opts.right !== undefined) {
    children.push(spacer(), text(opts.right, { fg: "muted" }));
  }
  return row(...children);
}
