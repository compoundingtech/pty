// Extended ANSI rendering utilities for threadsafe TUI experiments

const ESC = "\x1b";

// --- Screen management ---
export function clearScreen(): string { return `${ESC}[2J${ESC}[H`; }
export function hideCursor(): string { return `${ESC}[?25l`; }
export function showCursor(): string { return `${ESC}[?25h`; }
export function moveTo(row: number, col: number): string { return `${ESC}[${row};${col}H`; }

// --- True-color support ---
export function fg(r: number, g: number, b: number): string { return `${ESC}[38;2;${r};${g};${b}m`; }
export function bg(r: number, g: number, b: number): string { return `${ESC}[48;2;${r};${g};${b}m`; }
export function reset(): string { return `${ESC}[0m`; }

// --- Text styles ---
export function bold(s: string): string { return `${ESC}[1m${s}${ESC}[22m`; }
export function dim(s: string): string { return `${ESC}[2m${s}${ESC}[22m`; }
export function italic(s: string): string { return `${ESC}[3m${s}${ESC}[23m`; }
export function underline(s: string): string { return `${ESC}[4m${s}${ESC}[24m`; }
export function inverse(s: string): string { return `${ESC}[7m${s}${ESC}[27m`; }

// Raw style codes
export const BOLD = `${ESC}[1m`;
export const DIM = `${ESC}[2m`;
export const RESET = `${ESC}[0m`;

// --- ANSI stripping ---
const ANSI_RE = /\x1b\[[0-9;]*[a-zA-Z]/g;
export function stripAnsi(text: string): string { return text.replace(ANSI_RE, ""); }

// --- Character width (wcwidth-style) ---
// Returns the display cell width of a single character in a terminal.
// Most characters are 1 cell. East Asian wide chars and certain emoji are 2 cells.
export function charWidth(ch: string): number {
  const code = ch.codePointAt(0);
  if (code === undefined) return 0;
  // Control characters
  if (code < 0x20) return 0;
  // Standard ASCII + most Latin/symbols
  if (code < 0x2500) return 1;
  // Box drawing U+2500–U+257F: 1 cell
  if (code >= 0x2500 && code <= 0x257F) return 1;
  // Block elements U+2580–U+259F: 1 cell
  if (code >= 0x2580 && code <= 0x259F) return 1;
  // Geometric shapes U+25A0–U+25FF: 1 cell
  if (code >= 0x25A0 && code <= 0x25FF) return 1;
  // Miscellaneous Symbols U+2600–U+26FF: mostly 1, but specific emoji are 2 cells wide
  if (code >= 0x2600 && code <= 0x26FF) {
    // These specific codepoints render as 2 cells in most terminals (emoji presentation)
    if (code >= 0x2614 && code <= 0x2615) return 2; // ☔☕
    if (code >= 0x2648 && code <= 0x2653) return 2; // zodiac signs
    if (code === 0x267F) return 2; // ♿
    if (code === 0x2693) return 2; // ⚓
    if (code >= 0x26A0 && code <= 0x26A1) return 2; // ⚠⚡
    if (code >= 0x26AA && code <= 0x26AB) return 2; // ⚪⚫
    if (code >= 0x26BD && code <= 0x26BE) return 2; // ⚽⚾
    if (code >= 0x26C4 && code <= 0x26C5) return 2; // ⛄⛅
    if (code === 0x26D4) return 2; // ⛔
    if (code === 0x26EA) return 2; // ⛪
    if (code >= 0x26F2 && code <= 0x26F3) return 2; // ⛲⛳
    if (code === 0x26F5) return 2; // ⛵
    if (code === 0x26FA) return 2; // ⛺
    if (code === 0x26FD) return 2; // ⛽
    return 1;
  }
  // Dingbats U+2700–U+27BF: 1 cell
  if (code >= 0x2700 && code <= 0x27BF) return 1;
  // CJK Unified Ideographs and related
  if (code >= 0x2E80 && code <= 0x9FFF) return 2;
  if (code >= 0xAC00 && code <= 0xD7AF) return 2; // Hangul
  if (code >= 0xF900 && code <= 0xFAFF) return 2;
  if (code >= 0xFE10 && code <= 0xFE6F) return 2;
  if (code >= 0xFF01 && code <= 0xFF60) return 2; // Fullwidth forms
  if (code >= 0xFFE0 && code <= 0xFFE6) return 2;
  if (code >= 0x1F000 && code <= 0x1FBFF) return 2; // All emoji in supplementary planes
  if (code >= 0x20000 && code <= 0x3FFFF) return 2; // CJK extensions
  return 1;
}

export function visibleLength(text: string): number {
  const plain = stripAnsi(text);
  let w = 0;
  for (const ch of plain) {
    w += charWidth(ch);
  }
  return w;
}

// --- Text utilities ---
export function truncate(text: string, maxWidth: number): string {
  if (visibleLength(text) <= maxWidth) return text;
  if (maxWidth <= 1) return text.slice(0, maxWidth);
  // Build up result character by character, tracking display width
  let w = 0;
  let result = "";
  for (const ch of text) {
    const cw = charWidth(ch);
    if (w + cw + 1 > maxWidth) break; // +1 for the ellipsis
    result += ch;
    w += cw;
  }
  return result + "\u2026";
}

/**
 * Soft-wrap text into multiple visual lines that fit within `maxWidth` columns.
 * Breaks at word boundaries (spaces) when possible, falling back to character-
 * boundary breaking only when a single word exceeds the line width. CJK
 * characters (charWidth === 2) are always valid break points — a line can
 * break after any CJK character.
 *
 * Break semantics: when breaking at a space, the break happens BEFORE the
 * space so the space appears at the start of the next line.  This preserves
 * the invariant that concatenating all output lines reproduces the original
 * text exactly, keeping offset tracking simple.
 *
 * Returns at least one line, plus the starting code-point offset of each line
 * for span splitting.
 */
export function wrapText(text: string, maxWidth: number): { lines: string[]; offsets: number[] } {
  if (maxWidth <= 0) return { lines: [text], offsets: [0] };
  const lines: string[] = [];
  const offsets: number[] = [];

  // Expand code points into an array for easy indexing
  const chars = [...text];
  const len = chars.length;
  let pos = 0;

  while (pos < len) {
    let lineWidth = 0;
    let lineEnd = pos;
    let lastBreak = -1; // code-point index where we can break (exclusive end of line)

    while (lineEnd < len) {
      const ch = chars[lineEnd];
      const w = charWidth(ch);

      if (lineWidth + w > maxWidth) break; // this char would overflow

      // A space is a break opportunity BEFORE it (so the space goes to next line)
      if (ch === " " && lineEnd > pos) {
        lastBreak = lineEnd;
      }

      lineWidth += w;
      lineEnd++;

      // CJK / wide chars: can break AFTER them
      if (w >= 2) {
        lastBreak = lineEnd;
      }
    }

    if (lineEnd >= len) {
      // Rest of text fits on this line
      lines.push(chars.slice(pos, len).join(""));
      offsets.push(pos);
      break;
    }

    // Overflow — pick a break point
    if (lastBreak > pos) {
      lines.push(chars.slice(pos, lastBreak).join(""));
      offsets.push(pos);
      pos = lastBreak;
    } else {
      // No word break available — force character break
      if (lineEnd === pos) lineEnd++; // guarantee forward progress
      lines.push(chars.slice(pos, lineEnd).join(""));
      offsets.push(pos);
      pos = lineEnd;
    }
  }

  if (lines.length === 0) {
    lines.push("");
    offsets.push(0);
  }

  return { lines, offsets };
}

export function pad(text: string, width: number, align: "left" | "right" | "center" = "left"): string {
  const len = visibleLength(text);
  if (len >= width) return text;
  const p = width - len;
  if (align === "right") return " ".repeat(p) + text;
  if (align === "center") return " ".repeat(Math.floor(p / 2)) + text + " ".repeat(Math.ceil(p / 2));
  return text + " ".repeat(p);
}

// --- Convenience ---
export function writeAt(row: number, col: number, text: string): string {
  return moveTo(row, col) + text;
}

export function fillRect(row: number, col: number, width: number, height: number, char = " "): string {
  let out = "";
  const line = char.repeat(width);
  for (let r = 0; r < height; r++) {
    out += moveTo(row + r, col) + line;
  }
  return out;
}

export function fillLine(row: number, col: number, width: number, char = " "): string {
  return moveTo(row, col) + char.repeat(width);
}

// --- Box drawing ---
export type BoxStyle = "rounded" | "sharp" | "double" | "heavy";

interface BoxChars {
  tl: string; tr: string; bl: string; br: string;
  h: string; v: string; lj: string; rj: string;
}

const BOX: Record<BoxStyle, BoxChars> = {
  rounded: { tl: "\u256d", tr: "\u256e", bl: "\u2570", br: "\u256f", h: "\u2500", v: "\u2502", lj: "\u251c", rj: "\u2524" },
  sharp:   { tl: "\u250c", tr: "\u2510", bl: "\u2514", br: "\u2518", h: "\u2500", v: "\u2502", lj: "\u251c", rj: "\u2524" },
  double:  { tl: "\u2554", tr: "\u2557", bl: "\u255a", br: "\u255d", h: "\u2550", v: "\u2551", lj: "\u2560", rj: "\u2563" },
  heavy:   { tl: "\u250f", tr: "\u2513", bl: "\u2517", br: "\u251b", h: "\u2501", v: "\u2503", lj: "\u2523", rj: "\u252b" },
};

export function boxChars(style: BoxStyle = "rounded"): BoxChars { return BOX[style]; }

export function drawBox(
  row: number, col: number, width: number, height: number,
  opts: { style?: BoxStyle; title?: string; fill?: boolean } = {}
): string {
  const c = BOX[opts.style ?? "rounded"];
  let out = "";

  // Top border
  let top = c.h.repeat(width - 2);
  if (opts.title) {
    const tLen = visibleLength(opts.title) + 2;
    if (tLen < width - 4) {
      const rest = width - 2 - tLen - 1;
      top = c.h + " " + opts.title + " " + c.h.repeat(rest);
    }
  }
  out += moveTo(row, col) + c.tl + top + c.tr;

  // Sides + optional fill
  for (let r = 1; r < height - 1; r++) {
    out += moveTo(row + r, col) + c.v;
    if (opts.fill) out += " ".repeat(width - 2);
    out += moveTo(row + r, col + width - 1) + c.v;
  }

  // Bottom
  out += moveTo(row + height - 1, col) + c.bl + c.h.repeat(width - 2) + c.br;
  return out;
}

export function hSep(row: number, col: number, width: number, style: BoxStyle = "rounded"): string {
  const c = BOX[style];
  return moveTo(row, col) + c.lj + c.h.repeat(width - 2) + c.rj;
}

// --- Progress bar ---
export function progressBar(width: number, pct: number): string {
  const filled = Math.round(width * Math.min(1, Math.max(0, pct)));
  return "\u2588".repeat(filled) + "\u2591".repeat(width - filled);
}

// --- Theme ---
type ThemeColor = [number, number, number] | null;

export interface Theme {
  bg1: ThemeColor;
  bg2: ThemeColor;
  bgHi: ThemeColor;
  bgAc: ThemeColor;
  fg1: ThemeColor;
  fg2: ThemeColor;
  fgAc: ThemeColor;
  fgMu: ThemeColor;
  ok: ThemeColor;
  warn: ThemeColor;
  err: ThemeColor;
  info: ThemeColor;
  border: ThemeColor;
}

export const themes: Record<string, Theme> = {
  coolBlue: {
    bg1: [15, 17, 26], bg2: [22, 27, 42], bgHi: [30, 40, 65], bgAc: [40, 80, 140],
    fg1: [210, 218, 235], fg2: [140, 155, 185], fgAc: [100, 160, 255], fgMu: [70, 80, 105],
    ok: [80, 200, 120], warn: [240, 180, 50], err: [240, 80, 80], info: [80, 170, 240],
    border: [50, 60, 85],
  },
  warmAmber: {
    bg1: [24, 18, 12], bg2: [36, 28, 18], bgHi: [55, 42, 25], bgAc: [120, 80, 30],
    fg1: [235, 220, 195], fg2: [180, 160, 130], fgAc: [255, 190, 60], fgMu: [100, 85, 60],
    ok: [120, 200, 80], warn: [255, 200, 80], err: [220, 80, 60], info: [100, 180, 220],
    border: [80, 65, 40],
  },
  mono: {
    bg1: [18, 18, 18], bg2: [28, 28, 28], bgHi: [48, 48, 48], bgAc: [70, 70, 70],
    fg1: [220, 220, 220], fg2: [160, 160, 160], fgAc: [255, 255, 255], fgMu: [90, 90, 90],
    ok: [160, 220, 160], warn: [220, 200, 130], err: [220, 140, 140], info: [140, 180, 220],
    border: [60, 60, 60],
  },
  dracula: {
    bg1: [40, 42, 54], bg2: [50, 52, 68], bgHi: [68, 71, 90], bgAc: [98, 114, 164],
    fg1: [248, 248, 242], fg2: [189, 147, 249], fgAc: [139, 233, 253], fgMu: [98, 114, 164],
    ok: [80, 250, 123], warn: [241, 250, 140], err: [255, 85, 85], info: [139, 233, 253],
    border: [80, 83, 105],
  },
  forest: {
    bg1: [12, 20, 14], bg2: [18, 32, 22], bgHi: [28, 48, 32], bgAc: [40, 80, 50],
    fg1: [200, 225, 205], fg2: [140, 175, 150], fgAc: [100, 220, 130], fgMu: [65, 90, 70],
    ok: [80, 230, 120], warn: [230, 200, 80], err: [230, 90, 80], info: [80, 190, 220],
    border: [45, 65, 48],
  },
  // --- Light variants ---
  coolBlueLight: {
    bg1: [240, 244, 250], bg2: [230, 236, 245], bgHi: [210, 220, 238], bgAc: [70, 120, 200],
    fg1: [30, 35, 50], fg2: [80, 90, 115], fgAc: [40, 100, 220], fgMu: [140, 150, 175],
    ok: [30, 140, 70], warn: [180, 120, 0], err: [200, 40, 40], info: [30, 120, 200],
    border: [180, 190, 210],
  },
  warmAmberLight: {
    bg1: [252, 245, 235], bg2: [242, 232, 218], bgHi: [230, 215, 195], bgAc: [180, 130, 50],
    fg1: [50, 40, 30], fg2: [100, 80, 55], fgAc: [190, 120, 10], fgMu: [150, 135, 110],
    ok: [40, 150, 40], warn: [200, 140, 0], err: [190, 50, 30], info: [50, 130, 180],
    border: [200, 185, 160],
  },
  monoLight: {
    bg1: [245, 245, 245], bg2: [235, 235, 235], bgHi: [215, 215, 215], bgAc: [180, 180, 180],
    fg1: [30, 30, 30], fg2: [80, 80, 80], fgAc: [0, 0, 0], fgMu: [150, 150, 150],
    ok: [40, 140, 40], warn: [170, 140, 20], err: [180, 50, 50], info: [40, 120, 180],
    border: [190, 190, 190],
  },
  draculaLight: {
    bg1: [248, 248, 242], bg2: [238, 236, 230], bgHi: [220, 218, 210], bgAc: [150, 140, 200],
    fg1: [40, 42, 54], fg2: [100, 70, 180], fgAc: [50, 160, 180], fgMu: [140, 150, 170],
    ok: [30, 170, 70], warn: [180, 170, 40], err: [210, 50, 50], info: [50, 160, 180],
    border: [200, 198, 190],
  },
  forestLight: {
    bg1: [242, 250, 244], bg2: [230, 242, 232], bgHi: [210, 230, 215], bgAc: [70, 140, 85],
    fg1: [25, 45, 30], fg2: [60, 100, 70], fgAc: [40, 160, 70], fgMu: [130, 155, 135],
    ok: [30, 160, 60], warn: [170, 140, 20], err: [190, 50, 40], info: [40, 130, 170],
    border: [175, 200, 180],
  },
  terminal: {
    bg1: null, bg2: null, bgHi: null, bgAc: null,
    fg1: null, fg2: null, fgAc: null, fgMu: null,
    ok: null, warn: null, err: null, info: null,
    border: null,
  },
};

// Quick access to theme color codes
function fgOr(c: ThemeColor): string { return c ? fg(c[0], c[1], c[2]) : ""; }
function bgOr(c: ThemeColor): string { return c ? bg(c[0], c[1], c[2]) : ""; }

export function c(theme: Theme) {
  return {
    bg1: bgOr(theme.bg1), bg2: bgOr(theme.bg2), bgHi: bgOr(theme.bgHi), bgAc: bgOr(theme.bgAc),
    fg1: fgOr(theme.fg1), fg2: fgOr(theme.fg2), fgAc: fgOr(theme.fgAc), fgMu: fgOr(theme.fgMu),
    ok: fgOr(theme.ok), warn: fgOr(theme.warn), err: fgOr(theme.err), info: fgOr(theme.info),
    border: fgOr(theme.border),
    bgOk: bgOr(theme.ok), bgWarn: bgOr(theme.warn), bgErr: bgOr(theme.err), bgInfo: bgOr(theme.info),
  };
}

// Init screen: clear + fill with bg1
export function initScreen(rows: number, cols: number, theme: Theme): string {
  const clr = c(theme);
  return hideCursor() + clearScreen() + clr.bg1 + clr.fg1 + fillRect(1, 1, cols, rows) + reset();
}

// Title bar across top
export function titleBar(cols: number, left: string, right: string, theme: Theme): string {
  const clr = c(theme);
  let o = clr.bgAc + clr.fg1;
  o += fillLine(1, 1, cols);
  o += writeAt(1, 3, bold(left));
  if (right) o += writeAt(1, cols - visibleLength(right) - 2, right);
  o += reset();
  return o;
}

// Footer bar across bottom
export function footerBar(row: number, cols: number, text: string, theme: Theme): string {
  const clr = c(theme);
  return clr.bg1 + clr.fgMu + writeAt(row, 2, text) + reset();
}

// Draw a panel: filled box with title
export function panel(
  row: number, col: number, w: number, h: number,
  title: string, theme: Theme, style: BoxStyle = "rounded"
): string {
  const clr = c(theme);
  let o = clr.bg2 + fillRect(row, col, w, h);
  o += clr.border + drawBox(row, col, w, h, { style, fill: false });
  if (title) {
    o += writeAt(row, col + 2, " " + clr.fgAc + bold(title) + reset() + clr.bg2 + clr.border + " ");
  }
  return o;
}

// Write content line inside a panel
export function panelLine(row: number, col: number, text: string): string {
  return writeAt(row, col + 2, text);
}

// Ask-anything input bar (boxed, 3 rows tall)
export function askBar(
  row: number, col: number, width: number,
  theme: Theme, agentContext: string, style: BoxStyle = "rounded"
): string {
  const clr = c(theme);
  let o = clr.bg2 + fillRect(row, col, width, 3);
  o += clr.border + drawBox(row, col, width, 3, { style });
  o += writeAt(row + 1, col + 3, clr.fgMu + "> Ask anything...");
  o += writeAt(row + 1, col + width - visibleLength(agentContext) - 3, clr.fg2 + agentContext);
  return o;
}

// Compact ask bar (1 row, no box — for small terminals)
export function askBarCompact(
  row: number, col: number, width: number,
  theme: Theme, agentContext: string
): string {
  const clr = c(theme);
  let o = clr.bg2 + fillLine(row, col, width);
  o += writeAt(row, col + 2, clr.fgMu + "> Ask anything...");
  o += writeAt(row, col + width - visibleLength(agentContext) - 2, clr.fg2 + agentContext);
  return o;
}

// Agent activity status line
export function agentActivity(row: number, col: number, items: [string, string, string][], theme: Theme): string {
  const clr = c(theme);
  let o = clr.bg1;
  let text = "";
  items.forEach(([name, status, color], i) => {
    const colorCode = color === "ok" ? clr.ok : color === "warn" ? clr.warn : color === "info" ? clr.info : clr.fgMu;
    if (i > 0) text += clr.fgMu + "  \u2502  ";
    text += colorCode + "\u25cf " + clr.fg2 + name + ": " + clr.fgMu + status;
  });
  o += writeAt(row, col, text);
  return o;
}
