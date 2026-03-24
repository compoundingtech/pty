// Unit tests for agent teams demo
import { describe, it, expect, beforeEach, afterEach } from "vitest";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";
import {
  parseStatusFile, parseAgentDir, reloadAgents,
  rootAgent, flatAgents, selectedIndex, selectedAgent,
  activityLog, addActivity, elapsedTime, formatElapsed,
  moveUp, moveDown, togglePause, paused,
  dataDir, stopWatching,
  type AgentNode,
} from "./state.ts";
import { timeline, initDataDir } from "./timeline.ts";
import { dashboardScreen } from "./screens/dashboard.ts";
import { themes, type Theme, type ScreenContext } from "../../src/tui/index.ts";

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

function writeStatusFile(relPath: string, name: string, status: string, task: string, progress: number, body: string): void {
  const fullPath = path.join(testDir, relPath);
  fs.mkdirSync(path.dirname(fullPath), { recursive: true });
  fs.writeFileSync(fullPath, `---
name: ${name}
status: ${status}
task: ${task}
progress: ${progress.toFixed(2)}
started: 2024-03-20T10:00:00Z
updated: 2024-03-20T10:30:00Z
---
${body}
`);
}

beforeEach(() => {
  testDir = fs.mkdtempSync(path.join(os.tmpdir(), "agent-test-"));
  dataDir.set(testDir + "/");
  activityLog.set([]);
  elapsedTime.set(0);
  selectedIndex.set(0);
  paused.set(false);
});

afterEach(() => {
  stopWatching();
  fs.rmSync(testDir, { recursive: true, force: true });
});

// --- parseStatusFile ---
describe("parseStatusFile", () => {
  it("parses a valid status file", () => {
    writeStatusFile("main-agent/status.md", "Main Agent", "working", "Building something", 0.5, "Working on it.");
    const parsed = parseStatusFile(path.join(testDir, "main-agent/status.md"));
    expect(parsed.name).toBe("Main Agent");
    expect(parsed.status).toBe("working");
    expect(parsed.task).toBe("Building something");
    expect(parsed.progress).toBeCloseTo(0.5);
    expect(parsed.body).toBe("Working on it.");
  });

  it("returns defaults for non-existent file", () => {
    const parsed = parseStatusFile(path.join(testDir, "nope.md"));
    expect(parsed.name).toBeUndefined();
  });
});

// --- parseAgentDir ---
describe("parseAgentDir", () => {
  it("parses a single agent", () => {
    writeStatusFile("main-agent/status.md", "Main", "working", "Task", 0.3, "Body");
    const agent = parseAgentDir(path.join(testDir, "main-agent"), 0);
    expect(agent).not.toBeNull();
    expect(agent!.name).toBe("Main");
    expect(agent!.status).toBe("working");
    expect(agent!.children).toHaveLength(0);
  });

  it("parses nested sub-agents", () => {
    writeStatusFile("main-agent/status.md", "Main", "working", "Task", 0.3, "Body");
    writeStatusFile("main-agent/sub-agents/researcher/status.md", "Researcher", "working", "Researching", 0.5, "Scraping");
    writeStatusFile("main-agent/sub-agents/coder/status.md", "Coder", "idle", "Waiting", 0, "Ready");

    const agent = parseAgentDir(path.join(testDir, "main-agent"), 0);
    expect(agent!.children).toHaveLength(2);
    const names = agent!.children.map(c => c.name).sort();
    expect(names).toEqual(["Coder", "Researcher"]);
  });

  it("reads plan and output files", () => {
    writeStatusFile("main-agent/status.md", "Main", "done", "Done", 1.0, "Finished");
    fs.writeFileSync(path.join(testDir, "main-agent/plan.md"), "# Plan\n\n1. Do stuff\n");
    fs.writeFileSync(path.join(testDir, "main-agent/output.md"), "# Output\n\nResults here.\n");

    const agent = parseAgentDir(path.join(testDir, "main-agent"), 0);
    expect(agent!.plan).toContain("Do stuff");
    expect(agent!.output).toContain("Results here");
  });
});

// --- flatAgents ---
describe("flatAgents", () => {
  it("flattens the agent tree", () => {
    writeStatusFile("main-agent/status.md", "Main", "working", "Task", 0.3, "Body");
    writeStatusFile("main-agent/sub-agents/researcher/status.md", "Researcher", "working", "R", 0.5, "S");
    writeStatusFile("main-agent/sub-agents/coder/status.md", "Coder", "idle", "W", 0, "R");
    reloadAgents();

    const list = flatAgents.get();
    expect(list).toHaveLength(3);
    expect(list[0].name).toBe("Main");
  });
});

// --- Timeline events ---
describe("timeline", () => {
  it("first event creates main agent status", () => {
    initDataDir();
    const agent = rootAgent.peek();
    expect(agent).not.toBeNull();
    expect(agent!.name).toBe("Main Agent");
    expect(agent!.status).toBe("working");
  });

  it("initial state has all seeded agents", () => {
    initDataDir();
    const agent = rootAgent.peek()!;
    expect(agent.children.length).toBe(3);
    const names = agent.children.map(c => c.name).sort();
    expect(names).toEqual(["Coder", "Designer", "Researcher"]);
  });

  it("researcher completes at event index 3", () => {
    initDataDir();
    // Fire events 0-3 (researcher completes at 30s)
    for (let i = 0; i <= 3; i++) {
      timeline[i].action();
    }
    reloadAgents();
    const list = flatAgents.get();
    const researcher = list.find(a => a.name === "Researcher");
    expect(researcher).toBeDefined();
    expect(researcher!.status).toBe("done");
    expect(researcher!.progress).toBeCloseTo(1.0);
  });
});

// --- Activity log ---
describe("activityLog", () => {
  it("addActivity appends entries", () => {
    addActivity("Test Agent", "Did something");
    addActivity("Test Agent", "Did another thing");
    expect(activityLog.peek()).toHaveLength(2);
    expect(activityLog.peek()[0].agent).toBe("Test Agent");
    expect(activityLog.peek()[0].event).toBe("Did something");
  });
});

// --- Navigation ---
describe("navigation", () => {
  beforeEach(() => {
    writeStatusFile("main-agent/status.md", "Main", "working", "T", 0.3, "B");
    writeStatusFile("main-agent/sub-agents/researcher/status.md", "R", "working", "T", 0.5, "B");
    writeStatusFile("main-agent/sub-agents/coder/status.md", "C", "idle", "T", 0, "B");
    reloadAgents();
    selectedIndex.set(0);
  });

  it("moveDown increments selectedIndex", () => {
    moveDown();
    expect(selectedIndex.peek()).toBe(1);
  });

  it("moveUp decrements selectedIndex", () => {
    selectedIndex.set(2);
    moveUp();
    expect(selectedIndex.peek()).toBe(1);
  });

  it("togglePause toggles paused state", () => {
    expect(paused.peek()).toBe(false);
    togglePause();
    expect(paused.peek()).toBe(true);
    togglePause();
    expect(paused.peek()).toBe(false);
  });
});

// --- formatElapsed ---
describe("formatElapsed", () => {
  it("formats seconds as m:ss", () => {
    expect(formatElapsed(0)).toBe("0:00");
    expect(formatElapsed(65)).toBe("1:05");
    expect(formatElapsed(600)).toBe("10:00");
  });
});

// --- Dashboard rendering ---
describe("dashboard screen", () => {
  it("renders without crashing", () => {
    initDataDir();
    const ctx = testCtx();
    const buf = dashboardScreen.renderToBuffer(ctx);
    expect(buf.rows).toBe(35);
    expect(buf.cols).toBe(120);
  });

  it("shows agent name in buffer", () => {
    initDataDir();
    const ctx = testCtx();
    const buf = dashboardScreen.renderToBuffer(ctx);
    const allText = buf.cells.map(row => row.map(c => c.char).join("")).join("\n");
    expect(allText).toContain("Main Agent");
  });

  it("shows activity log entries", () => {
    initDataDir();
    const ctx = testCtx();
    const buf = dashboardScreen.renderToBuffer(ctx);
    const allText = buf.cells.map(row => row.map(c => c.char).join("")).join("\n");
    // Activity panel shows the most recent entries (limited by panel height)
    expect(allText).toContain("Started writing tests");
  });
});
