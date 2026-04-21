// Kitchen sink: a mini "reminders-ish" screen using many widgets at once.
// Proves the library composes.

import {
  text, row, column, panel, separator, spacer, checkbox,
  signal,
  createTabsState, handleTabsKey, renderTabs,
  createTreeState, flattenTree, handleTreeKey, treeGlyph,
  createTableState, sortRows, renderTable, handleTableKey,
  renderToasts, pushToast, pruneExpired, createToastQueue,
  type TabsState, type TabDef, type TreeState, type TreeNode,
  type TableColumn, type TableState, type ToastQueue,
} from "../../../src/tui/index.ts";
import type { Demo } from "../types.ts";

type Item = { id: string; title: string; tag: string; due: string; done: boolean };

const items: Item[] = [
  { id: "a", title: "Buy milk",            tag: "home", due: "today 17:00",   done: false },
  { id: "b", title: "Call Mom",            tag: "home", due: "tomorrow 10am", done: false },
  { id: "c", title: "Refactor auth",       tag: "work", due: "Friday",        done: false },
  { id: "d", title: "Write release notes", tag: "work", due: "today",         done: true  },
];

const tabs: TabDef[] = [
  { id: "all",    label: "All"    },
  { id: "home",   label: "#home"  },
  { id: "work",   label: "#work"  },
];
const tabsState = signal<TabsState>(createTabsState(tabs));

const tree: TreeNode<string>[] = [
  { id: "inbox", label: "Inbox", data: "root" },
  { id: "work",  label: "Work",  data: "root", children: [
    { id: "work/backlog", label: "Backlog", data: "list" },
    { id: "work/done",    label: "Done",    data: "list" },
  ] },
  { id: "home",  label: "Home", data: "root", children: [
    { id: "home/errands", label: "Errands", data: "list" },
  ] },
];
const treeState = signal<TreeState>({ expanded: new Set(["work", "home"]), selectedId: "work" });

const cols: TableColumn<Item>[] = [
  { id: "done",  header: " ",     render: r => r.done ? "\u2713" : "\u25cb" },
  { id: "title", header: "Title", render: r => r.title },
  { id: "tag",   header: "Tag",   render: r => `#${r.tag}` },
  { id: "due",   header: "Due",   render: r => r.due },
];
const tableState = signal<TableState>(createTableState(cols, "title"));

const toasts = signal<ToastQueue>(createToastQueue());

export const kitchenSink: Demo = {
  id: "kitchen-sink",
  category: "sink",
  name: "kitchen sink",
  blurb: "Tabs across the top, a folder tree on the left, a sortable table on the right. Compose the primitives.",
  render() {
    const q = pruneExpired(toasts.peek());
    if (q !== toasts.peek()) toasts.set(q);

    const filtered = items.filter(it => {
      const tab = tabsState.get().activeId;
      if (tab === "home") return it.tag === "home";
      if (tab === "work") return it.tag === "work";
      return true;
    });
    const sorted = sortRows(filtered, cols, tableState.get());

    const sidebar = column({ width: 20 }, [
      row(text("  Lists", "muted", { bold: true })),
      ...flattenTree(tree, treeState.get().expanded).map(r => {
        const selected = r.node.id === treeState.get().selectedId;
        return row(
          text("  ".repeat(r.depth) + treeGlyph(r) + r.node.label,
            selected ? "accent" : "primary",
            { bold: selected }),
        );
      }),
    ]);

    const main = column({ flex: true }, [
      renderTabs(tabsState.get(), tabs),
      separator(),
      renderTable(sorted, cols, tableState.get()),
      separator(),
      row(text("  n: new toast   t / space-tab: switch tab   ↑↓: move   1..4: sort", "muted", { dim: true })),
      renderToasts(toasts.get()),
    ]);

    return [
      row(sidebar, text("  ", "muted"), main),
    ];
  },
  handleKey(key) {
    // Tabs (ctrl+tab or number keys 1..3)
    const tabResult = handleTabsKey(tabsState.peek(), tabs, key);
    if (tabResult) { tabsState.set(tabResult); return true; }

    // Table (arrows, numbers to sort)
    const sorted = sortRows(items, cols, tableState.peek());
    const tResult = handleTableKey(tableState.peek(), sorted, cols, key);
    if (tResult.action !== "none") {
      tableState.set(tResult.state);
      if (tResult.action === "activate" && tResult.activated) {
        toasts.set(pushToast(toasts.peek(), `opened: ${tResult.activated.title}`, { kind: "info" }));
      }
      return true;
    }

    // Tree (left/right to expand/collapse — up/down already eaten above)
    if (key.name === "left" || key.name === "right") {
      const rows = flattenTree(tree, treeState.peek().expanded);
      const r = handleTreeKey(treeState.peek(), rows, key);
      treeState.set(r.state);
      return r.action !== "none";
    }

    // Toast hotkey
    if (key.char === "n") {
      toasts.set(pushToast(toasts.peek(), `saved at ${new Date().toLocaleTimeString()}`, { kind: "success" }));
      return true;
    }

    return false;
  },
  source: String.raw`// Layout:
//   [ sidebar: tree    ][ main: tabs + table ]
//
// State comes from a handful of signals (tabsState, treeState, tableState).
// handleKey is a short cascade: try tabs, then table, then tree, then app
// hotkeys. renderToasts is layered on last so notifications float above.
//
// This is 120 lines end-to-end including imports, state, render, and keys.`,
};
