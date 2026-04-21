// "Layout" — row / column / panel / separator / gap.

import {
  text, row, column, panel, separator, gap, spacer,
} from "../../../src/tui/index.ts";
import type { Demo } from "../types.ts";

export const layout: Demo = {
  id: "layout",
  category: "layout",
  name: "row / column / panel",
  blurb: "Containers compose into the shape you want. Rows flow horizontally, columns stack vertically.",
  render() {
    return [
      panel("rounded panel", [
        row(text("  first row", "primary")),
        row(text("  second row", "primary")),
        separator(),
        row(text("  below separator", "muted", { dim: true })),
      ]),
      gap(1),
      row(
        text(" left ", "accent", { bold: true }),
        spacer(),
        text(" pushed right ", "accent", { bold: true }),
      ),
      gap(1),
      row(
        text(" A ", "accent"),
        text(" | ", "muted", { dim: true }),
        text(" B ", "accent"),
        text(" | ", "muted", { dim: true }),
        text(" C ", "accent"),
      ),
    ];
  },
  handleKey() { return false; },
  source: String.raw`panel("rounded panel", [
  row(text("first row")),
  row(text("second row")),
  separator(),
  row(text("below separator", "muted", { dim: true })),
])

row(text("left"), spacer(), text("right"))  // spacer pushes to the right`,
};
