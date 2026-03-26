import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { KeyEvent } from "./input.ts";
import {
  moveTo,
  drawBox,
  bold,
  dim,
  inverse,
  truncate,
  pad,
} from "./render.ts";

export interface CreateAction {
  type: "create" | "cancel" | "none";
  dir?: string;
  name?: string;
  command?: string;
}

export type CreateStep = "dir-initial" | "dir-browse" | "name-command";

export interface CreateState {
  step: CreateStep;
  // Dir picker
  selectedIndex: number;
  cwdPath: string;
  browsePath: string;
  browseFilter: string;
  // Name + command
  name: string;
  command: string;
  focusedField: "name" | "command";
  // Layout
  termWidth: number;
  termHeight: number;
  // For dedup
  existingNames: Set<string>;
}

export function createCreateState(
  termWidth: number,
  termHeight: number,
  existingNames: string[]
): CreateState {
  const cwdPath = process.cwd();
  return {
    step: "dir-initial",
    selectedIndex: 0,
    cwdPath,
    browsePath: cwdPath,
    browseFilter: "",
    name: path.basename(cwdPath),
    command: "",
    focusedField: "command",
    termWidth,
    termHeight,
    existingNames: new Set(existingNames),
  };
}

function shortPath(p: string): string {
  const home = os.homedir();
  if (p === home) return "~";
  if (p.startsWith(home + "/")) return "~" + p.slice(home.length);
  return p;
}

export function dedupName(base: string, existing: Set<string>): string {
  if (!existing.has(base)) return base;
  for (let i = 2; ; i++) {
    const candidate = `${base}-${i}`;
    if (!existing.has(candidate)) return candidate;
  }
}

export function listDirs(dirPath: string, filter: string): string[] {
  let entries: fs.Dirent[];
  try {
    entries = fs.readdirSync(dirPath, { withFileTypes: true });
  } catch {
    return [];
  }
  const dirs = entries
    .filter((e) => {
      if (e.name.startsWith(".")) return false;
      if (e.isDirectory()) return true;
      // Follow symlinks — include if target is a directory
      if (e.isSymbolicLink()) {
        try {
          const target = fs.statSync(path.join(dirPath, e.name));
          return target.isDirectory();
        } catch {
          return false; // broken symlink
        }
      }
      return false;
    })
    .map((e) => e.name)
    .sort();
  if (!filter) return dirs;
  const lf = filter.toLowerCase();
  return dirs.filter((d) => d.toLowerCase().includes(lf));
}

export function handleCreateKey(state: CreateState, key: KeyEvent): CreateAction {
  switch (state.step) {
    case "dir-initial":
      return handleDirInitial(state, key);
    case "dir-browse":
      return handleDirBrowse(state, key);
    case "name-command":
      return handleNameCommand(state, key);
  }
}

function handleDirInitial(state: CreateState, key: KeyEvent): CreateAction {
  // Two items: 0 = cwd, 1 = "Choose disk location..."
  if (key.name === "up") {
    state.selectedIndex = 0;
    return { type: "none" };
  }
  if (key.name === "down") {
    state.selectedIndex = 1;
    return { type: "none" };
  }
  if (key.name === "return") {
    if (state.selectedIndex === 0) {
      // Use cwd
      state.step = "name-command";
      state.name = dedupName(path.basename(state.cwdPath), state.existingNames);
      return { type: "none" };
    } else {
      // Browse
      state.step = "dir-browse";
      state.browsePath = state.cwdPath;
      state.selectedIndex = 0;
      state.browseFilter = "";
      return { type: "none" };
    }
  }
  if (key.name === "escape" || (key.name === "c" && key.ctrl)) {
    return { type: "cancel" };
  }
  return { type: "none" };
}

function handleDirBrowse(state: CreateState, key: KeyEvent): CreateAction {
  const dirs = listDirs(state.browsePath, state.browseFilter);
  // Items: [Select this directory], .., ...dirs
  const totalItems = 2 + dirs.length;

  if (key.name === "up") {
    state.selectedIndex = Math.max(0, state.selectedIndex - 1);
    return { type: "none" };
  }
  if (key.name === "down") {
    state.selectedIndex = Math.min(totalItems - 1, state.selectedIndex + 1);
    return { type: "none" };
  }
  if (key.name === "return") {
    if (state.selectedIndex === 0) {
      // Select this directory
      state.step = "name-command";
      state.name = dedupName(path.basename(state.browsePath), state.existingNames);
      return { type: "none" };
    }
    if (state.selectedIndex === 1) {
      // ..
      const parent = path.dirname(state.browsePath);
      if (parent !== state.browsePath) {
        state.browsePath = parent;
        state.selectedIndex = 0;
        state.browseFilter = "";
      }
      return { type: "none" };
    }
    // Navigate into a directory
    const dirName = dirs[state.selectedIndex - 2];
    if (dirName) {
      state.browsePath = path.join(state.browsePath, dirName);
      state.selectedIndex = 0;
      state.browseFilter = "";
    }
    return { type: "none" };
  }
  if (key.name === "escape") {
    if (state.browseFilter) {
      state.browseFilter = "";
      state.selectedIndex = 0;
      return { type: "none" };
    }
    state.step = "dir-initial";
    state.selectedIndex = 0;
    return { type: "none" };
  }
  if (key.name === "c" && key.ctrl) {
    return { type: "cancel" };
  }
  if (key.name === "backspace") {
    if (state.browseFilter.length > 0) {
      state.browseFilter = state.browseFilter.slice(0, -1);
      state.selectedIndex = Math.min(state.selectedIndex, 1 + listDirs(state.browsePath, state.browseFilter).length);
    }
    return { type: "none" };
  }
  // Typing — filter directories
  if (key.char && !key.ctrl && !key.alt) {
    state.browseFilter += key.char;
    state.selectedIndex = 2; // Jump to first directory match
    return { type: "none" };
  }
  return { type: "none" };
}

function handleNameCommand(state: CreateState, key: KeyEvent): CreateAction {
  if (key.name === "tab") {
    state.focusedField = state.focusedField === "name" ? "command" : "name";
    return { type: "none" };
  }
  if (key.name === "return") {
    if (state.name.trim() && state.command.trim()) {
      const dir = state.step === "name-command"
        ? (state.browsePath !== state.cwdPath ? state.browsePath : state.cwdPath)
        : state.cwdPath;
      return {
        type: "create",
        dir,
        name: state.name.trim(),
        command: state.command.trim(),
      };
    }
    return { type: "none" };
  }
  if (key.name === "escape") {
    state.step = "dir-initial";
    state.selectedIndex = 0;
    return { type: "none" };
  }
  if (key.name === "c" && key.ctrl) {
    return { type: "cancel" };
  }

  // Text editing for focused field
  const field = state.focusedField;
  if (key.name === "backspace") {
    if (field === "name") {
      state.name = state.name.slice(0, -1);
    } else {
      state.command = state.command.slice(0, -1);
    }
    return { type: "none" };
  }
  if (key.char && !key.ctrl && !key.alt) {
    if (field === "name") {
      state.name += key.char;
    } else {
      state.command += key.char;
    }
    return { type: "none" };
  }
  return { type: "none" };
}

export function renderCreate(state: CreateState): string {
  switch (state.step) {
    case "dir-initial":
      return renderDirInitial(state);
    case "dir-browse":
      return renderDirBrowse(state);
    case "name-command":
      return renderNameCommand(state);
  }
}

function renderDirInitial(state: CreateState): string {
  const { termWidth, termHeight } = state;
  const boxWidth = Math.min(Math.max(40, termWidth - 4), termWidth - 2);
  const boxCol = Math.max(1, Math.floor((termWidth - boxWidth) / 2) + 1);
  const boxHeight = 8;
  const boxRow = Math.max(1, Math.floor((termHeight - boxHeight - 1) / 2) + 1);
  const contentWidth = boxWidth - 4;

  let out = drawBox(boxRow, boxCol, boxWidth, boxHeight, bold("New Session \u2014 Choose Directory"));

  let row = boxRow + 2;
  // Build items as plain text first, then style
  const cwdText = shortPath(state.cwdPath);
  const plainItems = [
    `  ${cwdText}  (current directory)`,
    `  Choose disk location\u2026`,
  ];
  const styledItems = [
    `  ${cwdText}  ${dim("(current directory)")}`,
    `  Choose disk location\u2026`,
  ];
  for (let i = 0; i < plainItems.length; i++) {
    const plain = pad(truncate(plainItems[i], contentWidth), contentWidth);
    // Rebuild styled version with correct padding
    const styled = styledItems[i];
    const padAmount = contentWidth - plainItems[i].length;
    const paddedStyled = padAmount > 0 ? styled + " ".repeat(padAmount) : styled;
    out += moveTo(row, boxCol + 2) + (i === state.selectedIndex ? inverse(paddedStyled) : paddedStyled);
    row++;
  }

  const footerRow = boxRow + boxHeight;
  out += moveTo(footerRow, boxCol + 1) + dim("\u2191\u2193 select  \u23ce confirm  \u238b back");

  return out;
}

function renderDirBrowse(state: CreateState): string {
  const { termWidth, termHeight } = state;
  const boxWidth = Math.min(Math.max(40, termWidth - 4), termWidth - 2);
  const boxCol = Math.max(1, Math.floor((termWidth - boxWidth) / 2) + 1);
  const contentWidth = boxWidth - 4;

  const dirs = listDirs(state.browsePath, state.browseFilter);
  const maxItems = Math.min(dirs.length + 2, termHeight - 6);
  const boxHeight = Math.max(8, maxItems + 5);
  const boxRow = Math.max(1, Math.floor((termHeight - boxHeight - 1) / 2) + 1);

  let out = drawBox(boxRow, boxCol, boxWidth, boxHeight, bold("Browse \u2014 " + shortPath(state.browsePath)));

  let row = boxRow + 1;

  // Filter line
  if (state.browseFilter) {
    row++;
    out += moveTo(row, boxCol + 2) + `Filter: ${state.browseFilter}`;
  }

  row++;

  // Fixed items — build plain first, style after
  const plainFixed = [
    "  [Select this directory]",
    "  ..",
  ];
  const styledFixed = [
    `  ${dim("[Select this directory]")}`,
    `  ${dim("..")}`,
  ];
  for (let i = 0; i < plainFixed.length; i++) {
    row++;
    const padAmount = Math.max(0, contentWidth - plainFixed[i].length);
    const line = styledFixed[i] + " ".repeat(padAmount);
    out += moveTo(row, boxCol + 2) + (i === state.selectedIndex ? inverse(line) : line);
  }

  // Directory entries
  const visibleDirs = dirs.slice(0, boxHeight - 7);
  for (let i = 0; i < visibleDirs.length; i++) {
    row++;
    const actualIndex = i + 2;
    const line = pad(`  ${visibleDirs[i]}/`, contentWidth);
    out += moveTo(row, boxCol + 2) + (actualIndex === state.selectedIndex ? inverse(line) : line);
  }

  const footerRow = boxRow + boxHeight;
  out += moveTo(footerRow, boxCol + 1) + dim("\u2191\u2193 select  \u23ce enter  \u238b back  type to filter");

  return out;
}

function renderNameCommand(state: CreateState): string {
  const { termWidth, termHeight } = state;
  const boxWidth = Math.min(Math.max(40, termWidth - 4), termWidth - 2);
  const boxCol = Math.max(1, Math.floor((termWidth - boxWidth) / 2) + 1);
  const boxHeight = 10;
  const boxRow = Math.max(1, Math.floor((termHeight - boxHeight - 1) / 2) + 1);

  const dir = state.browsePath !== state.cwdPath ? state.browsePath : state.cwdPath;

  let out = drawBox(boxRow, boxCol, boxWidth, boxHeight, bold("New Session"));

  let row = boxRow + 2;
  out += moveTo(row, boxCol + 2) + `Directory: ${dim(shortPath(dir))}`;

  row += 2;
  const nameLabel = "Name:    ";
  const nameValue = state.name + (state.focusedField === "name" ? "\u2588" : "");
  out += moveTo(row, boxCol + 2) + (state.focusedField === "name" ? bold(nameLabel) : nameLabel) + nameValue;

  row++;
  const cmdLabel = "Command: ";
  const cmdValue = state.command + (state.focusedField === "command" ? "\u2588" : "");
  out += moveTo(row, boxCol + 2) + (state.focusedField === "command" ? bold(cmdLabel) : cmdLabel) + cmdValue;

  const footerRow = boxRow + boxHeight;
  out += moveTo(footerRow, boxCol + 1) + dim("\u21e5 switch field  \u23ce create  \u238b back");

  return out;
}
