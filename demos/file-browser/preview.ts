// Preview rendering: wraps long lines with gutter-aligned line numbers
// and optional syntax highlighting (e.g. markdown headings).
import {
  text, wrapText, visibleLength,
  type UINode, type Span,
} from "../../src/tui/index.ts";

const GUTTER_WIDTH = 7; // "   1 │ " = 7 chars

/** Compute the content width available for text after the gutter. */
export function previewContentWidth(cols: number, treeWidth: number): number {
  const panelWidth = cols - treeWidth;
  return Math.max(1, panelWidth - 4 - GUTTER_WIDTH);
}

/**
 * Build preview nodes: each source line becomes one or more text nodes.
 * The first visual line gets a line number gutter. Continuation lines get
 * a blank gutter of the same width so content stays aligned.
 */
export function buildPreviewNodes(
  lines: string[],
  startLine: number,
  contentWidth: number,
  highlighter?: (content: string) => Span[],
): UINode[] {
  const nodes: UINode[] = [];

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const lineNum = String(startLine + i + 1).padStart(4, " ");
    const gutterPrefix = lineNum + " \u2502 ";
    const blankPrefix = " ".repeat(GUTTER_WIDTH);

    if (contentWidth > 0 && visibleLength(line) > contentWidth) {
      const { lines: wrapped } = wrapText(line, contentWidth);
      for (let w = 0; w < wrapped.length; w++) {
        const prefix = w === 0 ? gutterPrefix : blankPrefix;
        const content = wrapped[w];
        nodes.push(text(prefix + content, "primary", {
          truncate: true,
          highlight: lineHighlight(prefix, content, w === 0, highlighter),
        }));
      }
    } else {
      nodes.push(text(gutterPrefix + line, "primary", {
        truncate: true,
        highlight: lineHighlight(gutterPrefix, line, true, highlighter),
      }));
    }
  }

  return nodes;
}

/** Build a highlight callback for a single preview line. */
function lineHighlight(
  prefix: string,
  content: string,
  hasLineNumber: boolean,
  highlighter?: (content: string) => Span[],
): (fullText: string) => Span[] {
  return () => {
    const prefixLen = [...prefix].length;
    const spans: Span[] = [];

    if (hasLineNumber) {
      spans.push({ start: 0, end: prefixLen - 2, color: "muted" as const });
      spans.push({ start: prefixLen - 2, end: prefixLen, color: "border" as const });
    } else {
      spans.push({ start: 0, end: prefixLen, color: "muted" as const });
    }

    if (highlighter) {
      for (const s of highlighter(content)) {
        spans.push({ ...s, start: s.start + prefixLen, end: s.end + prefixLen });
      }
    }

    return spans;
  };
}

// --- Markdown highlighter ---

export function markdownHighlight(content: string): Span[] {
  const len = [...content].length;
  if (content.startsWith("#")) {
    return [{ start: 0, end: len, bold: true, color: "accent" as const }];
  }
  if (content.startsWith("---")) {
    return [{ start: 0, end: len, dim: true, color: "muted" as const }];
  }
  if (content.startsWith("- ") || content.startsWith("* ")) {
    return [{ start: 0, end: 2, bold: true, color: "accent" as const }];
  }
  if (content.startsWith("> ")) {
    return [{ start: 0, end: len, italic: true, color: "secondary" as const }];
  }
  return [];
}

export function isMarkdown(filename: string): boolean {
  return filename.endsWith(".md") || filename.endsWith(".markdown") || filename.endsWith(".mdx");
}
