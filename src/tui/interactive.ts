// Interactive session list — built with the declarative TUI framework + app()
import * as fs from "node:fs";
import * as path from "node:path";
import { execFileSync, spawn as spawnChild, spawnSync } from "node:child_process";
import { attach } from "../client.ts";
import {
  listSessions, validateName, acquireLock, releaseLock,
  cleanupAll, getSession, getSessionDir, type SessionInfo,
} from "../sessions.ts";
import { spawnDaemon, resolveCommand } from "../spawn.ts";
import {
  app, screen, signal, computed, batch,
  text, row, spacer, panel, selectable, footer, canvas,
  groupedSelectable, type SelectableGroup,
  updateScrollRegion, themes,
  type KeyEvent, type ScreenContext, type UINode,
} from "./index.ts";
// Reuse utility functions from the existing screen modules
import { sortSessions, shortPath, timeAgo } from "./screen-list.ts";
import { dedupName, listDirs } from "./screen-create.ts";
import { fuzzyMatch } from "./fuzzy.ts";

/** Generate a session name from dir and command. */
function autoName(dir: string, cmd: string, cmdArgs: string[]): string {
  const dirPart = path.basename(dir);
  const cmdBase = path.basename(cmd);
  const firstArg = cmdArgs.find(a => !a.startsWith("-") && a.length < 30);
  let cmdPart = cmdBase;
  if (firstArg) {
    const argBase = path.basename(firstArg).replace(/\.[^.]+$/, "");
    if (argBase && /^[a-zA-Z0-9._-]+$/.test(argBase)) {
      cmdPart = `${cmdBase}-${argBase}`;
    }
  }
  const raw = `${dirPart}-${cmdPart}`;
  return raw.replace(/[^a-zA-Z0-9._-]/g, "-").replace(/-+/g, "-").replace(/^-|-$/g, "");
}

// ============================================================
// State (signals)
// ============================================================

const sessions = signal<SessionInfo[]>([]);
const filterText = signal("");
const selectedIndex = signal(0);
const currentScreen = signal<"list" | "create" | "remote-create">("list");

// Theme — persisted to ~/.local/state/pty/theme
const themeNames = Object.keys(themes);
const terminalIdx = themeNames.indexOf("terminal");

function loadSavedThemeIndex(): number {
  try {
    const name = fs.readFileSync(path.join(getSessionDir(), "theme"), "utf-8").trim();
    const idx = themeNames.indexOf(name);
    if (idx >= 0) return idx;
  } catch {}
  return terminalIdx >= 0 ? terminalIdx : 0;
}

function saveTheme(name: string): void {
  try {
    fs.writeFileSync(path.join(getSessionDir(), "theme"), name + "\n");
  } catch {}
}

const themeIndex = signal(loadSavedThemeIndex());
function cycleTheme(): void {
  const next = (themeIndex.peek() + 1) % themeNames.length;
  themeIndex.set(next);
  saveTheme(themeNames[next]);
}
function currentTheme() {
  return themes[themeNames[themeIndex.get()]];
}

// Create wizard state
const createStep = signal<"dir-initial" | "dir-browse" | "name-command">("dir-initial");
const cwdPath = signal(process.cwd());
const browsePath = signal(process.cwd());
const browseFilter = signal("");
const createSelectedIndex = signal(0);
const sessionName = signal("");
const sessionCommand = signal("");
const nameManuallyEdited = signal(false);
const focusedField = signal<"name" | "command">("command");
const existingNames = signal<Set<string>>(new Set());

// Remote create wizard state
const remoteCreateHost = signal<RelayHost | null>(null);
const remoteSessionName = signal("");
const remoteSessionCommand = signal("bash");
const remoteFocusedField = signal<"name" | "command">("name");

// ============================================================
// Relay integration
// ============================================================

interface RemoteSession {
  name: string;
  status: string;
  command?: string;
  cwd?: string;
}

interface RelayHost {
  label: string;
  url: string;
  sessions: RemoteSession[];
  spawn_enabled: boolean;
  error: string | null;
}

let relayBin: string | null = null;
try {
  relayBin = execFileSync("which", ["pty-relay"], { encoding: "utf-8" }).trim();
} catch {}

const relayHosts = signal<RelayHost[]>([]);

/** Fetch remote sessions from pty-relay asynchronously.
 *  Updates the relayHosts signal when data arrives, triggering a re-render. */
function refreshRelayHosts(): void {
  if (!relayBin) return;
  const child = spawnChild(relayBin, ["ls", "--json"], {
    stdio: ["ignore", "pipe", "ignore"],
    timeout: 10000,
  });
  let stdout = "";
  child.stdout?.on("data", (d: Buffer) => { stdout += d.toString(); });
  child.on("close", (code) => {
    if (code === 0 && stdout.trim()) {
      try {
        relayHosts.set(JSON.parse(stdout));
      } catch {}
    }
  });
}

// ============================================================
// Computed values
// ============================================================

interface ListItem {
  type: "session" | "create" | "remote" | "remote-create";
  session?: SessionInfo;
  remote?: { host: RelayHost; session: RemoteSession };
  remoteHost?: RelayHost;
}

const sortedSessions = computed<SessionInfo[]>(() => sortSessions(sessions.get()));

function filterAndSort(filter: string, items: ListItem[]): ListItem[] {
  if (!filter) return items;
  const matches: { item: ListItem; score: number }[] = [];
  for (const item of items) {
    let name = "";
    let cmd = "";
    let cwd = "";
    if (item.type === "session" && item.session) {
      name = item.session.name;
      cmd = item.session.metadata?.displayCommand ?? "";
      cwd = item.session.metadata?.cwd ?? "";
    } else if (item.type === "remote" && item.remote) {
      name = item.remote.session.name;
      cmd = item.remote.session.command ?? "";
      cwd = item.remote.session.cwd ?? "";
    } else {
      continue; // skip "create" items during filter
    }
    const nameResult = fuzzyMatch(filter, name);
    const cwdResult = fuzzyMatch(filter, cwd);
    const cmdResult = fuzzyMatch(filter, cmd);
    if (!nameResult.match && !cwdResult.match && !cmdResult.match) continue;
    const runningBonus = (item.type === "session" && item.session?.status === "running") || (item.type === "remote" && item.remote?.session.status === "running") ? 100000 : 0;
    const score = runningBonus + Math.max(
      nameResult.match ? nameResult.score + 10000 : 0,
      cwdResult.match ? cwdResult.score : 0,
      cmdResult.match ? cmdResult.score : 0,
    );
    matches.push({ item, score });
  }
  matches.sort((a, b) => b.score - a.score);
  return matches.map(m => m.item);
}

const filteredGroups = computed<SelectableGroup<ListItem>[]>(() => {
  const filter = filterText.get();

  // Local group
  const localItems: ListItem[] = sortedSessions.get().map(s => ({ type: "session" as const, session: s }));
  const filteredLocal = filter ? filterAndSort(filter, localItems) : localItems;
  // Always add "Create new session" at the end of local group
  const localWithCreate: ListItem[] = [...filteredLocal, { type: "create" }];

  const groups: SelectableGroup<ListItem>[] = [
    { title: "Local", items: localWithCreate },
  ];

  // Remote groups from relay
  for (const host of relayHosts.get()) {
    if (host.error) continue;
    const remoteItems: ListItem[] = host.sessions.map(s => ({
      type: "remote" as const,
      remote: { host, session: s },
    }));
    const filtered = filter ? filterAndSort(filter, remoteItems) : remoteItems;
    const items: ListItem[] = [...filtered];
    if (host.spawn_enabled) {
      items.push({ type: "remote-create", remoteHost: host });
    }
    if (items.length > 0 || !filter) {
      groups.push({ title: host.label, items });
    }
  }

  return groups;
});

// Total item count across all groups (for scroll region)
const totalItems = computed(() => {
  return filteredGroups.get().reduce((sum, g) => sum + g.items.length, 0);
});

// ============================================================
// List screen
// ============================================================

function renderListItem(item: ListItem, _index: number, selected: boolean): UINode[] {
  const sel = selected ? "\u25b8 " : "  ";
  if (item.type === "create") {
    return [text(sel + "+ Create new session...", selected ? "accent" : "muted", { bold: selected, truncate: true })];
  }

  if (item.type === "remote-create") {
    return [text(sel + "+ Spawn remote session...", selected ? "accent" : "muted", { bold: selected, truncate: true })];
  }

  if (item.type === "remote" && item.remote) {
    const rs = item.remote.session;
    const icon = rs.status === "running" ? "\u25cf" : "\u25cb";
    const cwdStr = rs.cwd ? shortPath(rs.cwd) : "";
    const cmd = rs.command ?? "";
    const nameStr = `${sel}${icon} ${rs.name}`;
    const detailStr = `  ${cwdStr}  ${cmd}`;
    const line = nameStr + detailStr;

    if (selected) {
      return [text(line, "accent", { bold: true, truncate: true })];
    }
    return [
      text(nameStr, rs.status === "running" ? "primary" : "muted", { bold: true }),
      text(detailStr, "muted", { dim: true, truncate: true }),
    ];
  }

  const s = item.session!;
  const icon = s.status === "running" ? "\u25cf" : "\u25cb";
  const cmd = s.metadata
    ? s.metadata.displayCommand ?? ""
    : "";
  const cwdStr = s.metadata?.cwd ? shortPath(s.metadata.cwd) : "";
  const exitStr = s.metadata?.exitedAt ? `(exited ${timeAgo(new Date(s.metadata.exitedAt))})` : "";
  const pathStr = s.status === "running"
    ? cwdStr
    : [cwdStr, exitStr].filter(Boolean).join("  ");

  const supStatus = s.metadata?.tags?.["supervisor.status"];
  const strategy = s.metadata?.tags?.strategy;
  const marker = supStatus === "failed" ? " [failed]" : strategy === "permanent" ? " [permanent]" : strategy === "temporary" ? " [temporary]" : "";

  const nameStr = `${sel}${icon} ${s.name}${marker}`;
  const detailStr = `  ${pathStr}  ${cmd}`;
  const line = nameStr + detailStr;

  if (selected) {
    return [text(line, "accent", { bold: true, truncate: true })];
  }
  return [
    text(nameStr, s.status === "running" ? "primary" : "muted", { bold: true }),
    text(detailStr, "muted", { dim: true, truncate: true }),
  ];
}

const listScreen = screen({
  id: "list",

  render(ctx: ScreenContext): UINode[] {
    const groups = filteredGroups.get();
    const total = totalItems.get();
    const viewport = Math.max(1, ctx.rows - 6);
    const region = updateScrollRegion(
      { offset: 0, selectedIndex: selectedIndex.get(), totalItems: total, viewportHeight: viewport },
      total,
      viewport,
    );

    const filter = filterText.get();
    const filterLine = filter
      ? text("  Filter: " + filter, "primary")
      : text("  Filter: (type to filter)", "muted", { dim: true });

    const hasRelay = relayHosts.get().length > 0;

    return [
      panel("pty", [
        filterLine,
        text("", "muted"), // blank line
        hasRelay
          ? groupedSelectable(region, groups, renderListItem)
          : groupedSelectable(region, groups, renderListItem, () => []),
      ]),
      footer(`\u2191\u2193 select  \u23ce attach  ctrl+g theme (${themeNames[themeIndex.get()]})  q quit`),
    ];
  },

  handleKey(key: KeyEvent, _ctx: ScreenContext): boolean {
    const total = totalItems.get();
    const idx = selectedIndex.peek();
    const maxIndex = total - 1;

    // Resolve the item at the current global index across all groups
    function getItemAtIndex(globalIdx: number): ListItem | null {
      let i = 0;
      for (const group of filteredGroups.get()) {
        for (const item of group.items) {
          if (i === globalIdx) return item;
          i++;
        }
      }
      return null;
    }

    if (key.name === "up") {
      selectedIndex.set(Math.max(0, idx - 1));
      return true;
    }
    if (key.name === "down") {
      selectedIndex.set(Math.min(maxIndex, idx + 1));
      return true;
    }
    if (key.name === "return") {
      const item = getItemAtIndex(idx);
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
      if (item.type === "remote-create" && item.remoteHost) {
        batch(() => {
          remoteCreateHost.set(item.remoteHost!);
          remoteSessionName.set("");
          remoteSessionCommand.set("bash");
          remoteFocusedField.set("name");
          currentScreen.set("remote-create");
        });
        return true;
      }
      if (item.type === "remote" && item.remote) {
        doAttachRemote(item.remote.host, item.remote.session);
        return true;
      }
      if (item.session) {
        if (item.session.status === "exited") {
          doRestart(item.session);
        } else {
          doAttach(item.session.name);
        }
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
        nameManuallyEdited.set(false);
        sessionCommand.set("");
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
        nameManuallyEdited.set(false);
        sessionCommand.set("");
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
      nameManuallyEdited.set(true);
      sessionName.set(sessionName.peek().slice(0, -1));
    } else {
      sessionCommand.set(sessionCommand.peek().slice(0, -1));
      regenerateNameIfAuto();
    }
    return true;
  }
  if (key.char && !key.ctrl && !key.alt) {
    if (focusedField.peek() === "name") {
      nameManuallyEdited.set(true);
      sessionName.set(sessionName.peek() + key.char);
    } else {
      sessionCommand.set(sessionCommand.peek() + key.char);
      regenerateNameIfAuto();
    }
    return true;
  }
  return true;
}

function regenerateNameIfAuto(): void {
  if (nameManuallyEdited.peek()) return;
  const dir = browsePath.peek() !== cwdPath.peek() ? browsePath.peek() : cwdPath.peek();
  const cmd = sessionCommand.peek().trim();
  if (!cmd) {
    sessionName.set(dedupName(path.basename(dir), existingNames.peek()));
    return;
  }
  const parts = cmd.split(/\s+/);
  const name = autoName(dir, parts[0], parts.slice(1));
  sessionName.set(dedupName(name, existingNames.peek()));
}

// ============================================================
// Attach / Create
// ============================================================

let myApp: ReturnType<typeof app> | null = null;

async function doRestart(session: SessionInfo): Promise<void> {
  const meta = session.metadata;
  if (!meta) {
    cleanupAll(session.name);
    return;
  }
  cleanupAll(session.name);
  try {
    await spawnDaemon({ name: session.name, command: meta.command, args: meta.args, displayCommand: meta.displayCommand, cwd: meta.cwd, tags: meta.tags });
  } catch {
    // Refresh list to show updated state
    const updated = await listSessions();
    sessions.set(updated);
    return;
  }
  doAttach(session.name);
}

function doAttachRemote(host: RelayHost, session: RemoteSession): void {
  if (!relayBin) return;
  myApp?.pause();

  // Build the connect URL: base URL + /session-name
  const url = host.url.replace(/#.*$/, "") + "/" + session.name +
    (host.url.includes("#") ? "#" + host.url.split("#").slice(1).join("#") : "");

  const result = spawnSync(relayBin, ["connect", url], {
    stdio: "inherit",
  });

  // Refresh and resume
  (async () => {
    const updated = await listSessions();
    refreshRelayHosts();
    batch(() => {
      sessions.set(updated);
      currentScreen.set("list");
      filterText.set("");
      const maxIdx = Math.max(0, totalItems.get() - 1);
      if (selectedIndex.peek() > maxIdx) selectedIndex.set(maxIdx);
    });
    myApp?.resume();
  })();
}

function doSpawnRemote(host: RelayHost, name: string): void {
  if (!relayBin) return;
  myApp?.pause();

  const result = spawnSync(relayBin, ["connect", host.url, "--spawn", name], {
    stdio: "inherit",
  });

  (async () => {
    const updated = await listSessions();
    refreshRelayHosts();
    batch(() => {
      sessions.set(updated);
      currentScreen.set("list");
      filterText.set("");
      const maxIdx = Math.max(0, totalItems.get() - 1);
      if (selectedIndex.peek() > maxIdx) selectedIndex.set(maxIdx);
    });
    myApp?.resume();
  })();
}

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
        const maxIdx = Math.max(0, totalItems.get() - 1);
        if (selectedIndex.peek() > maxIdx) selectedIndex.set(maxIdx);
      });
      myApp?.resume();
    },
    onExit: async (_code) => {
      // Brief delay to let the daemon write exit metadata to disk
      await new Promise((r) => setTimeout(r, 200));
      const updated = await listSessions();
      batch(() => {
        sessions.set(updated);
        currentScreen.set("list");
        filterText.set("");
        const maxIdx = Math.max(0, totalItems.get() - 1);
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

  // Spawn via sh -c so the command field supports quotes, pipes, env vars, etc.
  const shellCmd = "/bin/sh";
  const shellArgs = ["-c", command];

  try {
    await spawnDaemon({ name, command: shellCmd, args: shellArgs, displayCommand: command, cwd: dir });
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
// Remote create screen
// ============================================================

const remoteCreateScreen = screen({
  id: "remote-create",

  render(_ctx: ScreenContext): UINode[] {
    const host = remoteCreateHost.get();
    if (!host) return [text("No host selected", "error")];

    const name = remoteSessionName.get();

    return [
      panel(`Spawn on ${host.label}`, [
        text("", "muted"),
        text("  Session name: " + name + "\u2588", "primary"),
        text("", "muted"),
        text("  Enter to spawn, Escape to cancel", "muted", { dim: true }),
      ]),
    ];
  },

  handleKey(key: KeyEvent, _ctx: ScreenContext): boolean {
    if (key.name === "escape") {
      batch(() => {
        currentScreen.set("list");
        remoteCreateHost.set(null);
      });
      return true;
    }

    if (key.name === "return") {
      const host = remoteCreateHost.peek();
      const name = remoteSessionName.peek().trim();
      if (!host || !name) return true;
      doSpawnRemote(host, name);
      return true;
    }

    if (key.name === "backspace") {
      remoteSessionName.set(remoteSessionName.peek().slice(0, -1));
      return true;
    }

    if (key.char && !key.ctrl && !key.alt) {
      remoteSessionName.set(remoteSessionName.peek() + key.char);
      return true;
    }

    return true;
  },
});

// ============================================================
// Entry point
// ============================================================

export async function runInteractive(): Promise<void> {
  const sessionList = await listSessions();
  sessions.set(sessionList);

  // Fetch relay hosts in the background (non-blocking)
  refreshRelayHosts();

  myApp = app({
    screen: () => {
      const s = currentScreen.get();
      if (s === "remote-create") return remoteCreateScreen;
      if (s === "create") return createScreen;
      return listScreen;
    },
    theme: () => currentTheme(),
    onKey: (key) => {
      if (key.name === "g" && key.ctrl) { cycleTheme(); return true; }
      return false;
    },
  });

  myApp.start();
}
