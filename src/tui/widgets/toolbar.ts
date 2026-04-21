// Toolbar — a horizontal strip of labeled actions with highlighted hotkeys.
// Useful as a compact keybind legend or as a "buttons" row for mouse-free
// interfaces. The `[X]abel` convention makes the bound letter unambiguous
// without needing a separate legend.

import { row, text } from "../builders.ts";
import type { UINode, Color } from "../nodes.ts";

export interface ToolbarItem {
  /** The key bound to this action. One character — letter or digit. */
  key: string;
  /** Label around the key. Leave {key} placeholder or the widget will
   *  prepend "[key] " to `label`. See `format` option. */
  label: string;
  /** Optional one-line hint shown dim after the label. */
  hint?: string;
  /** Mark this item currently active (highlighted). */
  active?: boolean;
  /** Disable visually (dim). Key-dispatching is up to the caller. */
  disabled?: boolean;
}

export interface ToolbarOptions {
  /** Separator between items. Defaults to "  " (two spaces). */
  separator?: string;
  /** Render format:
   *    - "bracket" (default): `[N]ew  [S]ave`
   *    - "inline": first occurrence of `key` inside `label` is highlighted.
   *                Useful when the label naturally contains the key:
   *                  { key: "n", label: "new" } -> "[n]ew". */
  format?: "bracket" | "inline";
  /** Accent color for active items. Default "accent". */
  activeColor?: Color;
}

function bracketize(item: ToolbarItem): UINode[] {
  const baseColor: Color = item.active ? "accent" : "primary";
  const hintColor: Color = item.disabled ? "muted" : "muted";
  const cell: UINode[] = [
    text("[", baseColor, { bold: item.active }),
    text(item.key.toUpperCase(), baseColor, { bold: true }),
    text("]", baseColor, { bold: item.active }),
    text(item.label, item.disabled ? "muted" : baseColor, { bold: item.active, dim: item.disabled }),
  ];
  if (item.hint) {
    cell.push(text(` ${item.hint}`, hintColor, { dim: true }));
  }
  return cell;
}

function inlineize(item: ToolbarItem): UINode[] {
  const baseColor: Color = item.active ? "accent" : "primary";
  const keyIdx = item.label.toLowerCase().indexOf(item.key.toLowerCase());
  if (keyIdx < 0) return bracketize(item);
  const before = item.label.slice(0, keyIdx);
  const kChar  = item.label[keyIdx];
  const after  = item.label.slice(keyIdx + 1);
  const cell: UINode[] = [
    text(before, item.disabled ? "muted" : baseColor, { dim: item.disabled }),
    text(kChar, item.active ? "accent" : "accent", { bold: true, dim: item.disabled }),
    text(after, item.disabled ? "muted" : baseColor, { dim: item.disabled }),
  ];
  if (item.hint) cell.push(text(` ${item.hint}`, "muted", { dim: true }));
  return cell;
}

/** Render the toolbar as a single row. */
export function toolbar(items: readonly ToolbarItem[], opts: ToolbarOptions = {}): UINode {
  const separator = opts.separator ?? "  ";
  const format = opts.format ?? "bracket";
  const children: UINode[] = [];
  items.forEach((item, i) => {
    if (i > 0) children.push(text(separator, "muted"));
    const cell = format === "inline" ? inlineize(item) : bracketize(item);
    children.push(...cell);
  });
  return row(...children);
}

/** Check whether a KeyEvent-like `char` matches any non-disabled item's
 *  key. The caller wires the action via their own `switch (key.char)` — this
 *  helper is just for feature-detection / highlight decisions. */
export function toolbarItemFor(
  items: readonly ToolbarItem[],
  char: string | undefined,
): ToolbarItem | null {
  if (!char) return null;
  const c = char.toLowerCase();
  for (const it of items) {
    if (it.disabled) continue;
    if (it.key.toLowerCase() === c) return it;
  }
  return null;
}
