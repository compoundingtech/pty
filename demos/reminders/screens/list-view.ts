// List view: reminders grouped by due date
import {
  text, row, column, hstack, panel, groupedSelectable, checkbox,
  statusBar, footer,
  type UINode, type ScreenContext,
} from "../../../src/tui/index.ts";
import { updateScrollRegion } from "../../../src/tui/index.ts";
import {
  groupedByDate, selectedIndex, listScroll, flatReminders,
  currentView, dataDir, type Reminder,
} from "../state.ts";

function priorityColor(p: string): "error" | "warn" | "ok" {
  if (p === "high") return "error";
  if (p === "medium") return "warn";
  return "ok";
}

function renderItem(r: Reminder, _index: number, selected: boolean): UINode[] {
  const sel = selected ? "\u25b8 " : "  ";
  const prio = r.priority === "high" ? "!" : r.priority === "medium" ? "\u2022" : " ";
  return [
    text(sel, selected ? "accent" : "muted"),
    checkbox(r.completed, r.completed ? "ok" : "muted"),
    text(" " + prio + " ", priorityColor(r.priority)),
    text(r.title, selected ? "primary" : "secondary", { bold: selected, truncate: true }),
    text("  " + r.due, "muted"),
  ];
}

export function renderListView(ctx: ScreenContext): UINode[] {
  const groups = groupedByDate.get();
  const all = flatReminders.get();
  const viewport = Math.max(1, ctx.rows - 4);
  const region = updateScrollRegion(
    { ...listScroll.get(), selectedIndex: selectedIndex.get(), totalItems: all.length },
    all.length,
    viewport,
  );

  return [
    statusBar("Reminders", `${dataDir.get()} \u2502 ${currentView.get()} \u2502 ${all.length} items`),
    panel("Reminders", [
      groupedSelectable(region, groups, renderItem),
    ]),
    footer("v view  \u2191\u2193 nav  space toggle  n new  e edit  d delete  T theme  q quit"),
  ];
}
