// Sortable table. Columns describe: how to render a row's cell, how to
// extract a sort value, and an alignment hint. State tracks the selected
// row, the active sort column, and direction. Pure helpers for sort and
// key dispatch; the render is straight-forward once the data is sorted.

import { row, column, text } from "../builders.ts";
import type { UINode, ColumnNode, Color } from "../nodes.ts";
import type { KeyEvent } from "../input.ts";

export type TableAlign = "left" | "right";

export interface TableColumn<Row> {
  id: string;
  header: string;
  /** Render the cell as a plain string. Width/truncation handled outside. */
  render: (row: Row) => string;
  /** Value used to sort. Defaults to the rendered string. */
  getSortValue?: (row: Row) => string | number;
  align?: TableAlign;
  /** Explicit column width. Omitted means "auto" (max of header + cells). */
  width?: number;
}

export interface TableState {
  sortColumnId: string | null;
  sortDirection: "asc" | "desc";
  selectedIndex: number;
}

export function createTableState<Row>(
  columns: readonly TableColumn<Row>[],
  initialSortId?: string,
): TableState {
  const sortColumnId = initialSortId ?? columns[0]?.id ?? null;
  return { sortColumnId, sortDirection: "asc", selectedIndex: 0 };
}

function valueOf<Row>(col: TableColumn<Row>, r: Row): string | number {
  return col.getSortValue ? col.getSortValue(r) : col.render(r);
}

/** Returns a NEW array sorted per `state`. Stable — preserves original
 *  order for equal-keyed rows via the index tie-breaker. */
export function sortRows<Row>(
  rows: readonly Row[],
  columns: readonly TableColumn<Row>[],
  state: TableState,
): Row[] {
  if (!state.sortColumnId) return [...rows];
  const col = columns.find(c => c.id === state.sortColumnId);
  if (!col) return [...rows];
  const sign = state.sortDirection === "asc" ? 1 : -1;
  const indexed = rows.map((r, i) => ({ r, i }));
  indexed.sort((a, b) => {
    const va = valueOf(col, a.r);
    const vb = valueOf(col, b.r);
    if (va < vb) return -1 * sign;
    if (va > vb) return 1 * sign;
    return a.i - b.i;
  });
  return indexed.map(x => x.r);
}

export interface HandleTableKeyResult<Row> {
  state: TableState;
  action: "moved" | "sorted" | "activate" | "none";
  activated?: Row;
}

/** Default bindings:
 *    up/down        -> move selection
 *    home/end       -> first/last row
 *    pageup/down    -> +/- 10 rows
 *    return         -> "activate" with the selected row
 *    1..9           -> toggle sort on that column (by position). Same column
 *                      twice flips direction. */
export function handleTableKey<Row>(
  state: TableState,
  sortedRows: readonly Row[],
  columns: readonly TableColumn<Row>[],
  key: KeyEvent,
): HandleTableKeyResult<Row> {
  const clamp = (i: number) => Math.max(0, Math.min(Math.max(0, sortedRows.length - 1), i));

  switch (key.name) {
    case "up":       return { state: { ...state, selectedIndex: clamp(state.selectedIndex - 1) }, action: "moved" };
    case "down":     return { state: { ...state, selectedIndex: clamp(state.selectedIndex + 1) }, action: "moved" };
    case "pageup":   return { state: { ...state, selectedIndex: clamp(state.selectedIndex - 10) }, action: "moved" };
    case "pagedown": return { state: { ...state, selectedIndex: clamp(state.selectedIndex + 10) }, action: "moved" };
    case "home":     return { state: { ...state, selectedIndex: 0 }, action: "moved" };
    case "end":      return { state: { ...state, selectedIndex: clamp(sortedRows.length - 1) }, action: "moved" };
    case "return":
      return { state, action: "activate", activated: sortedRows[state.selectedIndex] };
  }

  if (key.char && /^[1-9]$/.test(key.char) && !key.ctrl && !key.alt) {
    const idx = parseInt(key.char, 10) - 1;
    const col = columns[idx];
    if (col) {
      if (state.sortColumnId === col.id) {
        return {
          state: { ...state, sortDirection: state.sortDirection === "asc" ? "desc" : "asc" },
          action: "sorted",
        };
      }
      return {
        state: { ...state, sortColumnId: col.id, sortDirection: "asc" },
        action: "sorted",
      };
    }
  }

  return { state, action: "none" };
}

function columnWidths<Row>(
  rows: readonly Row[],
  columns: readonly TableColumn<Row>[],
): number[] {
  return columns.map(col => {
    if (col.width != null) return col.width;
    let w = col.header.length;
    for (const r of rows) w = Math.max(w, col.render(r).length);
    return w;
  });
}

function padCell(s: string, w: number, align: TableAlign): string {
  if (s.length >= w) return s.slice(0, w);
  const pad = " ".repeat(w - s.length);
  return align === "right" ? pad + s : s + pad;
}

/** Render a sortable table. Rows should already be sorted via `sortRows`
 *  (we don't sort inside render so the caller can memoize). */
export function renderTable<Row>(
  sortedRows: readonly Row[],
  columns: readonly TableColumn<Row>[],
  state: TableState,
): UINode {
  const widths = columnWidths(sortedRows, columns);
  const arrow = (colId: string): string => {
    if (state.sortColumnId !== colId) return "  ";
    return state.sortDirection === "asc" ? " \u25b2" : " \u25bc";
  };

  const header = row(...columns.flatMap((col, i) => {
    const label = padCell(col.header + arrow(col.id), widths[i] + 2, col.align ?? "left");
    return [text(label, "accent", { bold: true })];
  }));

  const separator = row(...columns.flatMap((_, i) => [
    text("\u2500".repeat(widths[i]) + "  ", "muted", { dim: true }),
  ]));

  const body: UINode[] = sortedRows.map((r, idx) => {
    const selected = idx === state.selectedIndex;
    const color: Color = selected ? "accent" : "primary";
    const cells = columns.flatMap((col, i) => [
      text(padCell(col.render(r), widths[i], col.align ?? "left") + "  ",
           color, { bold: selected }),
    ]);
    return row(...cells);
  });

  const node: ColumnNode = { type: "column", children: [header, separator, ...body] };
  return node;
}
