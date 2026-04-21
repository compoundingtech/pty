// Data: sortable table.

import {
  text, row, separator,
  signal,
  createTableState, sortRows, handleTableKey, renderTable,
  type TableColumn, type TableState,
} from "../../../src/tui/index.ts";
import type { Demo } from "../types.ts";

interface Person { name: string; role: string; joined: string; age: number }

const people: Person[] = [
  { name: "Alex",  role: "eng",     joined: "2022-03-01", age: 28 },
  { name: "Bea",   role: "design",  joined: "2021-05-14", age: 34 },
  { name: "Cam",   role: "eng",     joined: "2024-01-09", age: 25 },
  { name: "Dan",   role: "product", joined: "2019-11-20", age: 41 },
  { name: "Erin",  role: "eng",     joined: "2023-07-03", age: 29 },
];

const cols: TableColumn<Person>[] = [
  { id: "name",   header: "Name",   render: p => p.name },
  { id: "role",   header: "Role",   render: p => p.role },
  { id: "joined", header: "Joined", render: p => p.joined },
  { id: "age",    header: "Age",    render: p => String(p.age), getSortValue: p => p.age, align: "right" },
];

const tableState = signal<TableState>(createTableState(cols, "name"));

export const tableDemo: Demo = {
  id: "table",
  category: "data",
  name: "sortable table",
  blurb: "Press 1-4 to sort by column; press the same number again to flip direction.",
  render() {
    const s = tableState.get();
    const sorted = sortRows(people, cols, s);
    return [
      renderTable(sorted, cols, s),
      separator(),
      row(text(`  sort: ${s.sortColumnId} ${s.sortDirection}   selected: ${sorted[s.selectedIndex]?.name ?? ""}`, "muted", { dim: true })),
    ];
  },
  handleKey(key) {
    const s = tableState.peek();
    const sorted = sortRows(people, cols, s);
    const r = handleTableKey(s, sorted, cols, key);
    tableState.set(r.state);
    return r.action !== "none";
  },
  source: String.raw`const cols: TableColumn<Row>[] = [
  { id: "name", header: "Name", render: r => r.name },
  { id: "age",  header: "Age",  render: r => String(r.age), getSortValue: r => r.age, align: "right" },
];
const state = signal(createTableState(cols, "name"));
// render:
const sorted = sortRows(rows, cols, state.get());
renderTable(sorted, cols, state.get());
// keys:
const r = handleTableKey(state.peek(), sorted, cols, key);
state.set(r.state);`,
};
