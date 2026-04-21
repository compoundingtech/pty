// All demos bundled for the sidebar. Order within the array is the order
// the sidebar renders them.

import { atoms } from "./demos/atoms.ts";
import { layout } from "./demos/layout.ts";
import {
  textInputDemo, textAreaDemo, formDemo, datePickerDemo,
} from "./demos/inputs.ts";
import {
  virtualListDemo, treeDemo,
} from "./demos/lists.ts";
import { tableDemo } from "./demos/data.ts";
import {
  confirmDemo, toastDemo, commandPaletteDemo, helpDemo,
} from "./demos/overlays.ts";
import {
  tabsDemo, markdownDemo, streamViewDemo,
} from "./demos/patterns.ts";
import {
  promptBarDemo, promptBarMultiDemo, toolbarDemo,
} from "./demos/bars.ts";
import {
  sparklineDemo, barChartDemo, meterGridDemo,
} from "./demos/meters.ts";
import { kitchenSink } from "./demos/kitchen-sink.ts";
import type { Demo } from "./types.ts";

export const demos: Demo[] = [
  atoms,
  layout,
  textInputDemo,
  textAreaDemo,
  formDemo,
  datePickerDemo,
  virtualListDemo,
  treeDemo,
  tableDemo,
  sparklineDemo,
  barChartDemo,
  tabsDemo,
  markdownDemo,
  streamViewDemo,
  promptBarDemo,
  promptBarMultiDemo,
  toolbarDemo,
  meterGridDemo,
  confirmDemo,
  toastDemo,
  commandPaletteDemo,
  helpDemo,
  kitchenSink,
];

export function demosByCategory(): { category: string; demos: Demo[] }[] {
  const order: string[] = [];
  const groups = new Map<string, Demo[]>();
  for (const d of demos) {
    if (!groups.has(d.category)) { order.push(d.category); groups.set(d.category, []); }
    groups.get(d.category)!.push(d);
  }
  return order.map(cat => ({ category: cat, demos: groups.get(cat)! }));
}
