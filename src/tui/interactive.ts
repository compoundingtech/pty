// Interactive session list — built with the declarative TUI framework + app()
import * as path from "node:path";
import { attach } from "../client.ts";
import {
  listSessions, validateName, acquireLock, releaseLock,
  cleanupAll, getSession, type SessionInfo,
} from "../sessions.ts";
import { spawnDaemon, resolveCommand } from "../spawn.ts";
import {
  app, screen, signal, computed, batch,
  text, row, spacer, panel, selectable, footer, canvas,
  updateScrollRegion, type KeyEvent, type ScreenContext, type UINode,
} from "./index.ts";
// Reuse utility functions from the existing screen modules
import { sortSessions, shortPath, timeAgo } from "./screen-list.ts";
import { dedupName, listDirs } from "./screen-create.ts";

// ============================================================
// State (signals)
// ============================================================

const sessions = signal<SessionInfo[]>([]);
const filterText = signal("");
const selectedIndex = signal(0);
const currentScreen = signal<"list" | "create">("list");

// Create wizard state
const createStep = signal<"dir-initial" | "dir-browse" | "name-command">("dir-initial");
const cwdPath = signal(process.cwd());
const browsePath = signal(process.cwd());
const browseFilter = signal("");
const createSelectedIndex = signal(0);
const sessionName = signal("");
const sessionCommand = signal("");
const focusedField = signal<"name" | "command">("command");
const existingNames = signal<Set<string>>(new Set());

// ============================================================
// Computed values
// ============================================================

interface ListItem {
  type: "session" | "create";
  session?: SessionInfo;
}

const sortedSessions = computed<SessionInfo[]>(() => sortSessions(sessions.get()));

const filteredItems = computed<ListItem[]>(() => {
  const filter = filterText.get().toLowerCase();
  const matches: { item: ListItem; rank: number }[] = [];
  for (const s of sortedSessions.get()) {
    if (!filter) {
      matches.push({ item: { type: "session", session: s }, rank: 0 });
      continue;
    }
    const cmd = s.metadata
      ? [s.metadata.displayCommand, ...s.metadata.args].join(" ")
      : "";
    const cwd = s.metadata?.cwd ?? "";
    const nameMatch = s.name.toLowerCase().includes(filter);
    const cwdMatch = cwd.toLowerCase().includes(filter);
    const cmdMatch = cmd.toLowerCase().includes(filter);
    if (!nameMatch && !cwdMatch && !cmdMatch) continue;
    // Name matches rank highest (0), then cwd (1), then command (2)
    const rank = nameMatch ? 0 : cwdMatch ? 1 : 2;
    matches.push({ item: { type: "session", session: s }, rank });
  }
  // Stable sort: within same rank, preserve the existing order (running first, alpha)
  matches.sort((a, b) => a.rank - b.rank);
  const items = matches.map(m => m.item);
  items.push({ type: "create" });
  return items;
});

// ============================================================
// List screen
// ============================================================

function renderListItem(item: ListItem, _index: number, selected: boolean): UINode[] {
  if (item.type === "create") {
    const label = selected ? "  + Create new session..." : "  + Create new session...";
    return [text(label, selected ? "accent" : "muted", { bold: selected, truncate: true })];
  }
  const s = item.session!;
  const icon = s.status === "running" ? "\u25cf" : "\u25cb";
  const iconColor: "ok" | "error" = s.status === "running" ? "ok" : "error";
  const cmd = s.metadata
    ? [s.metadata.displayCommand, ...s.metadata.args].join(" ")
    : "";
  const pathStr = s.status === "running"
    ? (s.metadata?.cwd ? shortPath(s.metadata.cwd) : "")
    : (s.metadata?.exitedAt ? `(exited ${timeAgo(new Date(s.metadata.exitedAt))})` : "");

  // Single text node per row — keeps column layout simple and avoids
  // the framework's flex distribution splitting the line oddly.
  const line = `  ${icon} ${s.name}  ${pathStr}  ${cmd}`;
  return [text(line, selected ? "accent" : "primary", { bold: selected, truncate: true })];
}

const listScreen = screen({
  id: "list",

  render(ctx: ScreenContext): UINode[] {
    const items = filteredItems.get();
    const viewport = Math.max(1, ctx.rows - 6);
    const region = updateScrollRegion(
      { offset: 0, selectedIndex: selectedIndex.get(), totalItems: items.length, viewportHeight: viewport },
      items.length,
      viewport,
    );

    const filter = filterText.get();
    const filterLine = filter
      ? text("  Filter: " + filter, "primary")
      : text("  Filter: (type to filter)", "muted", { dim: true });

    return [
      panel("pty", [
        filterLine,
        text("", "muted"), // blank line
        selectable(region, items, renderListItem),
      ]),
      footer("\u2191\u2193 select  \u23ce attach  q quit"),
    ];
  },

  handleKey(key: KeyEvent, _ctx: ScreenContext): boolean {
    const items = filteredItems.get();
    const idx = selectedIndex.peek();
    const maxIndex = items.length - 1;

    if (key.name === "up") {
      selectedIndex.set(Math.max(0, idx - 1));
      return true;
    }
    if (key.name === "down") {
      selectedIndex.set(Math.min(maxIndex, idx + 1));
      return true;
    }
    if (key.name === "return") {
      const item = items[idx];
      if (!item) return true;
      if (item.type === "create") {
        batch(() => {
          currentScreen.set("create");
          createStep.set("dir-initial");
          createSelectedIndex.set(0);
          existingNames.set(new Set(sessions.peek().map(s => s.name)));
        });
        return true;
      }
      if (item.session) {
        doAttach(item.session.name);
        return true;
      }
    }
    if (key.name === "escape") {
      if (filterText.peek()) {
        batch(() => { filterText.set(""); selectedIndex.set(0); });
        return true;
      }
      return false; // quit
    }
    if (key.char === "q" && !key.ctrl && !key.alt && !filterText.peek()) {
      return false; // quit
    }
    if (key.name === "c" && key.ctrl) {
      return false; // quit
    }
    if (key.name === "backspace") {
      if (filterText.peek().length > 0) {
        batch(() => { filterText.set(filterText.peek().slice(0, -1)); selectedIndex.set(0); });
      }
      return true;
    }
    if (key.char && !key.ctrl && !key.alt) {
      batch(() => { filterText.set(filterText.peek() + key.char); selectedIndex.set(0); });
      return true;
    }
    return true;
  },
});

// ============================================================
// Create screen (multi-step wizard)
// ============================================================

function renderDirInitialUI(ctx: ScreenContext): UINode[] {
  const cwd = cwdPath.get();
  const idx = createSelectedIndex.get();
  const items = [
    { label: shortPath(cwd) + "  (current directory)" },
    { label: "Choose disk location\u2026" },
  ];
  const region = updateScrollRegion(
    { offset: 0, selectedIndex: idx, totalItems: items.length, viewportHeight: 10 },
    items.length, 10,
  );
  return [
    panel("New Session \u2014 Choose Directory", [
      selectable(region, items, (item, _i, selected) => [
        text(selected ? "  " + item.label : "  " + item.label,
          selected ? "accent" : "primary", { bold: selected, truncate: true }),
      ]),
    ]),
    footer("\u2191\u2193 select  \u23ce confirm  esc back"),
  ];
}

function renderDirBrowseUI(ctx: ScreenContext): UINode[] {
  const bp = browsePath.get();
  const bf = browseFilter.get();
  const dirs = listDirs(bp, bf);
  const idx = createSelectedIndex.get();

  const items = [
    { label: "[Select this directory]", dim: true },
    { label: "..", dim: true },
    ...dirs.map(d => ({ label: d + "/", dim: false })),
  ];
  const region = updateScrollRegion(
    { offset: 0, selectedIndex: idx, totalItems: items.length, viewportHeight: Math.max(1, ctx.rows - 6) },
    items.length, Math.max(1, ctx.rows - 6),
  );

  const filterLine = bf
    ? text("  Filter: " + bf, "primary")
    : null;

  return [
    panel("Browse \u2014 " + shortPath(bp), [
      ...(filterLine ? [filterLine, text("", "muted")] : []),
      selectable(region, items, (item, _i, selected) => [
        text(selected ? "  " + item.label : "  " + item.label,
          selected ? "accent" : (item.dim ? "muted" : "primary"),
          { bold: selected, truncate: true }),
      ]),
    ]),
    footer("\u2191\u2193 select  \u23ce enter  esc back  type to filter"),
  ];
}

function renderNameCommandUI(ctx: ScreenContext): UINode[] {
  const bp = browsePath.peek();
  const cwd = cwdPath.peek();
  const dir = bp !== cwd ? bp : cwd;
  const focused = focusedField.get();
  const name = sessionName.get();
  const cmd = sessionCommand.get();

  return [
    panel("New Session", [
      text("  Directory: " + shortPath(dir), "muted"),
      text("", "muted"),
      row(
        text("  Name:    ", focused === "name" ? "accent" : "muted", { bold: focused === "name" }),
        text(name + (focused === "name" ? "\u2588" : ""), "primary"),
      ),
      row(
        text("  Command: ", focused === "command" ? "accent" : "muted", { bold: focused === "command" }),
        text(cmd + (focused === "command" ? "\u2588" : ""), "primary"),
      ),
      canvas(() => {}, {}), // flex spacer
    ]),
    footer("\u21e5 switch field  \u23ce create  esc back"),
  ];
}

const createScreen = screen({
  id: "create",

  render(ctx: ScreenContext): UINode[] {
    const step = createStep.get();
    if (step === "dir-initial") return renderDirInitialUI(ctx);
    if (step === "dir-browse") return renderDirBrowseUI(ctx);
    return renderNameCommandUI(ctx);
  },

  handleKey(key: KeyEvent, _ctx: ScreenContext): boolean {
    const step = createStep.peek();
    if (step === "dir-initial") return handleDirInitialKey(key);
    if (step === "dir-browse") return handleDirBrowseKey(key);
    return handleNameCommandKey(key);
  },
});

function handleDirInitialKey(key: KeyEvent): boolean {
  if (key.name === "up") { createSelectedIndex.set(0); return true; }
  if (key.name === "down") { createSelectedIndex.set(1); return true; }
  if (key.name === "return") {
    if (createSelectedIndex.peek() === 0) {
      batch(() => {
        createStep.set("name-command");
        sessionName.set(dedupName(path.basename(cwdPath.peek()), existingNames.peek()));
      });
    } else {
      batch(() => {
        createStep.set("dir-browse");
        browsePath.set(cwdPath.peek());
        createSelectedIndex.set(0);
        browseFilter.set("");
      });
    }
    return true;
  }
  if (key.name === "escape" || (key.name === "c" && key.ctrl)) {
    batch(() => { currentScreen.set("list"); });
    return true;
  }
  return true;
}

function handleDirBrowseKey(key: KeyEvent): boolean {
  const dirs = listDirs(browsePath.peek(), browseFilter.peek());
  const totalItems = 2 + dirs.length;
  const idx = createSelectedIndex.peek();

  if (key.name === "up") { createSelectedIndex.set(Math.max(0, idx - 1)); return true; }
  if (key.name === "down") { createSelectedIndex.set(Math.min(totalItems - 1, idx + 1)); return true; }
  if (key.name === "return") {
    if (idx === 0) {
      batch(() => {
        createStep.set("name-command");
        sessionName.set(dedupName(path.basename(browsePath.peek()), existingNames.peek()));
      });
    } else if (idx === 1) {
      const parent = path.dirname(browsePath.peek());
      if (parent !== browsePath.peek()) {
        batch(() => { browsePath.set(parent); createSelectedIndex.set(0); browseFilter.set(""); });
      }
    } else {
      const dirName = dirs[idx - 2];
      if (dirName) {
        batch(() => {
          browsePath.set(path.join(browsePath.peek(), dirName));
          createSelectedIndex.set(0);
          browseFilter.set("");
        });
      }
    }
    return true;
  }
  if (key.name === "escape") {
    if (browseFilter.peek()) {
      batch(() => { browseFilter.set(""); createSelectedIndex.set(0); });
      return true;
    }
    batch(() => { createStep.set("dir-initial"); createSelectedIndex.set(0); });
    return true;
  }
  if (key.name === "c" && key.ctrl) {
    batch(() => { currentScreen.set("list"); });
    return true;
  }
  if (key.name === "backspace") {
    if (browseFilter.peek().length > 0) {
      const newFilter = browseFilter.peek().slice(0, -1);
      const newDirs = listDirs(browsePath.peek(), newFilter);
      batch(() => {
        browseFilter.set(newFilter);
        createSelectedIndex.set(Math.min(idx, 1 + newDirs.length));
      });
    }
    return true;
  }
  if (key.char && !key.ctrl && !key.alt) {
    batch(() => { browseFilter.set(browseFilter.peek() + key.char); createSelectedIndex.set(2); });
    return true;
  }
  return true;
}

function handleNameCommandKey(key: KeyEvent): boolean {
  if (key.name === "tab") {
    focusedField.set(focusedField.peek() === "name" ? "command" : "name");
    return true;
  }
  if (key.name === "return") {
    const name = sessionName.peek().trim();
    const cmd = sessionCommand.peek().trim();
    if (name && cmd) {
      const dir = browsePath.peek() !== cwdPath.peek() ? browsePath.peek() : cwdPath.peek();
      doCreate(dir, name, cmd);
    }
    return true;
  }
  if (key.name === "escape") {
    batch(() => { createStep.set("dir-initial"); createSelectedIndex.set(0); });
    return true;
  }
  if (key.name === "c" && key.ctrl) {
    batch(() => { currentScreen.set("list"); });
    return true;
  }
  if (key.name === "backspace") {
    if (focusedField.peek() === "name") {
      sessionName.set(sessionName.peek().slice(0, -1));
    } else {
      sessionCommand.set(sessionCommand.peek().slice(0, -1));
    }
    return true;
  }
  if (key.char && !key.ctrl && !key.alt) {
    if (focusedField.peek() === "name") {
      sessionName.set(sessionName.peek() + key.char);
    } else {
      sessionCommand.set(sessionCommand.peek() + key.char);
    }
    return true;
  }
  return true;
}

// ============================================================
// Attach / Create
// ============================================================

let myApp: ReturnType<typeof app> | null = null;

function doAttach(name: string): void {
  myApp?.pause();
  attach({
    name,
    onDetach: async () => {
      const updated = await listSessions();
      batch(() => {
        sessions.set(updated);
        currentScreen.set("list");
        filterText.set("");
        const maxIdx = Math.max(0, filteredItems.get().length - 1);
        if (selectedIndex.peek() > maxIdx) selectedIndex.set(maxIdx);
      });
      myApp?.resume();
    },
    onExit: async (_code) => {
      const updated = await listSessions();
      batch(() => {
        sessions.set(updated);
        currentScreen.set("list");
        filterText.set("");
        const maxIdx = Math.max(0, filteredItems.get().length - 1);
        if (selectedIndex.peek() > maxIdx) selectedIndex.set(maxIdx);
      });
      myApp?.resume();
    },
  });
}

async function doCreate(dir: string, name: string, command: string): Promise<void> {
  myApp?.pause();

  try {
    validateName(name);
  } catch (e: any) {
    console.error(e.message);
    const updated = await listSessions();
    sessions.set(updated);
    currentScreen.set("list");
    myApp?.resume();
    return;
  }

  let existing;
  try {
    existing = await getSession(name);
  } catch {
    existing = null;
  }

  if (existing?.status === "running") {
    doAttach(name);
    return;
  }

  if (!acquireLock(name)) {
    console.error(`Session "${name}" is being created by another process.`);
    const updated = await listSessions();
    sessions.set(updated);
    currentScreen.set("list");
    myApp?.resume();
    return;
  }

  if (existing?.status === "exited") {
    cleanupAll(name);
  }

  const parts = command.split(/\s+/);
  const cmd = parts[0];
  const args = parts.slice(1);

  let resolvedCmd: string;
  try {
    resolvedCmd = resolveCommand(cmd);
  } catch (e: any) {
    releaseLock(name);
    console.error(e.message);
    const updated = await listSessions();
    sessions.set(updated);
    currentScreen.set("list");
    myApp?.resume();
    return;
  }

  try {
    await spawnDaemon(name, resolvedCmd, args, cmd, dir);
  } catch (e: any) {
    releaseLock(name);
    console.error(e.message);
    const updated = await listSessions();
    sessions.set(updated);
    currentScreen.set("list");
    myApp?.resume();
    return;
  } finally {
    releaseLock(name);
  }

  doAttach(name);
}

// ============================================================
// Entry point
// ============================================================

export async function runInteractive(): Promise<void> {
  const sessionList = await listSessions();
  sessions.set(sessionList);

  myApp = app({
    screen: () => currentScreen.get() === "list" ? listScreen : createScreen,
  });

  myApp.start();
}
