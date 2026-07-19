// message — an SRCL-style chat/bus message bubble.
//
// SRCL ships two mirror components: Message (incoming — a bubble on the muted
// `--theme-border` fill, left-aligned) and MessageViewer (outgoing — a bubble
// on the accent `--theme-focused-foreground` fill, right-aligned). Those are
// the same widget with a direction flag, so this exposes one `message()` with
// an `outgoing` option (outgoing === SRCL's MessageViewer). Maps to the
// bus/inbox view from the inventory.
//
// The CSS triangle pointer + drop shadow are web-only chrome; the terminal
// version keeps the essence — a padded bubble on a direction-colored fill,
// aligned left (incoming) or right (outgoing), with an optional sender label.
//
// A pure builder returning a `ColumnNode`, semantic tokens only (B-ready).

import { row, column, text, spacer } from "../builders.ts";
import type { ColumnNode, UINode } from "../nodes.ts";

export interface MessageOptions {
  /** Outgoing (accent fill, right-aligned) vs incoming (muted fill, left).
   *  Default false = incoming. `outgoing: true` is SRCL's MessageViewer. */
  outgoing?: boolean;
  /** Optional sender label rendered above the bubble. */
  from?: string;
}

/** Build a message bubble from (possibly multi-line) content. */
export function message(content: string, opts: MessageOptions = {}): ColumnNode {
  const outgoing = opts.outgoing ?? false;
  const bubbleBg = outgoing ? "accent" : "border";
  const align = (node: UINode): UINode => (outgoing ? row(spacer(), node) : node);

  const nodes: UINode[] = [];
  if (opts.from) {
    nodes.push(align(text(opts.from, { fg: "muted", bold: true })));
  }
  for (const line of content.split("\n")) {
    nodes.push(align(text(` ${line} `, { fg: "primary", background: bubbleBg })));
  }
  return column({}, nodes);
}
