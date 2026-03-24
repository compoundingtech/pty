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
                isBold = false; isDim = false; isItalic = false; isUnderline = false;
              } else if (p === 1) isBold = true;
              else if (p === 2) isDim = true;
              else if (p === 3) isItalic = true;
              else if (p === 4) isUnderline = true;
              else if (p === 22) { isBold = false; isDim = false; }
              else if (p === 23) isItalic = false;
              else if (p === 24) isUnderline = false;
              else if (p === 27) { /* reset inverse - ignore */ }
              else if (p === 39) fgColor = null;
              else if (p === 49) bgColor = null;
              else if (p === 38 && parts[j + 1] === 2) {
                fgColor = [parts[j + 2] ?? 0, parts[j + 3] ?? 0, parts[j + 4] ?? 0];
                j += 4;
              } else if (p === 48 && parts[j + 1] === 2) {
                bgColor = [parts[j + 2] ?? 0, parts[j + 3] ?? 0, parts[j + 4] ?? 0];
                j += 4;
              } else if (p === 38 && parts[j + 1] === 5) {
                fgColor = ansi256ToRgb(parts[j + 2] ?? 0);
                j += 2;
              } else if (p === 48 && parts[j + 1] === 5) {
                bgColor = ansi256ToRgb(parts[j + 2] ?? 0);
                j += 2;
              } else if (p >= 30 && p <= 37) fgColor = ansi16ToRgb(p - 30);
              else if (p >= 40 && p <= 47) bgColor = ansi16ToRgb(p - 40);
              else if (p >= 90 && p <= 97) fgColor = ansi16ToRgb(p - 90 + 8);
              else if (p >= 100 && p <= 107) bgColor = ansi16ToRgb(p - 100 + 8);
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
        // Printable character — may be wide (2 cells)
        const ch = ansi[i];
        const cw = charWidth(ch);
        if (curRow >= 0 && curRow < this.rows && curCol >= 0 && curCol < this.cols) {
          this.cells[curRow][curCol] = {
            char: ch,
            fg: fgColor ? [...fgColor] : null,
            bg: bgColor ? [...bgColor] : null,
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
              bold: isBold,
              dim: isDim,
              italic: isItalic,
              underline: isUnderline,
            };
          }
        }
        curCol += cw;
        i++;
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

/** Diff two buffers and emit minimal ANSI to update from prev to next.
 *  Uses DEC synchronized output (mode 2026) to prevent tearing. */
export function diff(prev: CellBuffer, next: CellBuffer): string {
  let out = "\x1b[?2026h"; // Begin synchronized update
  let lastRow = -1;
  let lastCol = -1;
  let lastFg: [number, number, number] | null = null;
  let lastBg: [number, number, number] | null = null;
  let lastBold = false;
  let lastDim = false;
  let lastItalic = false;
  let lastUnderline = false;
  let needsReset = true;

  for (let r = 0; r < next.rows; r++) {
    for (let c = 0; c < next.cols; c++) {
      const nc = next.cells[r][c];

      // Skip wide-char placeholder cells
      if (nc.char === "") continue;

      const pc = prev.cells[r]?.[c];

      if (pc && cellsEqual(pc, nc)) continue;

      // Move cursor if not adjacent
      if (r !== lastRow || c !== lastCol) {
        out += `\x1b[${r + 1};${c + 1}H`;
      }

      // Apply style changes
      const needReset =
        (nc.bold !== lastBold && !nc.bold) ||
        (nc.dim !== lastDim && !nc.dim) ||
        (nc.italic !== lastItalic && !nc.italic) ||
        (nc.underline !== lastUnderline && !nc.underline) ||
        needsReset;

      if (needReset) {
        out += "\x1b[0m";
        lastFg = null;
        lastBg = null;
        lastBold = false;
        lastDim = false;
        lastItalic = false;
        lastUnderline = false;
        needsReset = false;
      }

      if (nc.bold && !lastBold) out += "\x1b[1m";
      if (nc.dim && !lastDim) out += "\x1b[2m";
      if (nc.italic && !lastItalic) out += "\x1b[3m";
      if (nc.underline && !lastUnderline) out += "\x1b[4m";

      if (nc.fg && !colorEq(nc.fg, lastFg)) {
        out += `\x1b[38;2;${nc.fg[0]};${nc.fg[1]};${nc.fg[2]}m`;
      } else if (!nc.fg && lastFg) {
        out += "\x1b[39m";
      }

      if (nc.bg && !colorEq(nc.bg, lastBg)) {
        out += `\x1b[48;2;${nc.bg[0]};${nc.bg[1]};${nc.bg[2]}m`;
      } else if (!nc.bg && lastBg) {
        out += "\x1b[49m";
      }

      out += nc.char;

      lastRow = r;
      lastCol = c + 1;
      lastFg = nc.fg;
      lastBg = nc.bg;
      lastBold = nc.bold;
      lastDim = nc.dim;
      lastItalic = nc.italic;
      lastUnderline = nc.underline;
    }
  }

  out += "\x1b[0m"; // Reset at end
  out += "\x1b[?2026l"; // End synchronized update
  return out;
}

/** Render the full buffer to ANSI (for initial draw). */
export function fullRender(buf: CellBuffer): string {
  let out = "\x1b[?2026h\x1b[H\x1b[0m";
  let lastFg: [number, number, number] | null = null;
  let lastBg: [number, number, number] | null = null;
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
        lastFg = null;
        lastBg = null;
        lastBold = false;
        lastDim = false;
        lastItalic = false;
        lastUnderline = false;
      }

      if (cell.bold && !lastBold) out += "\x1b[1m";
      if (cell.dim && !lastDim) out += "\x1b[2m";
      if (cell.italic && !lastItalic) out += "\x1b[3m";
      if (cell.underline && !lastUnderline) out += "\x1b[4m";

      if (cell.fg && !colorEq(cell.fg, lastFg)) {
        out += `\x1b[38;2;${cell.fg[0]};${cell.fg[1]};${cell.fg[2]}m`;
      } else if (!cell.fg && lastFg) {
        out += "\x1b[39m";
      }

      if (cell.bg && !colorEq(cell.bg, lastBg)) {
        out += `\x1b[48;2;${cell.bg[0]};${cell.bg[1]};${cell.bg[2]}m`;
      } else if (!cell.bg && lastBg) {
        out += "\x1b[49m";
      }

      out += cell.char;
      lastFg = cell.fg;
      lastBg = cell.bg;
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
