// Markdown renderer — parses a subset of CommonMark and returns UINodes.
//
// Subset supported (v1):
//   - Headings (#, ##, ###, ####)
//   - Paragraphs (word-wrapped via `wrapText`)
//   - Bold (**x**), italic (*x*), inline code (`x`), links ([text](url))
//   - Fenced code blocks (```) — dim color, no syntax highlighting
//   - Unordered lists (-, *) and ordered lists (1.)
//   - Task lists (- [ ], - [x])
//   - Blockquotes (> line)
//   - Horizontal rules (---, ***)
//
// Deliberately NOT supported:
//   - Tables (too column-layout-dependent for terminal; can add later)
//   - HTML passthrough (security + rendering both complicated)
//   - Nested lists (would need indentation tracking; can add later)

import { text, row, separator, indent, checkbox, spacer, column } from "../builders.ts";
import type { UINode, Color } from "../nodes.ts";

export interface MarkdownOptions {
  /** Maximum width the renderer should assume when word-wrapping paragraphs.
   *  If omitted, paragraphs are not wrapped — useful when the renderer is
   *  nested inside a framework that does its own wrapping. */
  width?: number;
}

interface Block {
  kind:
    | "heading" | "paragraph" | "code" | "bullet" | "ordered"
    | "task" | "quote" | "hr";
  /** heading level (1-4) */
  level?: number;
  /** raw text for paragraph / heading / quote */
  text?: string;
  /** lines inside a code block */
  lines?: string[];
  /** items inside a list block */
  items?: string[];
  /** task items: [done, text] pairs for tasks */
  tasks?: { done: boolean; text: string }[];
}

const HEADING_RE = /^(#{1,4})\s+(.*)$/;
const BULLET_RE = /^[-*]\s+(.*)$/;
const ORDERED_RE = /^\d+\.\s+(.*)$/;
const TASK_RE = /^[-*]\s+\[([ xX])\]\s+(.*)$/;
const QUOTE_RE = /^>\s?(.*)$/;
const FENCE_RE = /^```/;
const HR_RE = /^(-{3,}|\*{3,}|_{3,})\s*$/;

/** Split the source into a list of block-level descriptors. This is the
 *  only parsing pass — rendering is then purely structural. */
export function parseMarkdown(source: string): Block[] {
  const lines = source.split("\n");
  const blocks: Block[] = [];
  let para: string[] = [];

  const flushPara = () => {
    if (para.length === 0) return;
    blocks.push({ kind: "paragraph", text: para.join(" ") });
    para = [];
  };

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];

    // Fenced code — consume until the closing fence.
    if (FENCE_RE.test(line)) {
      flushPara();
      const codeLines: string[] = [];
      i++;
      while (i < lines.length && !FENCE_RE.test(lines[i])) {
        codeLines.push(lines[i]);
        i++;
      }
      blocks.push({ kind: "code", lines: codeLines });
      continue;
    }

    // Blank line — ends the current paragraph.
    if (line.trim() === "") {
      flushPara();
      continue;
    }

    // Horizontal rule.
    if (HR_RE.test(line)) {
      flushPara();
      blocks.push({ kind: "hr" });
      continue;
    }

    // Heading.
    const h = HEADING_RE.exec(line);
    if (h) {
      flushPara();
      blocks.push({ kind: "heading", level: h[1].length, text: h[2] });
      continue;
    }

    // Task item (must match BEFORE bullet since task is a bullet superset).
    const t = TASK_RE.exec(line);
    if (t) {
      flushPara();
      const prev = blocks[blocks.length - 1];
      if (prev && prev.kind === "task") {
        prev.tasks!.push({ done: t[1] !== " ", text: t[2] });
      } else {
        blocks.push({ kind: "task", tasks: [{ done: t[1] !== " ", text: t[2] }] });
      }
      continue;
    }

    // Bullet list.
    const b = BULLET_RE.exec(line);
    if (b) {
      flushPara();
      const prev = blocks[blocks.length - 1];
      if (prev && prev.kind === "bullet") prev.items!.push(b[1]);
      else blocks.push({ kind: "bullet", items: [b[1]] });
      continue;
    }

    // Ordered list.
    const o = ORDERED_RE.exec(line);
    if (o) {
      flushPara();
      const prev = blocks[blocks.length - 1];
      if (prev && prev.kind === "ordered") prev.items!.push(o[1]);
      else blocks.push({ kind: "ordered", items: [o[1]] });
      continue;
    }

    // Blockquote.
    const q = QUOTE_RE.exec(line);
    if (q) {
      flushPara();
      const prev = blocks[blocks.length - 1];
      if (prev && prev.kind === "quote") prev.text! += "\n" + q[1];
      else blocks.push({ kind: "quote", text: q[1] });
      continue;
    }

    // Default — part of a paragraph.
    para.push(line);
  }
  flushPara();
  return blocks;
}

/** Split a line of text into inline styled segments — bold, italic, code,
 *  link. Returns alternating plain+styled chunks as children of a row.
 *  Simplified inline grammar; good enough for most content, keeps the
 *  renderer output predictable. */
interface InlineSegment {
  text: string;
  bold?: boolean;
  italic?: boolean;
  code?: boolean;
  url?: string;
}

export function parseInline(src: string): InlineSegment[] {
  const out: InlineSegment[] = [];
  let i = 0;
  let buf = "";
  const flushBuf = () => {
    if (buf.length > 0) { out.push({ text: buf }); buf = ""; }
  };

  while (i < src.length) {
    // Links: [text](url)
    if (src[i] === "[") {
      const end = src.indexOf("]", i + 1);
      if (end !== -1 && src[end + 1] === "(") {
        const urlEnd = src.indexOf(")", end + 2);
        if (urlEnd !== -1) {
          flushBuf();
          out.push({ text: src.slice(i + 1, end), url: src.slice(end + 2, urlEnd) });
          i = urlEnd + 1;
          continue;
        }
      }
    }
    // Inline code: `x`
    if (src[i] === "`") {
      const end = src.indexOf("`", i + 1);
      if (end !== -1) {
        flushBuf();
        out.push({ text: src.slice(i + 1, end), code: true });
        i = end + 1;
        continue;
      }
    }
    // Bold / italic. When we see `*`, decide which one we're looking for.
    // Unmatched delimiters emit as plain text (via buf) rather than eating
    // the next char on a mis-paired fallback.
    if (src[i] === "*") {
      if (src[i + 1] === "*") {
        const end = src.indexOf("**", i + 2);
        if (end !== -1) {
          flushBuf();
          out.push({ text: src.slice(i + 2, end), bold: true });
          i = end + 2;
          continue;
        }
        buf += "**";
        i += 2;
        continue;
      }
      const end = src.indexOf("*", i + 1);
      if (end !== -1) {
        flushBuf();
        out.push({ text: src.slice(i + 1, end), italic: true });
        i = end + 1;
        continue;
      }
      buf += "*";
      i++;
      continue;
    }
    buf += src[i];
    i++;
  }
  flushBuf();
  return out;
}

function renderInline(segments: InlineSegment[], baseColor: Color = "primary"): UINode[] {
  return segments.map(s => {
    if (s.code) return text(s.text, "accent", { italic: false });
    if (s.url) return text(`${s.text} `, "accent");
    return text(s.text, baseColor, { bold: !!s.bold, italic: !!s.italic });
  });
}

function headingColor(level: number): Color {
  switch (level) {
    case 1: return "accent";
    case 2: return "primary";
    case 3: return "accent";
    default: return "muted";
  }
}

function wrapLine(src: string, width?: number): string[] {
  if (!width || width <= 0) return [src];
  // Simple word wrap that keeps whole words together.
  const words = src.split(/(\s+)/);
  const lines: string[] = [];
  let line = "";
  for (const w of words) {
    if ((line + w).length <= width) { line += w; continue; }
    if (line.length > 0) { lines.push(line.trimEnd()); line = ""; }
    if (w.length > width) {
      // Unbreakable word longer than width — hard-split.
      for (let i = 0; i < w.length; i += width) lines.push(w.slice(i, i + width));
    } else if (w.trim().length > 0) {
      line = w;
    }
  }
  if (line.length > 0) lines.push(line.trimEnd());
  return lines;
}

/** Render markdown source as an array of UINodes ready for a panel/column. */
export function renderMarkdown(source: string, opts: MarkdownOptions = {}): UINode[] {
  const blocks = parseMarkdown(source);
  const out: UINode[] = [];

  for (let bi = 0; bi < blocks.length; bi++) {
    const b = blocks[bi];
    // Visual spacing between blocks: add a blank row between non-first blocks
    // that aren't tightly related (lists, tasks accept internal spacing).
    if (bi > 0) out.push(row(spacer()));

    switch (b.kind) {
      case "heading": {
        const c = headingColor(b.level!);
        out.push(row(
          text("#".repeat(b.level!) + " ", "muted", { dim: true }),
          ...renderInline(parseInline(b.text!), c).map((n, i) => {
            // First inline in heading is bold; subsequent inherit their own style.
            if (i === 0 && (n as any).type === "text") {
              return text((n as any).text, c, { bold: true });
            }
            return n;
          }),
        ));
        break;
      }
      case "paragraph": {
        const wrapped = wrapLine(b.text ?? "", opts.width);
        for (const ln of wrapped) out.push(row(...renderInline(parseInline(ln))));
        break;
      }
      case "code": {
        for (const ln of b.lines!) {
          out.push(row(text("  " + ln, "muted", { dim: false })));
        }
        break;
      }
      case "bullet": {
        for (const item of b.items!) {
          out.push(row(text("  • ", "muted"), ...renderInline(parseInline(item))));
        }
        break;
      }
      case "ordered": {
        b.items!.forEach((item, i) => {
          out.push(row(
            text(`  ${i + 1}. `, "muted"),
            ...renderInline(parseInline(item)),
          ));
        });
        break;
      }
      case "task": {
        for (const t of b.tasks!) {
          out.push(row(
            text("  ", "muted"),
            checkbox(t.done, t.done ? "muted" : "accent"),
            text(" ", "muted"),
            ...renderInline(parseInline(t.text), t.done ? "muted" : "primary"),
          ));
        }
        break;
      }
      case "quote": {
        for (const ln of (b.text ?? "").split("\n")) {
          const wrapped = wrapLine(ln, opts.width ? opts.width - 2 : undefined);
          for (const w of wrapped) {
            out.push(row(text("\u2502 ", "muted"), ...renderInline(parseInline(w), "muted")));
          }
        }
        break;
      }
      case "hr":
        out.push(separator());
        break;
    }
  }
  return out;
}
