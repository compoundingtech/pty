import { describe, it, expect } from "vitest";
import { promptBar } from "../src/tui/widgets/prompt-bar.ts";
import { createTextArea } from "../src/tui/widgets/text-area.ts";

describe("promptBar", () => {
  it("renders a column of three rows with single-line value and no extras", () => {
    const node = promptBar({ kind: "single", state: { text: "hi", cursor: 2 } });
    expect((node as any).type).toBe("column");
    // separator, input row, separator = 3 children.
    expect((node as any).children).toHaveLength(3);
  });

  it("adds a status row when status has left or right", () => {
    const node = promptBar(
      { kind: "single", state: { text: "", cursor: 0 } },
      { status: { left: "L", right: "R" } },
    );
    expect((node as any).children).toHaveLength(4);
  });

  it("title overlays the top rule with the title text", () => {
    const node = promptBar(
      { kind: "single", state: { text: "", cursor: 0 } },
      { title: { text: "compose", align: "center" } },
    );
    const top = (node as any).children[0];
    // Title becomes a row with rule/title/rule segments.
    expect(top.type).toBe("row");
    const texts = top.children.map((c: any) => c.text).join("");
    expect(texts).toContain("compose");
  });

  it("honours the custom glyph", () => {
    const node = promptBar(
      { kind: "single", state: { text: "", cursor: 0 } },
      { glyph: "$" },
    );
    const inputRow = (node as any).children[1];
    expect(inputRow.children[0].text).toBe(" $ ");
  });

  it("works with a multi-line TextAreaState — one row per line plus title + separator", () => {
    // New shape: each line of the text area gets its own row inside the
    // prompt bar's column. So for "line one\nline two" we have:
    //   title rule, line 1 row, line 2 row, bottom separator = 4 children.
    const node = promptBar(
      { kind: "multi", state: createTextArea("line one\nline two") },
      { title: { text: "chat" } },
    );
    const children = (node as any).children;
    expect(children).toHaveLength(4);
    // The first content row has the prompt glyph as its first child.
    const firstLine = children[1];
    expect(firstLine.type).toBe("row");
    expect(firstLine.children[0].text).toContain("\u276f");
    // Continuation rows are indented (no glyph).
    const secondLine = children[2];
    expect(secondLine.children[0].text).toBe("   ");
  });
});
