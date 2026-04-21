import { describe, it, expect } from "vitest";
import {
  createTableState, sortRows, handleTableKey, renderTable,
  type TableColumn,
} from "../src/tui/widgets/table.ts";
import type { KeyEvent } from "../src/tui/input.ts";

function k(name: string, opts: Partial<KeyEvent> = {}): KeyEvent {
  return { name, char: opts.char, ctrl: opts.ctrl ?? false, alt: opts.alt ?? false, shift: opts.shift ?? false };
}

interface Person { name: string; age: number }

const cols: TableColumn<Person>[] = [
  { id: "name", header: "Name", render: r => r.name },
  { id: "age",  header: "Age",  render: r => String(r.age), getSortValue: r => r.age, align: "right" },
];

const rows: Person[] = [
  { name: "Bea",  age: 30 },
  { name: "Alex", age: 25 },
  { name: "Dan",  age: 40 },
  { name: "Cam",  age: 35 },
];

describe("table — sorting", () => {
  it("initial sort defaults to first column ascending", () => {
    const s = createTableState(cols);
    const sorted = sortRows(rows, cols, s);
    expect(sorted.map(r => r.name)).toEqual(["Alex", "Bea", "Cam", "Dan"]);
  });

  it("numeric sort uses getSortValue, not the rendered string", () => {
    const s = { ...createTableState(cols), sortColumnId: "age", sortDirection: "asc" as const };
    const sorted = sortRows(rows, cols, s);
    expect(sorted.map(r => r.age)).toEqual([25, 30, 35, 40]);
  });

  it("desc flips direction", () => {
    const s = { ...createTableState(cols), sortColumnId: "age", sortDirection: "desc" as const };
    const sorted = sortRows(rows, cols, s);
    expect(sorted.map(r => r.age)).toEqual([40, 35, 30, 25]);
  });

  it("stable: equal keys keep original order", () => {
    const rs: Person[] = [
      { name: "Alex", age: 25 }, { name: "Bea", age: 25 }, { name: "Cam", age: 25 },
    ];
    const s = { ...createTableState(cols), sortColumnId: "age", sortDirection: "asc" as const };
    expect(sortRows(rs, cols, s).map(r => r.name)).toEqual(["Alex", "Bea", "Cam"]);
  });
});

describe("table — keys", () => {
  it("numeric keys toggle sort on that column", () => {
    const s0 = createTableState(cols);
    // "2" -> toggle sort on the 2nd column (age).
    const r1 = handleTableKey(s0, rows, cols, k("2", { char: "2" }));
    expect(r1.action).toBe("sorted");
    expect(r1.state.sortColumnId).toBe("age");
    expect(r1.state.sortDirection).toBe("asc");
    // Same column again flips direction.
    const r2 = handleTableKey(r1.state, rows, cols, k("2", { char: "2" }));
    expect(r2.state.sortDirection).toBe("desc");
  });

  it("up/down move selection within bounds", () => {
    const s0 = createTableState(cols);
    const sorted = sortRows(rows, cols, s0);
    const r = handleTableKey(s0, sorted, cols, k("down"));
    expect(r.state.selectedIndex).toBe(1);
    expect(r.action).toBe("moved");
  });

  it("return activates the selected row", () => {
    const s0 = { ...createTableState(cols), selectedIndex: 2 };
    const sorted = sortRows(rows, cols, s0);
    const r = handleTableKey(s0, sorted, cols, k("return"));
    expect(r.action).toBe("activate");
    expect(r.activated!.name).toBe(sorted[2].name);
  });
});

describe("table — rendering", () => {
  it("renders header + separator + data rows", () => {
    const s = createTableState(cols);
    const sorted = sortRows(rows, cols, s);
    const node = renderTable(sorted, cols, s);
    expect((node as any).children).toHaveLength(2 + rows.length);
  });

  it("shows a sort arrow on the active column", () => {
    const s = createTableState(cols, "age");
    const sorted = sortRows(rows, cols, s);
    const node = renderTable(sorted, cols, s);
    const headerRow = (node as any).children[0];
    const ageHeader = headerRow.children.find((c: any) => c.text?.includes("Age"));
    expect(ageHeader.text).toMatch(/▲/);
  });
});
