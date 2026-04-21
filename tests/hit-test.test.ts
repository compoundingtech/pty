import { describe, it, expect } from "vitest";
import { row, column, panel, text } from "../src/tui/builders.ts";
import { layoutRoot } from "../src/tui/layout.ts";
import { hitTest, findInPath } from "../src/tui/hit-test.ts";
import type { UINode } from "../src/tui/nodes.ts";

function build(): UINode[] {
  return [
    panel("left", [
      row(text("aaa")),
      row(text("bbb")),
    ]),
    panel("right", [
      row(text("ccc")),
      row(text("ddd")),
    ]),
  ];
}

describe("hitTest", () => {
  it("returns null outside all nodes", () => {
    const nodes = build();
    // Lay out into a small rect; clicks at 9999,9999 are clearly outside.
    layoutRoot(nodes, { x: 0, y: 0, width: 40, height: 10 });
    const h = hitTest(nodes, 9999, 9999);
    expect(h).toBeNull();
  });

  it("finds the deepest node under a point", () => {
    const nodes = build();
    layoutRoot(nodes, { x: 0, y: 0, width: 40, height: 10 });
    const panelRect = nodes[0]._rect!;
    // Click somewhere inside the first panel, on the first row.
    const h = hitTest(nodes, panelRect.x + 3, panelRect.y + 1);
    expect(h).not.toBeNull();
    // The root of the path is the panel; deepest should be a text node.
    expect(h!.path[0].type).toBe("panel");
    expect(h!.node.type).toBe("text");
  });

  it("findInPath locates an ancestor by type", () => {
    const nodes = build();
    layoutRoot(nodes, { x: 0, y: 0, width: 40, height: 10 });
    const panelRect = nodes[0]._rect!;
    const h = hitTest(nodes, panelRect.x + 3, panelRect.y + 1)!;
    const p = findInPath(h, "panel");
    expect(p?.title).toBe("left");
  });
});
