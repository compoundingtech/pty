// Unit tests for reminders demo
import { describe, it, expect, beforeEach, afterEach } from "vitest";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";
import {
  parseFrontmatter, parseReminderFile, serializeReminder, seedData,
  loadReminders, reminders, groupedByDate, boardColumns, calendarData,
  selectedIndex, selectedReminder, currentView, cycleView,
  toggleComplete, deleteReminder, createReminder,
  dataDir, init, stopWatching, calendarMonth, calendarYear,
  type Reminder,
} from "./state.ts";
import { themes, type Theme, type ScreenContext } from "../../src/tui/index.ts";
import { renderListView } from "./screens/list-view.ts";

let testDir: string;

function testCtx(rows = 35, cols = 120): ScreenContext {
  const theme = themes.coolBlue as Theme;
  return {
    rows, cols, theme, boxStyle: "rounded",
    navigate: () => {}, back: () => {},
    openOverlay: () => {}, closeOverlay: () => {},
    isTextInputActive: () => false, setTextInputActive: () => {},
  };
}

beforeEach(() => {
  testDir = fs.mkdtempSync(path.join(os.tmpdir(), "rem-test-"));
  dataDir.set(testDir + "/");
  seedData();
  loadReminders();
});

afterEach(() => {
  stopWatching();
  fs.rmSync(testDir, { recursive: true, force: true });
});

// --- Frontmatter parsing ---
describe("parseFrontmatter", () => {
  it("parses valid frontmatter", () => {
    const content = `---
due: 2024-03-25
priority: high
completed: false
tags: [work, urgent]
created: 2024-03-20T10:00:00Z
---
# Buy groceries

Get milk, eggs, bread.`;
    const { meta, body } = parseFrontmatter(content);
    expect(meta.due).toBe("2024-03-25");
    expect(meta.priority).toBe("high");
    expect(meta.completed).toBe("false");
    expect(meta.tags).toBe("[work, urgent]");
    expect(body).toContain("Buy groceries");
    expect(body).toContain("Get milk");
  });

  it("returns empty meta for no frontmatter", () => {
    const { meta, body } = parseFrontmatter("Hello world");
    expect(Object.keys(meta)).toHaveLength(0);
    expect(body).toBe("Hello world");
  });

  it("handles unclosed frontmatter", () => {
    const { meta, body } = parseFrontmatter("---\nkey: value\nno closing");
    expect(Object.keys(meta)).toHaveLength(0);
  });
});

// --- Seed data ---
describe("seedData", () => {
  it("creates reminder files on disk", () => {
    const files = fs.readdirSync(testDir).filter(f => f.endsWith(".md"));
    expect(files.length).toBe(15);
  });

  it("creates parseable files", () => {
    const files = fs.readdirSync(testDir).filter(f => f.endsWith(".md"));
    const r = parseReminderFile(path.join(testDir, files[0]));
    expect(r).not.toBeNull();
    expect(r!.title.length).toBeGreaterThan(0);
  });
});

// --- Load reminders ---
describe("loadReminders", () => {
  it("loads all seeded reminders", () => {
    expect(reminders.peek().length).toBe(15);
  });

  it("parses reminder fields correctly", () => {
    const all = reminders.peek();
    const buy = all.find(r => r.title === "Buy groceries");
    expect(buy).toBeDefined();
    expect(buy!.priority).toBe("high");
    expect(buy!.tags).toContain("personal");
  });
});

// --- Grouping ---
describe("groupedByDate", () => {
  it("groups reminders into date categories", () => {
    const groups = groupedByDate.get();
    expect(groups.length).toBeGreaterThan(0);
    // Should have at least Overdue, Today, and Completed groups
    const titles = groups.map(g => g.title);
    expect(titles).toContain("Completed");
    expect(titles).toContain("Today");
  });

  it("completed group has 3 items", () => {
    const groups = groupedByDate.get();
    const completed = groups.find(g => g.title === "Completed");
    expect(completed).toBeDefined();
    expect(completed!.items.length).toBe(3);
  });
});

// --- Board columns ---
describe("boardColumns", () => {
  it("splits into todo, in-progress, done", () => {
    const [todo, inProgress, done] = boardColumns.get();
    expect(done.length).toBe(3); // 3 completed
    expect(inProgress.length).toBeGreaterThan(0); // high priority items
    expect(todo.length + inProgress.length).toBe(12); // 15 - 3 completed
  });
});

// --- Calendar data ---
describe("calendarData", () => {
  it("returns counts for days with reminders", () => {
    const month = new Date().getMonth();
    const year = new Date().getFullYear();
    calendarMonth.set(month);
    calendarYear.set(year);
    const counts = calendarData.get();
    // Today should have reminders (we seeded some for today)
    const today = new Date().getDate();
    expect(counts.get(today)).toBeGreaterThan(0);
  });
});

// --- View cycling ---
describe("cycleView", () => {
  it("cycles through list, board, calendar", () => {
    expect(currentView.peek()).toBe("list");
    cycleView();
    expect(currentView.peek()).toBe("board");
    cycleView();
    expect(currentView.peek()).toBe("calendar");
    cycleView();
    expect(currentView.peek()).toBe("list");
  });
});

// --- Toggle complete ---
describe("toggleComplete", () => {
  it("toggles the completed flag and rewrites file", () => {
    selectedIndex.set(0);
    const r = selectedReminder.get()!;
    const wasDone = r.completed;
    toggleComplete();
    loadReminders();
    const updated = reminders.peek().find(x => x.id === r.id);
    expect(updated!.completed).toBe(!wasDone);
  });
});

// --- Delete reminder ---
describe("deleteReminder", () => {
  it("removes the reminder file", () => {
    const before = reminders.peek().length;
    selectedIndex.set(0);
    const r = selectedReminder.get()!;
    deleteReminder();
    expect(reminders.peek().length).toBe(before - 1);
    expect(fs.existsSync(r.filePath)).toBe(false);
  });
});

// --- Create reminder ---
describe("createReminder", () => {
  it("creates a new reminder file", () => {
    const before = reminders.peek().length;
    createReminder("Test reminder", "2024-12-25", "low", ["test"], "Body text");
    expect(reminders.peek().length).toBe(before + 1);
    const r = reminders.peek().find(x => x.title === "Test reminder");
    expect(r).toBeDefined();
    expect(r!.priority).toBe("low");
  });
});

// --- Serialize ---
describe("serializeReminder", () => {
  it("produces valid markdown with frontmatter", () => {
    const r: Reminder = {
      id: "test", title: "Test", body: "Body",
      due: "2024-03-25", priority: "high", completed: false,
      tags: ["a", "b"], created: "2024-03-20T10:00:00Z",
      filePath: "/tmp/test.md",
    };
    const md = serializeReminder(r);
    expect(md).toContain("---");
    expect(md).toContain("due: 2024-03-25");
    expect(md).toContain("priority: high");
    expect(md).toContain("# Test");
    expect(md).toContain("Body");
    expect(md).toContain("tags: [a, b]");
  });
});

// --- List view rendering ---
describe("list view", () => {
  it("renders without crashing", () => {
    const ctx = testCtx();
    const nodes = renderListView(ctx);
    expect(nodes.length).toBeGreaterThan(0);
  });
});
