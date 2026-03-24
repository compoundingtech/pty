// Reminders state: signals, file I/O, parsing, seeding
import * as fs from "node:fs";
import * as path from "node:path";
import {
  signal, computed, batch,
  createScrollRegion, updateScrollRegion, scrollUp, scrollDown,
  themes,
  type ScrollRegion, type Theme, type BoxStyle,
} from "../../src/tui/index.ts";

// --- Types ---
export interface Reminder {
  id: string;
  title: string;
  body: string;
  due: string; // YYYY-MM-DD
  priority: "low" | "medium" | "high";
  completed: boolean;
  tags: string[];
  created: string; // ISO
  filePath: string;
}

export interface ReminderGroup {
  title: string;
  items: Reminder[];
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
export const dataDir = signal(`/tmp/reminders-demo-${process.pid}/`);

// --- Signals ---
export const reminders = signal<Reminder[]>([]);
export const selectedIndex = signal(0);
export const listScroll = signal<ScrollRegion>(createScrollRegion(0, 20));

// --- View switching ---
export type ViewType = "list" | "board" | "calendar";
const viewOrder: ViewType[] = ["list", "board", "calendar"];
export const currentView = signal<ViewType>("list");
export function cycleView(): void {
  const idx = viewOrder.indexOf(currentView.peek());
  currentView.set(viewOrder[(idx + 1) % viewOrder.length]);
  selectedIndex.set(0);
}

// --- Board state ---
export const boardColumn = signal(0); // 0=todo, 1=in-progress, 2=done
export const boardScroll = signal<ScrollRegion[]>([
  createScrollRegion(0, 20),
  createScrollRegion(0, 20),
  createScrollRegion(0, 20),
]);

// --- Calendar state ---
export const calendarMonth = signal(new Date().getMonth()); // 0-11
export const calendarYear = signal(new Date().getFullYear());
export const calendarSelectedDay = signal(new Date().getDate());

// --- Frontmatter parsing ---
export function parseFrontmatter(content: string): { meta: Record<string, string>; body: string } {
  const lines = content.split("\n");
  if (lines[0] !== "---") return { meta: {}, body: content };

  let endIdx = -1;
  for (let i = 1; i < lines.length; i++) {
    if (lines[i] === "---") {
      endIdx = i;
      break;
    }
  }
  if (endIdx === -1) return { meta: {}, body: content };

  const meta: Record<string, string> = {};
  for (let i = 1; i < endIdx; i++) {
    const line = lines[i];
    const colonIdx = line.indexOf(":");
    if (colonIdx === -1) continue;
    const key = line.slice(0, colonIdx).trim();
    let value = line.slice(colonIdx + 1).trim();
    meta[key] = value;
  }

  const body = lines.slice(endIdx + 1).join("\n").trim();
  return { meta, body };
}

function parseTags(raw: string): string[] {
  // Parse "[tag1, tag2]" format
  const trimmed = raw.replace(/^\[|\]$/g, "").trim();
  if (!trimmed) return [];
  return trimmed.split(",").map(t => t.trim());
}

export function parseReminderFile(filePath: string): Reminder | null {
  try {
    const content = fs.readFileSync(filePath, "utf-8");
    const { meta, body } = parseFrontmatter(content);
    const title = body.split("\n")[0]?.replace(/^#\s*/, "") ?? "Untitled";
    const bodyText = body.split("\n").slice(1).join("\n").trim();

    return {
      id: path.basename(filePath, ".md"),
      title,
      body: bodyText,
      due: meta.due ?? "",
      priority: (meta.priority as Reminder["priority"]) ?? "medium",
      completed: meta.completed === "true",
      tags: meta.tags ? parseTags(meta.tags) : [],
      created: meta.created ?? new Date().toISOString(),
      filePath,
    };
  } catch {
    return null;
  }
}

// --- Serialize to markdown ---
export function serializeReminder(r: Reminder): string {
  const lines = [
    "---",
    `due: ${r.due}`,
    `priority: ${r.priority}`,
    `completed: ${r.completed}`,
    `tags: [${r.tags.join(", ")}]`,
    `created: ${r.created}`,
    "---",
    `# ${r.title}`,
    "",
    r.body,
  ];
  return lines.join("\n") + "\n";
}

// --- Load all reminders from disk ---
export function loadReminders(): void {
  const dir = dataDir.peek();
  if (!fs.existsSync(dir)) return;
  const files = fs.readdirSync(dir).filter(f => f.endsWith(".md")).sort();
  const parsed: Reminder[] = [];
  for (const file of files) {
    const r = parseReminderFile(path.join(dir, file));
    if (r) parsed.push(r);
  }
  reminders.set(parsed);
  listScroll.set(updateScrollRegion(listScroll.peek(), totalSelectableItems(), 20));
}

// --- Computed groupings ---
function dateLabel(due: string): string {
  if (!due) return "No Date";
  const today = new Date();
  today.setHours(0, 0, 0, 0);
  const dueDate = new Date(due + "T00:00:00");
  const diff = Math.floor((dueDate.getTime() - today.getTime()) / (1000 * 60 * 60 * 24));

  if (diff < 0) return "Overdue";
  if (diff === 0) return "Today";
  if (diff === 1) return "Tomorrow";
  if (diff <= 7) return "This Week";
  return "Later";
}

export const groupedByDate = computed<ReminderGroup[]>(() => {
  const all = reminders.get();
  const active = all.filter(r => !r.completed);
  const completed = all.filter(r => r.completed);

  const groups: Record<string, Reminder[]> = {};
  const order = ["Overdue", "Today", "Tomorrow", "This Week", "Later", "No Date"];

  for (const r of active) {
    const label = dateLabel(r.due);
    if (!groups[label]) groups[label] = [];
    groups[label].push(r);
  }

  const result: ReminderGroup[] = [];
  for (const key of order) {
    if (groups[key] && groups[key].length > 0) {
      result.push({ title: key, items: groups[key] });
    }
  }
  if (completed.length > 0) {
    result.push({ title: "Completed", items: completed });
  }
  return result;
});

export const boardColumns = computed<[Reminder[], Reminder[], Reminder[]]>(() => {
  const all = reminders.get();
  const todo = all.filter(r => !r.completed && r.priority !== "high");
  const inProgress = all.filter(r => !r.completed && r.priority === "high");
  const done = all.filter(r => r.completed);
  return [todo, inProgress, done];
});

export const calendarData = computed<Map<number, number>>(() => {
  const all = reminders.get();
  const month = calendarMonth.get();
  const year = calendarYear.get();
  const counts = new Map<number, number>();

  for (const r of all) {
    if (!r.due) continue;
    const d = new Date(r.due + "T00:00:00");
    if (d.getMonth() === month && d.getFullYear() === year) {
      counts.set(d.getDate(), (counts.get(d.getDate()) ?? 0) + 1);
    }
  }
  return counts;
});

// --- Flat list of reminders in grouped order (matches visual display order) ---
export const flatReminders = computed<Reminder[]>(() => {
  const groups = groupedByDate.get();
  const result: Reminder[] = [];
  for (const group of groups) {
    for (const item of group.items) {
      result.push(item);
    }
  }
  return result;
});

// --- Count total selectable items across groups ---
function totalSelectableItems(): number {
  return flatReminders.get().length;
}

export const selectedReminder = computed<Reminder | null>(() => {
  const flat = flatReminders.get();
  const idx = selectedIndex.get();
  return flat[idx] ?? null;
});

// --- Seed data ---
export function seedData(): void {
  const dir = dataDir.peek();
  if (!fs.existsSync(dir)) fs.mkdirSync(dir, { recursive: true });

  const today = new Date();
  const fmt = (d: Date) => d.toISOString().split("T")[0];
  const addDays = (d: Date, n: number) => new Date(d.getTime() + n * 86400000);

  const seeds: Omit<Reminder, "id" | "filePath">[] = [
    { title: "Buy groceries", body: "Milk, eggs, bread, coffee", due: fmt(today), priority: "high", completed: false, tags: ["personal", "errands"], created: addDays(today, -3).toISOString() },
    { title: "Review PR #847", body: "Check auth flow changes", due: fmt(today), priority: "high", completed: false, tags: ["work", "code"], created: addDays(today, -1).toISOString() },
    { title: "Call dentist", body: "Schedule annual checkup", due: fmt(addDays(today, 1)), priority: "medium", completed: false, tags: ["personal", "health"], created: addDays(today, -5).toISOString() },
    { title: "Write blog post", body: "TUI framework design decisions", due: fmt(addDays(today, 2)), priority: "low", completed: false, tags: ["work", "writing"], created: addDays(today, -7).toISOString() },
    { title: "Deploy v2.1", body: "Stage, test, then production", due: fmt(addDays(today, 3)), priority: "high", completed: false, tags: ["work", "ops"], created: addDays(today, -2).toISOString() },
    { title: "Plan team offsite", body: "Book venue, send invites", due: fmt(addDays(today, 5)), priority: "medium", completed: false, tags: ["work", "planning"], created: addDays(today, -10).toISOString() },
    { title: "Update dependencies", body: "Run npm audit and update", due: fmt(addDays(today, 7)), priority: "low", completed: false, tags: ["work", "maintenance"], created: addDays(today, -4).toISOString() },
    { title: "Read Designing Data Apps", body: "Chapters 5-8", due: fmt(addDays(today, 14)), priority: "low", completed: false, tags: ["personal", "learning"], created: addDays(today, -14).toISOString() },
    { title: "Fix login bug", body: "Session tokens expire prematurely", due: fmt(addDays(today, -1)), priority: "high", completed: false, tags: ["work", "bug"], created: addDays(today, -6).toISOString() },
    { title: "Prepare slides", body: "Q1 review presentation", due: fmt(addDays(today, -2)), priority: "medium", completed: false, tags: ["work", "presentation"], created: addDays(today, -8).toISOString() },
    { title: "Backup photos", body: "Sync to cloud storage", due: fmt(addDays(today, 10)), priority: "low", completed: false, tags: ["personal"], created: addDays(today, -12).toISOString() },
    { title: "Submit expense report", body: "March travel expenses", due: fmt(addDays(today, 4)), priority: "medium", completed: false, tags: ["work", "admin"], created: addDays(today, -3).toISOString() },
    { title: "Set up CI pipeline", body: "GitHub Actions for the new repo", due: fmt(today), priority: "medium", completed: true, tags: ["work", "devops"], created: addDays(today, -15).toISOString() },
    { title: "Order monitor stand", body: "The ergonomic one from the review", due: fmt(addDays(today, -5)), priority: "low", completed: true, tags: ["personal", "office"], created: addDays(today, -20).toISOString() },
    { title: "Refactor auth module", body: "Extract middleware, add tests", due: fmt(addDays(today, -3)), priority: "high", completed: true, tags: ["work", "code"], created: addDays(today, -10).toISOString() },
  ];

  for (let i = 0; i < seeds.length; i++) {
    const s = seeds[i];
    const id = `reminder-${String(i + 1).padStart(3, "0")}`;
    const r: Reminder = { ...s, id, filePath: path.join(dir, `${id}.md`) };
    fs.writeFileSync(r.filePath, serializeReminder(r));
  }
}

// --- File watcher ---
let watchDebounce: ReturnType<typeof setTimeout> | null = null;
let watcher: fs.FSWatcher | null = null;

export function startWatching(): void {
  const dir = dataDir.peek();
  try {
    watcher = fs.watch(dir, () => {
      if (watchDebounce) clearTimeout(watchDebounce);
      watchDebounce = setTimeout(() => loadReminders(), 100);
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
    listScroll.set(scrollUp(listScroll.peek()));
  });
}

export function moveDown(): void {
  const idx = selectedIndex.peek();
  const flat = flatReminders.get();
  if (idx >= flat.length - 1) return;
  batch(() => {
    selectedIndex.set(idx + 1);
    listScroll.set(scrollDown(listScroll.peek()));
  });
}

export function toggleComplete(): void {
  const r = selectedReminder.get();
  if (!r) return;
  r.completed = !r.completed;
  fs.writeFileSync(r.filePath, serializeReminder(r));
  loadReminders();
  // Clamp selectedIndex to valid range after items shift between groups
  const flat = flatReminders.get();
  if (selectedIndex.peek() >= flat.length) {
    selectedIndex.set(Math.max(0, flat.length - 1));
  }
}

export function deleteReminder(): void {
  const r = selectedReminder.get();
  if (!r) return;
  try {
    fs.unlinkSync(r.filePath);
  } catch {}
  loadReminders();
  const flat = flatReminders.get();
  if (selectedIndex.peek() >= flat.length) {
    selectedIndex.set(Math.max(0, flat.length - 1));
  }
}

export function createReminder(title: string, due: string, priority: Reminder["priority"], tags: string[], body: string): void {
  const dir = dataDir.peek();
  const id = `reminder-${Date.now()}`;
  const r: Reminder = {
    id,
    title,
    body,
    due,
    priority,
    completed: false,
    tags,
    created: new Date().toISOString(),
    filePath: path.join(dir, `${id}.md`),
  };
  fs.writeFileSync(r.filePath, serializeReminder(r));
  loadReminders();
}

// --- Init ---
export function init(): void {
  const dir = dataDir.peek();
  if (!fs.existsSync(dir)) {
    seedData();
  }
  loadReminders();
  startWatching();
}
