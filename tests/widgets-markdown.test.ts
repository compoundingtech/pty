import { describe, it, expect } from "vitest";
import { parseMarkdown, parseInline, renderMarkdown } from "../src/tui/widgets/markdown.ts";

describe("parseMarkdown — blocks", () => {
  it("splits paragraphs on blank lines", () => {
    const b = parseMarkdown("first para\nmore of it\n\nsecond para");
    expect(b).toHaveLength(2);
    expect(b[0].kind).toBe("paragraph");
    expect(b[0].text).toBe("first para more of it");
    expect(b[1].kind).toBe("paragraph");
  });

  it("captures headings with their level", () => {
    const b = parseMarkdown("# h1\n## h2\n### h3\n#### h4");
    expect(b.map(x => x.kind)).toEqual(["heading", "heading", "heading", "heading"]);
    expect(b.map(x => x.level)).toEqual([1, 2, 3, 4]);
    expect(b[0].text).toBe("h1");
  });

  it("captures fenced code blocks as-is", () => {
    const src = "```\nconst x = 1;\nconsole.log(x);\n```";
    const b = parseMarkdown(src);
    expect(b).toHaveLength(1);
    expect(b[0].kind).toBe("code");
    expect(b[0].lines).toEqual(["const x = 1;", "console.log(x);"]);
  });

  it("groups contiguous bullet items into one block", () => {
    const b = parseMarkdown("- a\n- b\n- c");
    expect(b).toHaveLength(1);
    expect(b[0].kind).toBe("bullet");
    expect(b[0].items).toEqual(["a", "b", "c"]);
  });

  it("parses task lists, distinguishing done from not-done", () => {
    const b = parseMarkdown("- [ ] todo\n- [x] done\n- [X] also done");
    expect(b).toHaveLength(1);
    expect(b[0].kind).toBe("task");
    expect(b[0].tasks).toEqual([
      { done: false, text: "todo" },
      { done: true, text: "done" },
      { done: true, text: "also done" },
    ]);
  });

  it("parses ordered lists", () => {
    const b = parseMarkdown("1. first\n2. second");
    expect(b).toHaveLength(1);
    expect(b[0].kind).toBe("ordered");
    expect(b[0].items).toEqual(["first", "second"]);
  });

  it("parses blockquotes, joining multiple > lines", () => {
    const b = parseMarkdown("> one\n> two");
    expect(b).toHaveLength(1);
    expect(b[0].kind).toBe("quote");
    expect(b[0].text).toBe("one\ntwo");
  });

  it("recognises horizontal rules", () => {
    const b = parseMarkdown("para\n\n---\n\nmore");
    expect(b.map(x => x.kind)).toEqual(["paragraph", "hr", "paragraph"]);
  });
});

describe("parseInline — spans", () => {
  it("extracts bold, italic, and inline code", () => {
    const s = parseInline("plain **bold** *italic* `code`");
    expect(s).toEqual([
      { text: "plain " },
      { text: "bold", bold: true },
      { text: " " },
      { text: "italic", italic: true },
      { text: " " },
      { text: "code", code: true },
    ]);
  });

  it("extracts links", () => {
    const s = parseInline("see [docs](https://example.com) now");
    expect(s).toEqual([
      { text: "see " },
      { text: "docs", url: "https://example.com" },
      { text: " now" },
    ]);
  });

  it("leaves unclosed delimiters as plain text", () => {
    const s = parseInline("nothing **here");
    expect(s).toEqual([{ text: "nothing **here" }]);
  });
});

describe("renderMarkdown — smoke", () => {
  it("produces a non-empty node array for a mixed document", () => {
    const nodes = renderMarkdown(
      "# Title\n\nsome **emphasis**\n\n- [ ] a task\n- [x] done\n\n```\ncode here\n```\n\n> a quote\n",
    );
    expect(nodes.length).toBeGreaterThan(0);
  });

  it("respects the width option for paragraph wrapping", () => {
    const long = "one two three four five six seven eight nine ten eleven twelve";
    const wide = renderMarkdown(long);
    const narrow = renderMarkdown(long, { width: 20 });
    expect(narrow.length).toBeGreaterThan(wide.length);
  });
});
