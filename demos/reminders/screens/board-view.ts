// Board view: kanban columns (todo / in-progress / done)
import {
  text, column, hstack, panel, selectable, checkbox,
  statusBar, footer,
  type UINode, type ScreenContext,
} from "../../../src/tui/index.ts";
import { updateScrollRegion } from "../../../src/tui/index.ts";
import {
  boardColumns, boardColumn, boardScroll, currentView, dataDir, reminders,
  type Reminder,
} from "../state.ts";

const COLUMN_TITLES = ["Todo", "In Progress", "Done"];

function renderCard(r: Reminder, _index: number, selected: boolean): UINode[] {
  const sel = selected ? "\u25b8 " : "  ";
  return [
    text(sel, selected ? "accent" : "muted"),
    checkbox(r.completed, r.completed ? "ok" : "muted"),
    text(" " + r.title, selected ? "primary" : "secondary", { bold: selected, truncate: true }),
  ];
}

export function renderBoardView(ctx: ScreenContext): UINode[] {
  const cols = boardColumns.get();
  const activeCol = boardColumn.get();
  const scrolls = boardScroll.get();
  const all = reminders.get();
  const viewport = Math.max(1, ctx.rows - 4);

  const columnNodes = cols.map((items, i) => {
    const region = updateScrollRegion(
      { ...scrolls[i], totalItems: items.length },
      items.length,
      viewport,
    );
    const isActive = i === activeCol;
    return column({ flex: true }, [
      panel(COLUMN_TITLES[i] + ` (${items.length})`, [
        items.length > 0
          ? selectable(region, items, (r, idx, sel) =>
              renderCard(r, idx, isActive && sel))
          : text("(empty)", "muted"),
      ]),
    ]);
  });

  return [
    statusBar("Reminders", `${dataDir.get()} \u2502 board \u2502 ${all.length} items`),
    hstack({ gap: 0 }, columnNodes as any),
    footer("v view  \u2190\u2192 column  \u2191\u2193 nav  space toggle  n new  T theme  q quit"),
  ];
}
