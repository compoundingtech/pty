// Interactive session list — built with the declarative TUI framework + app()
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { randomBytes } from "node:crypto";
import { execFileSync, spawn as spawnChild, spawnSync } from "node:child_process";
import { attach } from "../client.ts";
import {
  listSessions, acquireLock, releaseLock,
  cleanupAll, getSession, getSessionDir, type SessionInfo,
} from "../sessions.ts";
import { spawnDaemon } from "../spawn.ts";
import { matchesAllTags, isReservedTagKey } from "../tags.ts";
import {
  app, screen, signal, computed, batch,
  text, panel, footer,
  groupedSelectable, type SelectableGroup,
  updateScrollRegion, themes,
  type KeyEvent, type ScreenContext, type UINode,
} from "./index.ts";
// Reuse utility functions from the existing screen modules
import { sortSessions, shortPath, timeAgo } from "./screen-list.ts";
import { fuzzyMatch } from "./fuzzy.ts";

/** Short random id matching src/cli.ts's randomSessionName — 8 chars of
 *  Crockford-ish base32. Used when spawning sessions from the TUI where
 *  the user didn't pick a name. */
function randomSessionName(): string {
  const alphabet = "23456789abcdefghjkmnpqrstuvwxyz";
  const bytes = randomBytes(8);
  let out = "";
  for (const b of bytes) out += alphabet[b % alphabet.length];
  return out;
}

/** The directory the shell spawned from the TUI opens in. Defaults to the
 *  user's home so "Create new session" is predictable regardless of where
 *  the `pty` binary was invoked. */
function defaultCwd(): string {
  return process.env.HOME ?? os.homedir();
}

/** The shell to spawn for a one-keystroke "Create new session". */
function defaultShell(): string {
  return process.env.SHELL ?? "bash";
}

// ============================================================
// State (signals)
// ============================================================

const sessions = signal<SessionInfo[]>([]);
const filterText = signal("");
const selectedIndex = signal(0);

/** Tag filter from `--filter-tag key=value`. Filters the list AND auto-applies
 *  to any session created from this TUI instance. */
const filterTags = signal<Record<string, string>>({});

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

// ============================================================
// Relay integration
// ============================================================

export interface RemoteSession {
  name: string;
  status: string;
  command?: string;
  cwd?: string;
  /** Optional tags reported by the relay (e.g., pty-relay ls --json). Used
   *  by the interactive TUI's --filter-tag to filter remote sessions. */
  tags?: Record<string, string>;
}

export interface RelayHost {
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

export interface ListItem {
  type: "session" | "create" | "remote" | "remote-create";
  session?: SessionInfo;
  remote?: { host: RelayHost; session: RemoteSession };
  remoteHost?: RelayHost;
}

const sortedSessions = computed<SessionInfo[]>(() => {
  const all = sortSessions(sessions.get());
  const required = filterTags.get();
  if (Object.keys(required).length === 0) return all;
  return all.filter((s) => matchesAllTags(s.metadata?.tags, required));
});

function filterAndSort(filter: string, items: ListItem[]): ListItem[] {
  if (!filter) return items;
  const matches: { item: ListItem; score: number }[] = [];
  for (const item of items) {
    let name = "";
    let displayName = "";
    let cmd = "";
    let cwd = "";
    if (item.type === "session" && item.session) {
      name = item.session.name;
      displayName = item.session.metadata?.displayName ?? "";
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
    const displayNameResult = displayName ? fuzzyMatch(filter, displayName) : { match: false, score: 0 };
    const cwdResult = fuzzyMatch(filter, cwd);
    const cmdResult = fuzzyMatch(filter, cmd);
    if (!nameResult.match && !displayNameResult.match && !cwdResult.match && !cmdResult.match) continue;
    const runningBonus = (item.type === "session" && item.session?.status === "running") || (item.type === "remote" && item.remote?.session.status === "running") ? 100000 : 0;
    const score = runningBonus + Math.max(
      displayNameResult.match ? displayNameResult.score + 10000 : 0,
      nameResult.match ? nameResult.score + 10000 : 0,
      cwdResult.match ? cwdResult.score : 0,
      cmdResult.match ? cmdResult.score : 0,
    );
    matches.push({ item, score });
  }
  matches.sort((a, b) => b.score - a.score);
  return matches.map(m => m.item);
}

/** Build filtered groups from local sessions and relay hosts.
 *  Exported for unit testing. */
export function buildFilteredGroups(
  filter: string,
  localSessions: SessionInfo[],
  hosts: RelayHost[],
): SelectableGroup<ListItem>[] {
  // Parse "host/session" filter syntax
  let hostFilter = "";
  let sessionFilter = filter;
  if (filter.includes("/")) {
    const slashIdx = filter.indexOf("/");
    hostFilter = filter.slice(0, slashIdx).trim();
    sessionFilter = filter.slice(slashIdx + 1).trim();
  }

  const showCreate = !filter || "new".startsWith(filter.toLowerCase());

  // Local group — skip if host filter is set (user is filtering by remote host)
  const groups: SelectableGroup<ListItem>[] = [];
  if (!hostFilter || fuzzyMatch(hostFilter, "local").match) {
    const localItems: ListItem[] = localSessions.map(s => ({ type: "session" as const, session: s }));
    const filteredLocal = sessionFilter ? filterAndSort(sessionFilter, localItems) : localItems;
    const localWithCreate: ListItem[] = showCreate ? [...filteredLocal, { type: "create" }] : filteredLocal;
    groups.push({ title: "Local", items: localWithCreate });
  }

  // Remote groups from relay
  for (const host of hosts) {
    if (host.error) continue;
    // If host filter is set, only include matching hosts
    if (hostFilter && !fuzzyMatch(hostFilter, host.label).match) continue;

    const remoteItems: ListItem[] = host.sessions.map(s => ({
      type: "remote" as const,
      remote: { host, session: s },
    }));
    const filtered = sessionFilter ? filterAndSort(sessionFilter, remoteItems) : remoteItems;
    const items: ListItem[] = [...filtered];
    if (host.spawn_enabled && showCreate) {
      items.push({ type: "remote-create", remoteHost: host });
    }
    if (items.length > 0 || !filter) {
      groups.push({ title: host.label, items });
    }
  }

  return groups;
}

const filteredGroups = computed<SelectableGroup<ListItem>[]>(() => {
  // When --filter-tag is active, filter remote sessions by tag too. Hosts
  // whose relay reports no tags will simply render no matching sessions.
  const required = filterTags.get();
  const requireTagMatch = Object.keys(required).length > 0;
  const hosts = requireTagMatch
    ? relayHosts.get().map((h) => ({
        ...h,
        sessions: h.sessions.filter((s) => matchesAllTags(s.tags, required)),
      }))
    : relayHosts.get();
  return buildFilteredGroups(
    filterText.get(),
    sortedSessions.get(),
    hosts,
  );
});

// Total item count across all groups (for scroll region)
const totalItems = computed(() => {
  return filteredGroups.get().reduce((sum, g) => sum + g.items.length, 0);
});

// ============================================================
// List screen
// ============================================================

/** Render user-facing tags as a " #key=value" string. Hides reserved
 *  keys (pty-internal bookkeeping + any key starting with `:`, the
 *  tool-owned-tag convention). */
function renderTagsInline(tags: Record<string, string> | undefined): string {
  if (!tags) return "";
  const entries = Object.entries(tags).filter(([k]) => !isReservedTagKey(k));
  return entries.length > 0 ? " " + entries.map(([k, v]) => `#${k}=${v}`).join(" ") : "";
}

function renderListItem(item: ListItem, _index: number, selected: boolean): UINode[] {
  const sel = selected ? "\u25b8 " : "  ";
  if (item.type === "create") {
    return [text(sel + "+ Create new session...", selected ? "accent" : "muted", { bold: selected, truncate: true })];
  }

  if (item.type === "remote-create") {
    return [text(sel + "+ Create new session...", selected ? "accent" : "muted", { bold: selected, truncate: true })];
  }

  if (item.type === "remote" && item.remote) {
    const rs = item.remote.session;
    const icon = rs.status === "running" ? "\u25cf" : "\u25cb";
    const cwdStr = rs.cwd ? shortPath(rs.cwd) : "";
    const cmd = rs.command ?? "";
    const tagStr = renderTagsInline(rs.tags);
    const nameStr = `${sel}${icon} ${rs.name}${tagStr}`;
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
  const tagStr = renderTagsInline(s.metadata?.tags);

  // Prefer displayName for the primary label; show the stable id secondarily
  // when both exist, so users can still type either in CLI commands.
  const displayName = s.metadata?.displayName;
  const labelStr = displayName ? `${displayName} (${s.name})` : s.name;

  const nameStr = `${sel}${icon} ${labelStr}${marker}${tagStr}`;
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
    const tagFilter = filterTags.get();
    const tagFilterStr = Object.entries(tagFilter).map(([k, v]) => `#${k}=${v}`).join(" ");
    const filterLine = filter
      ? text("  Filter: " + filter + (tagFilterStr ? "  " + tagFilterStr : ""), "primary")
      : tagFilterStr
        ? text("  Filter: " + tagFilterStr, "primary")
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

  handleKey(key: KeyEvent, ctx: ScreenContext): boolean {
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
        // One-keystroke local create: spawn the user's shell in $HOME with a
        // random id and no displayName. The user can `pty rename` and/or
        // `pty exec` from inside to promote it.
        doCreate(defaultCwd(), randomSessionName(), defaultShell());
        return true;
      }
      if (item.type === "remote-create" && item.remoteHost) {
        // One-keystroke remote create: ask pty-relay to spawn a shell on the
        // remote host with a random id. The relay is responsible for the
        // remote-side shell/cwd defaults.
        doSpawnRemote(item.remoteHost, randomSessionName());
        return true;
      }
      if (item.type === "remote" && item.remote) {
        doAttachRemote(item.remote.host, item.remote.session);
        return true;
      }
      if (item.session) {
        // Both `exited` (clean) and `vanished` (killed) are dead daemons
        // that can be restarted with the same metadata.
        if (item.session.status !== "running") {
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
      ctx.quit();
      return true;
    }
    if (key.char === "q" && !key.ctrl && !key.alt && !filterText.peek()) {
      ctx.quit();
      return true;
    }
    // ctrl+c is now handled globally by app(); leave the explicit check off
    // here so the global default fires. If someone binds it in a context
    // that should NOT quit (e.g. a composer with double-ctrl-c semantics),
    // they intercept via AppConfig.onKey.
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
      filterText.set("");
      const maxIdx = Math.max(0, totalItems.get() - 1);
      if (selectedIndex.peek() > maxIdx) selectedIndex.set(maxIdx);
    });
    myApp?.resume();
  })();
}

/** Build the argv for `pty-relay connect <url> --spawn <name>` with tags
 *  forwarded as `--tag key=value`. Exported for unit testing. */
export function buildSpawnRemoteArgs(url: string, name: string, tags: Record<string, string>): string[] {
  const argv = ["connect", url, "--spawn", name];
  for (const [k, v] of Object.entries(tags)) {
    argv.push("--tag", `${k}=${v}`);
  }
  return argv;
}

function doSpawnRemote(host: RelayHost, name: string): void {
  if (!relayBin) return;
  myApp?.pause();

  // Forward --filter-tag tags to the relay so the newly-spawned remote
  // session is tagged and stays within the filtered view.
  const argv = buildSpawnRemoteArgs(host.url, name, filterTags.peek());
  const result = spawnSync(relayBin, argv, {
    stdio: "inherit",
  });

  (async () => {
    const updated = await listSessions();
    refreshRelayHosts();
    batch(() => {
      sessions.set(updated);
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
        filterText.set("");
        const maxIdx = Math.max(0, totalItems.get() - 1);
        if (selectedIndex.peek() > maxIdx) selectedIndex.set(maxIdx);
      });
      myApp?.resume();
    },
  });
}

/** Spawn a new local session with the given shell in the given cwd. Used
 *  by the one-keystroke "Create new session" path; name is a random id
 *  generated by the caller. */
async function doCreate(dir: string, name: string, shell: string): Promise<void> {
  myApp?.pause();

  // Random names come from randomSessionName() — no collision check needed,
  // but we still acquireLock to guard against multi-process races during
  // creation.
  if (!acquireLock(name)) {
    console.error(`Session "${name}" is being created by another process.`);
    const updated = await listSessions();
    sessions.set(updated);
    myApp?.resume();
    return;
  }

  // Auto-apply --filter-tag tags so the new session stays in the layout.
  const tags = filterTags.peek();
  // displayName intentionally unset — matches `pty run --no-display-name`.
  // Users run `pty rename` / `pty exec` from inside to promote it.
  const spawnOpts = {
    name,
    command: shell,
    args: [] as string[],
    displayCommand: shell,
    cwd: dir,
    ...(Object.keys(tags).length > 0 ? { tags } : {}),
  };

  try {
    await spawnDaemon(spawnOpts);
  } catch (e: any) {
    releaseLock(name);
    console.error(e.message);
    const updated = await listSessions();
    sessions.set(updated);
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

export interface RunInteractiveOptions {
  /** Pre-select the local "+ Create new session..." item on startup. */
  preselectNew?: boolean;
  /** Filter the local list to sessions with all matching tags, and apply
   *  them to any session created from this TUI. */
  filterTags?: Record<string, string>;
}

export async function runInteractive(options?: RunInteractiveOptions): Promise<void> {
  if (options?.filterTags && Object.keys(options.filterTags).length > 0) {
    filterTags.set(options.filterTags);
  }

  const sessionList = await listSessions();
  sessions.set(sessionList);

  if (options?.preselectNew) {
    const groups = filteredGroups.get();
    let idx = 0;
    outer: for (const g of groups) {
      for (const item of g.items) {
        if (item.type === "create") {
          selectedIndex.set(idx);
          break outer;
        }
        idx++;
      }
    }
  }

  // Fetch relay hosts in the background (non-blocking)
  refreshRelayHosts();

  myApp = app({
    screen: () => listScreen,
    theme: () => currentTheme(),
    onKey: (key) => {
      if (key.name === "g" && key.ctrl) { cycleTheme(); return true; }
      return false;
    },
  });

  myApp.start();
}
