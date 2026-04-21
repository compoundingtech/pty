// Prompt bar — a full-width input styled like Claude Code's message box.
//
// Layout:
//     ───────────[title]────────────
//     ❯ the input goes here█
//     ───────────────────────────────
//       status left                 status right
//
// Top and bottom horizontal rules (no side borders, unlike `panel`), a
// prompt glyph, the text field itself, and an optional status strip.
// Works with both single-line (TextFieldState) and multi-line
// (TextAreaState) values — pass whichever you own.

import { row, column, text, separator, spacer } from "../builders.ts";
import type { UINode, ColumnNode, Color } from "../nodes.ts";
import { renderFieldNodes, type TextFieldState } from "./form.ts";
import { renderTextArea, type TextAreaState } from "./text-area.ts";

export type PromptBarValue =
  | { kind: "single"; state: TextFieldState }
  | { kind: "multi";  state: TextAreaState };

export type TitleAlign = "left" | "center" | "right";

export interface PromptBarTitle {
  text: string;
  align?: TitleAlign;
  /** Color for the title text. Defaults to "accent". */
  color?: Color;
}

export interface PromptBarStatus {
  left?: string;
  right?: string;
  /** Color for the status text. Defaults to "muted". */
  color?: Color;
}

export interface PromptBarOptions {
  /** Prompt glyph rendered before the value. Default: "\u276f" (❯). */
  glyph?: string;
  /** Color for the glyph. Default "accent". */
  glyphColor?: Color;
  /** Title overlaid on the top rule. Omit for a plain rule. */
  title?: PromptBarTitle;
  /** Optional status strip rendered under the bottom rule. */
  status?: PromptBarStatus;
  /** Whether the field is focused — controls cursor rendering. Default true. */
  active?: boolean;
}

function titleRule(title: PromptBarTitle | undefined, color: Color = "muted"): UINode {
  if (!title) return separator();
  const label = ` ${title.text} `;
  const align = title.align ?? "left";
  // Separator can't carry embedded text, so we approximate with a row made
  // of three text segments: left-rule, title, right-rule. The layout engine
  // handles width. We use long rule strings that will get clipped to width;
  // for a principled fixed-width build use separator() without a title.
  const ruleChar = "\u2500";
  const leftRule = align === "left"   ? ruleChar.repeat(2)
                 : align === "center" ? ruleChar.repeat(20)
                 :                       ruleChar.repeat(40);
  const rightRule = align === "right"  ? ruleChar.repeat(2)
                  : align === "center" ? ruleChar.repeat(20)
                  :                       ruleChar.repeat(40);
  return row(
    text(leftRule, color, { dim: true }),
    text(label, title.color ?? "accent", { bold: true }),
    text(rightRule, color, { dim: true }),
  );
}

function statusRow(s: PromptBarStatus | undefined): UINode | null {
  if (!s) return null;
  if (!s.left && !s.right) return null;
  const color = s.color ?? "muted";
  const children: UINode[] = [];
  if (s.left)  children.push(text(s.left, color, { dim: true }));
  children.push(spacer());
  if (s.right) children.push(text(s.right + "  ", color, { dim: true }));
  return row(...children);
}

/** Render a prompt bar around `value`. Returns a column of rows — drop it
 *  into a screen's render() array directly. */
export function promptBar(value: PromptBarValue, opts: PromptBarOptions = {}): UINode {
  const glyph = opts.glyph ?? "\u276f";
  const active = opts.active ?? true;
  const glyphColor = opts.glyphColor ?? "accent";

  const children: UINode[] = [titleRule(opts.title)];

  if (value.kind === "multi") {
    // Multi-line: prompt glyph on the FIRST row only; the text area
    // is rendered as its own column of rows below (continued-line
    // rows are indented to line up with the glyph width).
    const lines = value.state.lines;
    const active_ = active;
    lines.forEach((line, i) => {
      const onCursor = active_ && i === value.state.row;
      const prompt = i === 0
        ? text(` ${glyph} `, glyphColor, { bold: true })
        : text(`   `, "muted");
      if (!onCursor) {
        children.push(row(prompt, text(line.length === 0 ? " " : line, "primary")));
      } else {
        const col = value.state.col;
        const before = line.slice(0, col);
        const under  = line.slice(col, col + 1) || " ";
        const after  = line.slice(col + 1);
        children.push(row(
          prompt,
          text(before, "primary"),
          text(under,  "primary", { inverse: true }),
          text(after,  "primary"),
        ));
      }
    });
  } else {
    // Single-line: everything fits on one row.
    const s = value.state;
    children.push(row(
      text(` ${glyph} `, glyphColor, { bold: true }),
      ...renderFieldNodes(s.text, s.cursor, active),
    ));
  }

  children.push(separator());
  const status = statusRow(opts.status);
  if (status) children.push(status);

  const node: ColumnNode = { type: "column", children };
  return node;
}
