// Lists: selectable, virtual-list, tree.

import {
  text, row, column, panel, separator,
  signal,
  createVirtualListState, handleVirtualKey, renderVirtualList, virtualWindow,
  createTreeState, flattenTree, handleTreeKey, treeGlyph,
  type VirtualListState, type TreeState, type TreeNode,
} from "../../../src/tui/index.ts";
import type { Demo } from "../types.ts";

// --- virtualized list ---
const big = Array.from({ length: 5000 }, (_, i) => `item ${i.toString().padStart(5, "0")}`);
const virtState = signal<VirtualListState>(createVirtualListState(big.length, 10));

export const virtualListDemo: Demo = {
  id: "virtual-list",
  category: "lists",
  name: "virtualized list (5000 items)",
  blurb: "renderVirtualList only invokes the item renderer for the visible slice. Arrow keys, page up/down, home/end all work.",
  render() {
    const s = virtState.get();
    return [
      row(text(`  window: ${JSON.stringify(virtualWindow(s))}   selected: ${s.selectedIndex}`, "muted", { dim: true })),
      panel("5000 items", [
        renderVirtualList(s, (i, sel) =>
          row(text((sel ? "  \u25b8 " : "    ") + big[i],
                   sel ? "accent" : "primary", { bold: sel }))
        ),
      ]),
    ];
  },
  handleKey(key) {
    const r = handleVirtualKey(virtState.peek(), key);
    virtState.set(r.state);
    return r.action !== "none";
  },
  source: String.raw`const big = Array.from({ length: 5000 }, (_, i) => 'item ' + i);
const state = signal(createVirtualListState(big.length, 10));

// render:
renderVirtualList(state.get(), (i, sel) =>
  row(text((sel ? '> ' : '  ') + big[i])),
)

// keys:
const r = handleVirtualKey(state.peek(), key);
state.set(r.state);`,
};

// --- tree ---
const sampleTree: TreeNode<string>[] = [
  { id: "work", label: "Work", data: "folder", children: [
    { id: "work/backlog", label: "Backlog", data: "folder", children: [
      { id: "work/backlog/a", label: "refactor auth", data: "file" },
      { id: "work/backlog/b", label: "ship feature X", data: "file" },
    ] },
    { id: "work/in-progress", label: "In progress", data: "folder", children: [
      { id: "work/in-progress/a", label: "fix CSI-Z", data: "file" },
    ] },
  ] },
  { id: "home", label: "Home", data: "folder", children: [
    { id: "home/groceries", label: "groceries", data: "file" },
    { id: "home/plants", label: "water plants", data: "file" },
  ] },
];

const treeState = signal<TreeState>({ expanded: new Set(["work"]), selectedId: "work" });

export const treeDemo: Demo = {
  id: "tree",
  category: "lists",
  name: "tree view",
  blurb: "arrow keys move; right/left expand/collapse; enter activates a leaf.",
  render() {
    const s = treeState.get();
    const rows = flattenTree(sampleTree, s.expanded);
    return rows.map(r => {
      const selected = r.node.id === s.selectedId;
      const prefix = "  ".repeat(r.depth) + treeGlyph(r);
      return row(
        text(prefix, "muted"),
        text(r.node.label + (r.node.data === "file" ? "" : ""),
          selected ? "accent" : "primary",
          { bold: selected }),
      );
    });
  },
  handleKey(key) {
    const s = treeState.peek();
    const rows = flattenTree(sampleTree, s.expanded);
    const r = handleTreeKey(s, rows, key);
    treeState.set(r.state);
    return r.action !== "none";
  },
  source: String.raw`const state = signal({ expanded: new Set(['work']), selectedId: 'work' });
// render:
const rows = flattenTree(tree, state.get().expanded);
rows.map(r => row(
  text('  '.repeat(r.depth) + treeGlyph(r), 'muted'),
  text(r.node.label),
))
// keys:
const r = handleTreeKey(state.peek(), rows, key);
state.set(r.state);`,
};
