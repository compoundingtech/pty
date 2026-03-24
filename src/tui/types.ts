// Core types for the TUI framework

import type { Theme, BoxStyle } from "./colors.ts";
import type { KeyEvent } from "./input.ts";

// --- Cell buffer types ---
export interface Cell {
  char: string;
  fg: [number, number, number] | null;
  bg: [number, number, number] | null;
  bold: boolean;
  dim: boolean;
  italic: boolean;
  underline: boolean;
}

export function emptyCell(): Cell {
  return { char: " ", fg: null, bg: null, bold: false, dim: false, italic: false, underline: false };
}

export function cellsEqual(a: Cell, b: Cell): boolean {
  return (
    a.char === b.char &&
    a.bold === b.bold &&
    a.dim === b.dim &&
    a.italic === b.italic &&
    a.underline === b.underline &&
    colorEqual(a.fg, b.fg) &&
    colorEqual(a.bg, b.bg)
  );
}

function colorEqual(a: [number, number, number] | null, b: [number, number, number] | null): boolean {
  if (a === b) return true;
  if (!a || !b) return false;
  return a[0] === b[0] && a[1] === b[1] && a[2] === b[2];
}

// --- Screen context (minimal base) ---
// Apps can extend this interface with additional methods/properties.
export interface ScreenContext {
  rows: number;
  cols: number;
  theme: Theme;
  boxStyle: BoxStyle;
  navigate: (screenId: string) => void;
  back: () => void;
  openOverlay: (screenId: string) => void;
  closeOverlay: () => void;
  isTextInputActive: () => boolean;
  setTextInputActive: (active: boolean) => void;
  [key: string]: any;
}

// --- Screen interface ---
export interface Screen {
  id: string;
  render(ctx: ScreenContext): string;
  renderToBuffer(ctx: ScreenContext): import("./buffer.ts").CellBuffer;
  handleKey(key: KeyEvent, ctx: ScreenContext): boolean;
  onEnter?(ctx: ScreenContext): void;
  onLeave?(ctx: ScreenContext): void;
}
