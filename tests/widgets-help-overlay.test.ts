import { describe, it, expect } from "vitest";
import { helpPanel, type HelpSection } from "../src/tui/widgets/help-overlay.ts";

const sections: HelpSection[] = [
  {
    title: "Navigation",
    bindings: [
      { key: "j/k", desc: "down/up" },
      { key: "enter", desc: "open" },
    ],
  },
  {
    title: "Editing",
    bindings: [
      { key: "n", desc: "new" },
      { key: "x", desc: "delete" },
    ],
  },
];

describe("help overlay", () => {
  it("renders a panel whose title defaults to 'keybindings'", () => {
    const node = helpPanel(sections);
    expect((node as any).type).toBe("panel");
    expect((node as any).title).toBe("keybindings");
  });

  it("respects a custom title", () => {
    const node = helpPanel(sections, "Shortcuts");
    expect((node as any).title).toBe("Shortcuts");
  });

  it("includes one row per binding plus section headers + separators", () => {
    const node = helpPanel(sections);
    // 2 sections: 2 titles + 4 bindings = 6 core rows
    // + 1 separator between sections
    // + 1 separator + 1 hint at the end
    // = 9
    const count = (node as any).children.length;
    expect(count).toBe(9);
  });

  it("key column is padded to the widest key across ALL sections", () => {
    const node = helpPanel(sections);
    // "enter" is the longest key (5 chars). Render pads to 5+2=7 chars.
    const children = (node as any).children;
    // Find a key text node ("j/k" padded to 7 + trailing spaces).
    for (const child of children) {
      if (child.type !== "row") continue;
      const keyCell = child.children?.[1];
      if (keyCell?.text?.includes("j/k")) {
        // padEnd(5+2) means "j/k" + 4 spaces + 2 trailing.
        expect(keyCell.text.length).toBe(7);
      }
    }
  });
});
