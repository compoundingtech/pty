import * as os from "node:os";
import type { SessionInfo } from "../sessions.ts";
import type { KeyEvent } from "./input.ts";
import {
  moveTo,
  drawBox,
  bold,
  dim,
  green,
  red,
  inverse,
  truncate,
  pad,
} from "./render.ts";

export interface ListAction {
  type: "attach" | "create" | "quit" | "none";
  session?: SessionInfo;
}

export interface ListState {
  sessions: SessionInfo[];
  filterText: string;
  selectedIndex: number;
  termWidth: number;
  termHeight: number;
}

export function createListState(
  sessions: SessionInfo[],
  termWidth: number,
  termHeight: number
): ListState {
  return {
    sessions: sortSessions(sessions),
    filterText: "",
    selectedIndex: 0,
    termWidth,
    termHeight,
  };
}

export function updateSessions(state: ListState, sessions: SessionInfo[]): void {
  state.sessions = sortSessions(sessions);
  // Clamp selection
  const itemCount = filteredItems(state).length;
  if (state.selectedIndex >= itemCount) {
    state.selectedIndex = Math.max(0, itemCount - 1);
  }
}

export function sortSessions(sessions: SessionInfo[]): SessionInfo[] {
  return [...sessions].sort((a, b) => {
    const aRunning = a.status === "running" ? 0 : 1;
    const bRunning = b.status === "running" ? 0 : 1;
    if (aRunning !== bRunning) return aRunning - bRunning;
    return a.name.localeCompare(b.name);
  });
}

export function shortPath(p: string): string {
  const home = os.homedir();
  if (p === home) return "~";
  if (p.startsWith(home + "/")) return "~" + p.slice(home.length);
  return p;
}

export function timeAgo(date: Date): string {
  const seconds = Math.floor((Date.now() - date.getTime()) / 1000);
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

interface ListItem {
  type: "session" | "create";
  session?: SessionInfo;
  label: string;
}

function filteredItems(state: ListState): ListItem[] {
  const items: ListItem[] = [];
  const filter = state.filterText.toLowerCase();

  for (const s of state.sessions) {
    const cmd = s.metadata
      ? [s.metadata.displayCommand, ...s.metadata.args].join(" ")
      : "";
    const cwd = s.metadata?.cwd ?? "";
    const searchable = `${s.name} ${cwd} ${cmd}`.toLowerCase();
    if (filter && !searchable.includes(filter)) continue;
    items.push({ type: "session", session: s, label: s.name });
  }

  // Always show "Create new session..."
  items.push({ type: "create", label: "Create new session..." });

  return items;
}

export function handleListKey(state: ListState, key: KeyEvent): ListAction {
  const items = filteredItems(state);
  const maxIndex = items.length - 1;

  // Navigation
  if (key.name === "up") {
    state.selectedIndex = Math.max(0, state.selectedIndex - 1);
    return { type: "none" };
  }
  if (key.name === "down") {
    state.selectedIndex = Math.min(maxIndex, state.selectedIndex + 1);
    return { type: "none" };
  }

  // Select
  if (key.name === "return") {
    const item = items[state.selectedIndex];
    if (!item) return { type: "none" };
    if (item.type === "create") return { type: "create" };
    return { type: "attach", session: item.session };
  }

  // Quit
  if (key.name === "q" && !key.ctrl && !key.alt && !state.filterText) {
    return { type: "quit" };
  }
  if (key.name === "c" && key.ctrl) {
    return { type: "quit" };
  }

  // Escape: clear filter or quit
  if (key.name === "escape") {
    if (state.filterText) {
      state.filterText = "";
      state.selectedIndex = 0;
      return { type: "none" };
    }
    return { type: "quit" };
  }

  // Backspace
  if (key.name === "backspace") {
    if (state.filterText.length > 0) {
      state.filterText = state.filterText.slice(0, -1);
      state.selectedIndex = 0;
    }
    return { type: "none" };
  }

  // Typing — filter
  if (key.char && !key.ctrl && !key.alt) {
    state.filterText += key.char;
    state.selectedIndex = 0;
    return { type: "none" };
  }

  return { type: "none" };
}

export function renderList(state: ListState): string {
  const { termWidth, termHeight } = state;
  // Use full terminal width with 1-column margin each side
  const boxWidth = Math.max(40, termWidth - 2);
  const boxCol = Math.max(1, Math.floor((termWidth - boxWidth) / 2) + 1);
  const items = filteredItems(state);
  // contentWidth = space between the box's left and right borders, minus 1 padding each side
  const contentWidth = boxWidth - 4;

  // Box height: filter line + blank + items + blank + borders
  const minBoxHeight = Math.min(items.length + 5, termHeight - 2);
  const boxHeight = Math.max(7, minBoxHeight);
  const boxRow = Math.max(1, Math.floor((termHeight - boxHeight - 1) / 2) + 1);

  let out = "";

  // Draw box
  out += drawBox(boxRow, boxCol, boxWidth, boxHeight, bold("pty"));

  let row = boxRow + 1;

  // Filter line
  row++;
  const filterLabel = "Filter: ";
  const filterDisplay = state.filterText || dim("(type to filter)");
  out += moveTo(row, boxCol + 2) + filterLabel + filterDisplay;

  row++;

  // Compute dynamic column widths for session rows
  // Layout: "  ● name  path  command"
  // Fixed: 2 (indent) + 2 (icon + space) + 2 (gap after name) + 2 (gap after path) = 8
  const fixedChars = 8;
  const flexWidth = contentWidth - fixedChars;

  // Compute name column width from actual data
  const sessionItems = items.filter((it) => it.type === "session");
  const maxNameLen = sessionItems.reduce(
    (max, it) => Math.max(max, it.session!.name.length),
    0
  );
  const nameWidth = Math.min(Math.max(maxNameLen, 8), Math.floor(flexWidth * 0.25));

  // Remaining space split between path and command
  const remaining = flexWidth - nameWidth;
  const pathWidth = Math.max(4, Math.floor(remaining * 0.55));
  const cmdWidth = Math.max(4, remaining - pathWidth);

  // Items
  const maxVisibleItems = boxHeight - 5; // borders + filter + padding
  let startIdx = 0;
  if (state.selectedIndex >= startIdx + maxVisibleItems) {
    startIdx = state.selectedIndex - maxVisibleItems + 1;
  }
  const visibleItems = items.slice(startIdx, startIdx + maxVisibleItems);

  for (let i = 0; i < visibleItems.length; i++) {
    row++;
    const item = visibleItems[i];
    const actualIndex = startIdx + i;
    const selected = actualIndex === state.selectedIndex;

    let line: string;
    if (item.type === "create") {
      // Plain text, then pad to contentWidth
      line = pad("  + " + item.label, contentWidth);
    } else {
      const s = item.session!;
      const icon = s.status === "running" ? "\u25cf" : "\u25cb";
      const cmd = s.metadata
        ? [s.metadata.displayCommand, ...s.metadata.args].join(" ")
        : "";

      // Build plain-text columns, truncate/pad each
      const nameCol = pad(truncate(s.name, nameWidth), nameWidth);
      let pathCol: string;
      let cmdCol: string;

      if (s.status === "running") {
        const cwd = s.metadata?.cwd ? shortPath(s.metadata.cwd) : "";
        pathCol = pad(truncate(cwd, pathWidth), pathWidth);
        cmdCol = truncate(cmd, cmdWidth);
      } else {
        const cwd = s.metadata?.cwd ? shortPath(s.metadata.cwd) : "";
        const ago = s.metadata?.exitedAt ? `(exited ${timeAgo(new Date(s.metadata.exitedAt))})` : "(exited)";
        const exitPath = cwd ? `${cwd}  ${ago}` : ago;
        pathCol = pad(truncate(exitPath, pathWidth), pathWidth);
        cmdCol = truncate(cmd, cmdWidth);
      }

      // Assemble plain-text line first, then apply styles
      // "  ● name  path  command" — all plain text sizes are correct
      const plainLine = `  ${icon} ${nameCol}  ${pathCol}  ${cmdCol}`;
      // Pad the full line to contentWidth using plain text length
      line = pad(plainLine, contentWidth);

      // Now apply ANSI styles to the assembled plain-text line
      // Re-build with styles — column positions are fixed, so we can reconstruct
      const styledIcon = s.status === "running" ? green(icon) : red(icon);
      const styledPath = s.status === "running" ? dim(pathCol) : dim(pathCol);
      line = `  ${styledIcon} ${nameCol}  ${styledPath}  ${cmdCol}`;
      // Pad with spaces to contentWidth (line's visible length = plainLine.length)
      const visLen = plainLine.length;
      if (visLen < contentWidth) {
        line += " ".repeat(contentWidth - visLen);
      }
    }

    out += moveTo(row, boxCol + 2) + (selected ? inverse(line) : line);
  }

  // Footer (below box)
  const footerRow = boxRow + boxHeight;
  const bindings = [
    "\u2191\u2193 select",
    "\u23ce attach",
    "q quit",
  ];
  out += moveTo(footerRow, boxCol + 1) + dim(bindings.join("  "));

  return out;
}
