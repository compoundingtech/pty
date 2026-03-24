// Agent teams state: signals, file watcher, agent tree parsing
import * as fs from "node:fs";
import * as path from "node:path";
import {
  signal, computed, batch,
  createScrollRegion, updateScrollRegion, scrollUp, scrollDown,
  themes,
  type ScrollRegion, type Theme, type BoxStyle,
} from "../../src/tui/index.ts";

// --- Types ---
export interface AgentNode {
  name: string;
  path: string;
  status: "idle" | "working" | "blocked" | "done" | "error";
  task: string;
  progress: number;
  started: string;
  updated: string;
  body: string;
  plan: string;
  output: string;
  children: AgentNode[];
  depth: number;
}

export interface ActivityEntry {
  timestamp: string;
  agent: string;
  event: string;
}

// --- Theme ---
const themeNames = Object.keys(themes);
export const themeIndex = signal(0);
export const currentTheme = computed<Theme>(() => themes[themeNames[themeIndex.get()]] as Theme);
export const boxStyle = signal<BoxStyle>("rounded");

export function cycleTheme(): void {
  themeIndex.set((themeIndex.peek() + 1) % themeNames.length);
}

// --- Data directory ---
export const dataDir = signal(`/tmp/agent-teams-demo-${process.pid}/`);

// --- Signals ---
export const rootAgent = signal<AgentNode | null>(null);
export const selectedIndex = signal(0);
export const agentScroll = signal<ScrollRegion>(createScrollRegion(0, 20));
export const activityLog = signal<ActivityEntry[]>([]);
export const elapsedTime = signal(0);
export const paused = signal(false);

// --- Flatten agents ---
export const flatAgents = computed<AgentNode[]>(() => {
  const root = rootAgent.get();
  if (!root) return [];
  const result: AgentNode[] = [];
  function walk(node: AgentNode): void {
    result.push(node);
    for (const child of node.children) {
      walk(child);
    }
  }
  walk(root);
  return result;
});

export const selectedAgent = computed<AgentNode | null>(() => {
  const list = flatAgents.get();
  const idx = selectedIndex.get();
  return list[idx] ?? null;
});

export const agentCount = computed<number>(() => flatAgents.get().length);

// --- Parse status file ---
export function parseStatusFile(filePath: string): Partial<AgentNode> {
  try {
    const content = fs.readFileSync(filePath, "utf-8");
    const lines = content.split("\n");
    const meta: Record<string, string> = {};
    let bodyLines: string[] = [];

    if (lines[0] === "---") {
      let endIdx = -1;
      for (let i = 1; i < lines.length; i++) {
        if (lines[i] === "---") { endIdx = i; break; }
      }
      if (endIdx > 0) {
        for (let i = 1; i < endIdx; i++) {
          const colonIdx = lines[i].indexOf(":");
          if (colonIdx === -1) continue;
          meta[lines[i].slice(0, colonIdx).trim()] = lines[i].slice(colonIdx + 1).trim();
        }
        bodyLines = lines.slice(endIdx + 1);
      }
    }

    return {
      name: meta.name ?? "",
      status: (meta.status as AgentNode["status"]) ?? "idle",
      task: meta.task ?? "",
      progress: parseFloat(meta.progress ?? "0"),
      started: meta.started ?? "",
      updated: meta.updated ?? "",
      body: bodyLines.join("\n").trim(),
    };
  } catch {
    return {};
  }
}

function readFileOr(filePath: string): string {
  try {
    return fs.readFileSync(filePath, "utf-8").trim();
  } catch {
    return "";
  }
}

// --- Parse agent directory recursively ---
export function parseAgentDir(dirPath: string, depth: number): AgentNode | null {
  const statusPath = path.join(dirPath, "status.md");
  if (!fs.existsSync(statusPath)) return null;

  const parsed = parseStatusFile(statusPath);
  const plan = readFileOr(path.join(dirPath, "plan.md"));
  const output = readFileOr(path.join(dirPath, "output.md"));

  const children: AgentNode[] = [];
  const subAgentsDir = path.join(dirPath, "sub-agents");
  if (fs.existsSync(subAgentsDir)) {
    const entries = fs.readdirSync(subAgentsDir, { withFileTypes: true });
    for (const entry of entries) {
      if (entry.isDirectory()) {
        const child = parseAgentDir(path.join(subAgentsDir, entry.name), depth + 1);
        if (child) children.push(child);
      }
    }
  }

  return {
    name: parsed.name || path.basename(dirPath),
    path: dirPath,
    status: parsed.status ?? "idle",
    task: parsed.task ?? "",
    progress: parsed.progress ?? 0,
    started: parsed.started ?? "",
    updated: parsed.updated ?? "",
    body: parsed.body ?? "",
    plan,
    output,
    children,
    depth,
  };
}

// --- Load from disk ---
export function reloadAgents(): void {
  const dir = dataDir.peek();
  const mainDir = path.join(dir, "main-agent");
  if (!fs.existsSync(mainDir)) return;
  const root = parseAgentDir(mainDir, 0);
  if (root) {
    rootAgent.set(root);
    const list = flatAgents.get();
    agentScroll.set(updateScrollRegion(agentScroll.peek(), list.length));
  }
}

// --- Activity log ---
export function addActivity(agent: string, event: string, timestamp?: string): void {
  const ts = timestamp ?? (() => {
    const now = new Date();
    return `${String(now.getHours()).padStart(2, "0")}:${String(now.getMinutes()).padStart(2, "0")}`;
  })();
  const log = activityLog.peek();
  activityLog.set([...log, { timestamp: ts, agent, event }]);
}

// --- File watcher ---
let watchDebounce: ReturnType<typeof setTimeout> | null = null;
let watcher: fs.FSWatcher | null = null;

export function startWatching(): void {
  const dir = dataDir.peek();
  try {
    watcher = fs.watch(dir, { recursive: true }, () => {
      if (watchDebounce) clearTimeout(watchDebounce);
      watchDebounce = setTimeout(() => reloadAgents(), 100);
    });
  } catch {
    // Directory may not exist yet
  }
}

export function stopWatching(): void {
  if (watcher) {
    watcher.close();
    watcher = null;
  }
  if (watchDebounce) {
    clearTimeout(watchDebounce);
    watchDebounce = null;
  }
}

// --- Actions ---
export function moveUp(): void {
  const idx = selectedIndex.peek();
  if (idx <= 0) return;
  batch(() => {
    selectedIndex.set(idx - 1);
    agentScroll.set(scrollUp(agentScroll.peek()));
  });
}

export function moveDown(): void {
  const idx = selectedIndex.peek();
  const list = flatAgents.get();
  if (idx >= list.length - 1) return;
  batch(() => {
    selectedIndex.set(idx + 1);
    agentScroll.set(scrollDown(agentScroll.peek()));
  });
}

export function togglePause(): void {
  paused.set(!paused.peek());
}

// --- Format elapsed ---
export function formatElapsed(seconds: number): string {
  const m = Math.floor(seconds / 60);
  const s = seconds % 60;
  return `${m}:${String(s).padStart(2, "0")}`;
}
