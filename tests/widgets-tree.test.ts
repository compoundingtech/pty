import { describe, it, expect } from "vitest";
import {
  createTreeState, flattenTree, toggleExpanded, moveSelection, handleTreeKey,
  type TreeNode,
} from "../src/tui/widgets/tree.ts";
import type { KeyEvent } from "../src/tui/input.ts";

function k(name: string): KeyEvent {
  return { name, ctrl: false, alt: false, shift: false };
}

const sample: TreeNode<null>[] = [
  { id: "root/a", label: "a", data: null, children: [
    { id: "root/a/1", label: "a1", data: null },
    { id: "root/a/2", label: "a2", data: null, children: [
      { id: "root/a/2/x", label: "x", data: null },
    ] },
  ] },
  { id: "root/b", label: "b", data: null },
];

describe("flattenTree", () => {
  it("returns roots when nothing is expanded", () => {
    const rows = flattenTree(sample, new Set());
    expect(rows.map(r => r.node.id)).toEqual(["root/a", "root/b"]);
    expect(rows[0].depth).toBe(0);
    expect(rows[0].hasChildren).toBe(true);
    expect(rows[0].expanded).toBe(false);
    expect(rows[1].hasChildren).toBe(false);
  });

  it("expands children at depth + 1", () => {
    const rows = flattenTree(sample, new Set(["root/a"]));
    expect(rows.map(r => r.node.id)).toEqual(["root/a", "root/a/1", "root/a/2", "root/b"]);
    expect(rows[1].depth).toBe(1);
    expect(rows[2].expanded).toBe(false); // a/2 has children but not expanded
  });

  it("expands grandchildren at depth + 2", () => {
    const rows = flattenTree(sample, new Set(["root/a", "root/a/2"]));
    expect(rows.map(r => r.node.id)).toEqual([
      "root/a", "root/a/1", "root/a/2", "root/a/2/x", "root/b",
    ]);
    expect(rows[3].depth).toBe(2);
  });
});

describe("toggleExpanded", () => {
  it("adds when absent, removes when present", () => {
    const s0 = createTreeState();
    const s1 = toggleExpanded(s0, "foo");
    expect(s1.expanded.has("foo")).toBe(true);
    const s2 = toggleExpanded(s1, "foo");
    expect(s2.expanded.has("foo")).toBe(false);
  });
});

describe("moveSelection", () => {
  it("clamps at the top and bottom", () => {
    const rows = flattenTree(sample, new Set());
    const s0 = { expanded: new Set<string>(), selectedId: "root/a" };
    expect(moveSelection(s0, rows, -5).selectedId).toBe("root/a");
    expect(moveSelection(s0, rows, 5).selectedId).toBe("root/b");
  });

  it("picks first row when nothing selected", () => {
    const rows = flattenTree(sample, new Set());
    const s0 = createTreeState();
    expect(moveSelection(s0, rows, 1).selectedId).toBe("root/a");
  });
});

describe("handleTreeKey", () => {
  it("arrow keys move selection", () => {
    const s0 = { expanded: new Set<string>(), selectedId: "root/a" };
    const rows = flattenTree(sample, s0.expanded);
    const r = handleTreeKey(s0, rows, k("down"));
    expect(r.state.selectedId).toBe("root/b");
    expect(r.action).toBe("moved");
  });

  it("right expands a folder", () => {
    const s0 = { expanded: new Set<string>(), selectedId: "root/a" };
    const rows = flattenTree(sample, s0.expanded);
    const r = handleTreeKey(s0, rows, k("right"));
    expect(r.state.expanded.has("root/a")).toBe(true);
    expect(r.action).toBe("expanded");
  });

  it("left collapses an expanded folder", () => {
    const s0 = { expanded: new Set(["root/a"]), selectedId: "root/a" };
    const rows = flattenTree(sample, s0.expanded);
    const r = handleTreeKey(s0, rows, k("left"));
    expect(r.state.expanded.has("root/a")).toBe(false);
    expect(r.action).toBe("collapsed");
  });

  it("enter activates leaves", () => {
    const expanded = new Set(["root/a"]);
    const rows = flattenTree(sample, expanded);
    const s0 = { expanded, selectedId: "root/a/1" };
    const r = handleTreeKey(s0, rows, k("return"));
    expect(r.action).toBe("activated");
    expect(r.row?.node.id).toBe("root/a/1");
  });
});
