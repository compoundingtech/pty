// breadCrumbs — an SRCL-style breadcrumb trail.
//
// SRCL's BreadCrumbs renders items joined by a ` ❯ ` symbol (1ch margins),
// each item on a subtle `--theme-border` fill. This mirrors that: a row of
// item labels separated by the same ❯ glyph. Terminal convention adds one
// thing SRCL conveys via hover/focus — the current (last) crumb is
// emphasized with the accent color so "where you are" reads at a glance.
//
// Maps directly to the inventory's "network → host → agent drill-path".
//
// A pure builder returning a `RowNode` of `TextNode`s, styled with semantic
// tokens only — the B-ready pattern, renderable under a non-terminal backend.

import { row, text } from "../builders.ts";
import type { RowNode, UINode, Color } from "../nodes.ts";

export interface BreadCrumbItem {
  label: string;
}

export type BreadCrumbInput = BreadCrumbItem | string;

export interface BreadCrumbsOptions {
  /** Separator glyph between crumbs. Default " ❯ " (SRCL's symbol, 1ch margins). */
  separator?: string;
  /** Emphasize the last (current) crumb with the accent color + bold.
   *  Default true. */
  emphasizeLast?: boolean;
  /** Render each crumb on a muted, border-toned fill like SRCL. Default false
   *  (a plainer terminal trail). */
  chips?: boolean;
}

/** Build a breadcrumb trail: `breadCrumbs(["net", "host", "agent"])`. Accepts
 *  bare strings or `{ label }` items. Returns a `RowNode` of styled text. */
export function breadCrumbs(items: BreadCrumbInput[], opts: BreadCrumbsOptions = {}): RowNode {
  const separator = opts.separator ?? " ❯ ";
  const emphasizeLast = opts.emphasizeLast ?? true;
  const chips = opts.chips ?? false;

  const labels = items.map((it) => (typeof it === "string" ? it : it.label));
  const children: UINode[] = [];

  labels.forEach((label, i) => {
    const isLast = i === labels.length - 1;
    const current = isLast && emphasizeLast;
    const color: Color = current ? "accent" : "secondary";
    children.push(
      text(chips ? ` ${label} ` : label, {
        fg: color,
        ...(current ? { bold: true } : {}),
        ...(chips ? { background: "border" as Color } : {}),
      }),
    );
    if (!isLast) children.push(text(separator, { fg: "muted" }));
  });

  return row(...children);
}
