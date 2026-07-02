// CellBuffer: ANSI string → cell grid, with diff-based rendering

import { type Cell, emptyCell, cellsEqual } from "./types.ts";
import { charWidth } from "./colors.ts";

export class CellBuffer {
  rows: number;
  cols: number;
  cells: Cell[][];

  constructor(rows: number, cols: number) {
    this.rows = rows;
    this.cols = cols;
    this.cells = [];
    for (let r = 0; r < rows; r++) {
      const row: Cell[] = [];
      for (let c = 0; c < cols; c++) {
        row.push(emptyCell());
      }
      this.cells.push(row);
    }
  }

  clear(): void {
    for (let r = 0; r < this.rows; r++) {
      for (let c = 0; c < this.cols; c++) {
        this.cells[r][c] = emptyCell();
      }
    }
  }

  getCell(row: number, col: number): Cell | undefined {
    return this.cells[row]?.[col];
  }

  setCell(row: number, col: number, cell: Cell): void {
    if (row >= 0 && row < this.rows && col >= 0 && col < this.cols) {
      this.cells[row][col] = cell;
    }
  }

  /** Parse an ANSI string and write it into the buffer.
   *  Coordinates are 1-based (matching terminal conventions from render.ts moveTo).
   */
  writeAnsi(ansi: string): void {
    // Parser state
    let curRow = 0; // 0-based internal
    let curCol = 0;
    let fgColor: [number, number, number] | null = null;
    let bgColor: [number, number, number] | null = null;
    // Palette index tracking — populated on indexed SGR (30-37 / 90-97 /
    // 38;5;N / 48;5;N), nulled on truecolor SGR (38;2) and resets (0, 39, 49).
    let fgIndex: number | null = null;
    let bgIndex: number | null = null;
    let isBold = false;
    let isDim = false;
    let isItalic = false;
    let isUnderline = false;

    let i = 0;
    while (i < ansi.length) {
      if (ansi[i] === "\x1b") {
        if (ansi[i + 1] === "[") {
          // CSI sequence — params may include private prefix chars like ? > < =
          i += 2;
          let params = "";
          while (i < ansi.length && ((ansi[i] >= "0" && ansi[i] <= "9") || ansi[i] === ";" || ansi[i] === "?" || ansi[i] === ">" || ansi[i] === "<" || ansi[i] === "=" || ansi[i] === " ")) {
            params += ansi[i];
            i++;
          }
          const cmd = ansi[i] ?? "";
          i++;

          if (cmd === "m") {
            // SGR - colors/attributes
            const parts = params ? params.split(";").map(Number) : [0];
            let j = 0;
            while (j < parts.length) {
              const p = parts[j]!;
              if (p === 0) {
                fgColor = null; bgColor = null;
                fgIndex = null; bgIndex = null;
                isBold = false; isDim = false; isItalic = false; isUnderline = false;
              } else if (p === 1) isBold = true;
              else if (p === 2) isDim = true;
              else if (p === 3) isItalic = true;
              else if (p === 4) isUnderline = true;
              else if (p === 22) { isBold = false; isDim = false; }
              else if (p === 23) isItalic = false;
              else if (p === 24) isUnderline = false;
              else if (p === 27) { /* reset inverse - ignore */ }
              else if (p === 39) { fgColor = null; fgIndex = null; }
              else if (p === 49) { bgColor = null; bgIndex = null; }
              else if (p === 38 && parts[j + 1] === 2) {
                fgColor = [parts[j + 2] ?? 0, parts[j + 3] ?? 0, parts[j + 4] ?? 0];
                fgIndex = null; // truecolor has no palette index
                j += 4;
              } else if (p === 48 && parts[j + 1] === 2) {
                bgColor = [parts[j + 2] ?? 0, parts[j + 3] ?? 0, parts[j + 4] ?? 0];
                bgIndex = null;
                j += 4;
              } else if (p === 38 && parts[j + 1] === 5) {
                const n = parts[j + 2] ?? 0;
                fgColor = ansi256ToRgb(n);
                fgIndex = n;
                j += 2;
              } else if (p === 48 && parts[j + 1] === 5) {
                const n = parts[j + 2] ?? 0;
                bgColor = ansi256ToRgb(n);
                bgIndex = n;
                j += 2;
              } else if (p >= 30 && p <= 37) { fgColor = ansi16ToRgb(p - 30); fgIndex = p - 30; }
              else if (p >= 40 && p <= 47) { bgColor = ansi16ToRgb(p - 40); bgIndex = p - 40; }
              else if (p >= 90 && p <= 97) { fgColor = ansi16ToRgb(p - 90 + 8); fgIndex = p - 90 + 8; }
              else if (p >= 100 && p <= 107) { bgColor = ansi16ToRgb(p - 100 + 8); bgIndex = p - 100 + 8; }
              j++;
            }
          } else if (cmd === "H") {
            // Cursor position: ESC[row;colH (1-based)
            const positions = params ? params.split(";").map(Number) : [1, 1];
            curRow = (positions[0] ?? 1) - 1;
            curCol = (positions[1] ?? 1) - 1;
          } else if (cmd === "J") {
            // Clear screen - we handle this by clearing the buffer
            if (params === "" || params === "2") {
              this.clear();
              curRow = 0;
              curCol = 0;
            }
          } else if (cmd === "K") {
            // Clear line from cursor to end
            for (let c = curCol; c < this.cols; c++) {
              this.cells[curRow]![c] = emptyCell();
            }
          }
          // Skip other CSI commands (cursor show/hide, etc.)
        } else if (ansi[i + 1] === "]") {
          // OSC sequence - skip until ST
          i += 2;
          while (i < ansi.length && ansi[i] !== "\x07" && !(ansi[i] === "\x1b" && ansi[i + 1] === "\\")) i++;
          if (ansi[i] === "\x07") i++;
          else i += 2;
        } else if (ansi[i + 1] === "(" || ansi[i + 1] === ")") {
          // Character set designation - skip
          i += 3;
        } else {
          i += 2; // skip unknown escape
        }
      } else if (ansi[i] === "\n") {
        curRow++;
        curCol = 0;
        i++;
      } else if (ansi[i] === "\r") {
        curCol = 0;
        i++;
      } else {
        // Printable character — may be wide (2 cells) OR a surrogate pair
        // encoding an astral-plane codepoint (emoji, CJK ideographs above
        // U+FFFF). `ansi[i]` returns a single UTF-16 code unit, so a lone
        // high-surrogate for a non-BMP character would be treated as a
        // narrow char — splitting `📬` into two width-1 cells and letting
        // the diff renderer fossilize the halves independently.
        // Combine surrogate pairs into a single Cell before measuring width.
        let ch = ansi[i];
        const code = ch.charCodeAt(0);
        if (code >= 0xd800 && code <= 0xdbff && i + 1 < ansi.length) {
          const low = ansi.charCodeAt(i + 1);
          if (low >= 0xdc00 && low <= 0xdfff) {
            ch = ansi.slice(i, i + 2);
          }
        }
        const cw = charWidth(ch);
        if (curRow >= 0 && curRow < this.rows && curCol >= 0 && curCol < this.cols) {
          this.cells[curRow][curCol] = {
            char: ch,
            fg: fgColor ? [...fgColor] : null,
            bg: bgColor ? [...bgColor] : null,
            fgIndex,
            bgIndex,
            bold: isBold,
            dim: isDim,
            italic: isItalic,
            underline: isUnderline,
          };
          // For wide characters, fill the next cell with an empty placeholder
          if (cw === 2 && curCol + 1 < this.cols) {
            this.cells[curRow][curCol + 1] = {
              char: "",
              fg: fgColor ? [...fgColor] : null,
              bg: bgColor ? [...bgColor] : null,
              fgIndex,
              bgIndex,
              bold: isBold,
              dim: isDim,
              italic: isItalic,
              underline: isUnderline,
            };
          }
        }
        curCol += cw;
        i += ch.length;
      }
    }
  }

  clone(): CellBuffer {
    const buf = new CellBuffer(this.rows, this.cols);
    for (let r = 0; r < this.rows; r++) {
      for (let c = 0; c < this.cols; c++) {
        const cell = this.cells[r][c];
        buf.cells[r][c] = { ...cell, fg: cell.fg ? [...cell.fg] : null, bg: cell.bg ? [...cell.bg] : null };
      }
    }
    return buf;
  }
}

/** SGR emitter state used by diff and fullRender.
 *  `fgIdx` / `bgIdx` track the palette index when the last-emitted color
 *  was indexed; `fgRgb` / `bgRgb` track truecolor. They're always in sync
 *  — an index-based emit leaves `fgIdx` set AND `fgRgb` populated with
 *  the flattened value, so re-checking equality against a later cell
 *  that happens to carry the same RGB but a different index (or no
 *  index at all) still fires a re-emit through the index check. */
interface EmitState {
  fgIdx: number | null;
  fgRgb: [number, number, number] | null;
  bgIdx: number | null;
  bgRgb: [number, number, number] | null;
}

function indexedFgSgr(idx: number): string {
  if (idx < 8) return `\x1b[${30 + idx}m`;
  if (idx < 16) return `\x1b[${90 + idx - 8}m`;
  return `\x1b[38;5;${idx}m`;
}

function indexedBgSgr(idx: number): string {
  if (idx < 8) return `\x1b[${40 + idx}m`;
  if (idx < 16) return `\x1b[${100 + idx - 8}m`;
  return `\x1b[48;5;${idx}m`;
}

/** Emit an SGR for the foreground of `cell`, if different from `state`.
 *  Mutates `state`. Index-first: palette cells round-trip as SGR 30-37 /
 *  90-97 / 38;5;N so the outer terminal's theme wins. Truecolor cells
 *  emit SGR 38;2. Default cells emit SGR 39 only when transitioning
 *  away from a non-default color. */
function emitFg(cell: Cell, state: EmitState): string {
  if (cell.fgIndex !== null) {
    if (state.fgIdx === cell.fgIndex) return "";
    state.fgIdx = cell.fgIndex;
    state.fgRgb = cell.fg;
    return indexedFgSgr(cell.fgIndex);
  }
  if (cell.fg !== null) {
    if (state.fgIdx === null && colorEq(state.fgRgb, cell.fg)) return "";
    state.fgIdx = null;
    state.fgRgb = cell.fg;
    return `\x1b[38;2;${cell.fg[0]};${cell.fg[1]};${cell.fg[2]}m`;
  }
  // Default
  if (state.fgIdx === null && state.fgRgb === null) return "";
  state.fgIdx = null;
  state.fgRgb = null;
  return "\x1b[39m";
}

function emitBg(cell: Cell, state: EmitState): string {
  if (cell.bgIndex !== null) {
    if (state.bgIdx === cell.bgIndex) return "";
    state.bgIdx = cell.bgIndex;
    state.bgRgb = cell.bg;
    return indexedBgSgr(cell.bgIndex);
  }
  if (cell.bg !== null) {
    if (state.bgIdx === null && colorEq(state.bgRgb, cell.bg)) return "";
    state.bgIdx = null;
    state.bgRgb = cell.bg;
    return `\x1b[48;2;${cell.bg[0]};${cell.bg[1]};${cell.bg[2]}m`;
  }
  if (state.bgIdx === null && state.bgRgb === null) return "";
  state.bgIdx = null;
  state.bgRgb = null;
  return "\x1b[49m";
}

function resetState(state: EmitState): void {
  state.fgIdx = null; state.fgRgb = null;
  state.bgIdx = null; state.bgRgb = null;
}

/** Diff two buffers and emit minimal ANSI to update from prev to next.
 *  Uses DEC synchronized output (mode 2026) to prevent tearing.
 *
 *  Wide-char handling:
 *  - `lastCol` tracks where the terminal cursor lands after each emit.
 *    For a width-2 glyph, that's `c + 2`, not `c + 1`. Without accounting
 *    for the width, cursor-adjacency checks mispredict after every wide
 *    char, and — combined with the placeholder-skip below — can leave
 *    the right half of a prev-frame wide char visible on screen.
 *  - When `next.cells[r][c]` is a placeholder (`char === ""`), we skip it
 *    because the wide char at `c-1` already claims that column. But if
 *    `prev.cells[r][c-1]` was NOT a wide char (i.e. it's a real narrow
 *    char, or `prev` had a wide char at `c-1` that's now been overwritten
 *    with a narrow at c-1's position via a diff earlier in this pass),
 *    then the terminal at column c may still hold prev-frame content
 *    (a fossil). We emit an explicit space to clear it before letting
 *    the enclosing wide char's downstream repaint the position.
 *
 *  Reference: #47 tui-sup bug — `📬` fossilizing to `📬📬2 99` when
 *  navigating a cards grid caused the glyph to shift columns between
 *  frames. */
export function diff(prev: CellBuffer, next: CellBuffer): string {
  let out = "\x1b[?2026h"; // Begin synchronized update
  let lastRow = -1;
  let lastCol = -1;
  const state: EmitState = { fgIdx: null, fgRgb: null, bgIdx: null, bgRgb: null };
  let lastBold = false;
  let lastDim = false;
  let lastItalic = false;
  let lastUnderline = false;
  let needsReset = true;

  const emit = (r: number, c: number, ch: string, styleCell: Cell): void => {
    if (r !== lastRow || c !== lastCol) {
      out += `\x1b[${r + 1};${c + 1}H`;
    }

    const needReset =
      (styleCell.bold !== lastBold && !styleCell.bold) ||
      (styleCell.dim !== lastDim && !styleCell.dim) ||
      (styleCell.italic !== lastItalic && !styleCell.italic) ||
      (styleCell.underline !== lastUnderline && !styleCell.underline) ||
      needsReset;

    if (needReset) {
      out += "\x1b[0m";
      resetState(state);
      lastBold = false;
      lastDim = false;
      lastItalic = false;
      lastUnderline = false;
      needsReset = false;
    }

    if (styleCell.bold && !lastBold) out += "\x1b[1m";
    if (styleCell.dim && !lastDim) out += "\x1b[2m";
    if (styleCell.italic && !lastItalic) out += "\x1b[3m";
    if (styleCell.underline && !lastUnderline) out += "\x1b[4m";

    out += emitFg(styleCell, state);
    out += emitBg(styleCell, state);

    out += ch;

    lastRow = r;
    lastCol = c + (ch === "" ? 0 : charWidth(ch));
    lastBold = styleCell.bold;
    lastDim = styleCell.dim;
    lastItalic = styleCell.italic;
    lastUnderline = styleCell.underline;
  };

  for (let r = 0; r < next.rows; r++) {
    for (let c = 0; c < next.cols; c++) {
      const nc = next.cells[r][c];

      // Placeholder cells (char === "") are the right half of a wide
      // char at c-1 in `next`. Skip — the wide char's own emit at c-1
      // (either just performed this iteration, or elided as a cellsEqual
      // no-op) already covers column c on the terminal. writeAnsi's
      // placeholder invariant guarantees `next.cells[r][c].char === ""`
      // iff `next.cells[r][c-1]` is a wide char.
      if (nc.char === "") continue;

      const pc = prev.cells[r]?.[c];

      if (pc && cellsEqual(pc, nc)) continue;

      emit(r, c, nc.char, nc);
    }
  }

  out += "\x1b[0m"; // Reset at end
  out += "\x1b[?2026l"; // End synchronized update
  return out;
}

/** Render the full buffer to ANSI (for initial draw). */
export function fullRender(buf: CellBuffer): string {
  let out = "\x1b[?2026h\x1b[H\x1b[0m";
  const state: EmitState = { fgIdx: null, fgRgb: null, bgIdx: null, bgRgb: null };
  let lastBold = false;
  let lastDim = false;
  let lastItalic = false;
  let lastUnderline = false;

  for (let r = 0; r < buf.rows; r++) {
    if (r > 0) out += `\x1b[${r + 1};1H`;
    for (let c = 0; c < buf.cols; c++) {
      const cell = buf.cells[r][c];

      // Skip wide-char placeholder cells (second cell of a 2-cell character)
      if (cell.char === "") continue;

      const needReset =
        (cell.bold !== lastBold && !cell.bold) ||
        (cell.dim !== lastDim && !cell.dim) ||
        (cell.italic !== lastItalic && !cell.italic) ||
        (cell.underline !== lastUnderline && !cell.underline);

      if (needReset) {
        out += "\x1b[0m";
        resetState(state);
        lastBold = false;
        lastDim = false;
        lastItalic = false;
        lastUnderline = false;
      }

      if (cell.bold && !lastBold) out += "\x1b[1m";
      if (cell.dim && !lastDim) out += "\x1b[2m";
      if (cell.italic && !lastItalic) out += "\x1b[3m";
      if (cell.underline && !lastUnderline) out += "\x1b[4m";

      out += emitFg(cell, state);
      out += emitBg(cell, state);

      out += cell.char;
      lastBold = cell.bold;
      lastDim = cell.dim;
      lastItalic = cell.italic;
      lastUnderline = cell.underline;
    }
  }

  out += "\x1b[0m\x1b[?2026l";
  return out;
}

function colorEq(a: [number, number, number] | null, b: [number, number, number] | null): boolean {
  if (a === b) return true;
  if (!a || !b) return false;
  return a[0] === b[0] && a[1] === b[1] && a[2] === b[2];
}

// --- Color conversion helpers ---
const COLORS_16: [number, number, number][] = [
  [0, 0, 0], [204, 0, 0], [0, 204, 0], [204, 204, 0],
  [0, 0, 204], [204, 0, 204], [0, 204, 204], [204, 204, 204],
  [85, 85, 85], [255, 85, 85], [85, 255, 85], [255, 255, 85],
  [85, 85, 255], [255, 85, 255], [85, 255, 255], [255, 255, 255],
];

function ansi16ToRgb(n: number): [number, number, number] {
  return COLORS_16[n] ?? [255, 255, 255];
}

function ansi256ToRgb(n: number): [number, number, number] {
  if (n < 16) return ansi16ToRgb(n);
  if (n >= 232) {
    const v = (n - 232) * 10 + 8;
    return [v, v, v];
  }
  const idx = n - 16;
  const r = Math.floor(idx / 36) * 51;
  const g = Math.floor((idx % 36) / 6) * 51;
  const b = (idx % 6) * 51;
  return [r, g, b];
}
