import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { TuiSession, createBackgroundSession } from "./tui-harness.ts";

// Each test gets its own temp session dir
const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "ptui-"));
afterAll(() => {
  fs.rmSync(testRoot, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

let tuiSessions: TuiSession[] = [];
let bgPids: number[] = [];
let sessionDirs: string[] = [];

function makeSessionDir(): string {
  const dir = fs.mkdtempSync(path.join(testRoot, "d-"));
  sessionDirs.push(dir);
  return dir;
}

afterEach(async () => {
  for (const s of tuiSessions) {
    s.close();
  }
  tuiSessions = [];
  for (const pid of bgPids) {
    try {
      process.kill(pid, "SIGTERM");
    } catch {}
  }
  bgPids = [];
  // Clean up session dir contents
  for (const dir of sessionDirs) {
    try {
      const entries = fs.readdirSync(dir);
      for (const e of entries) {
        try { fs.unlinkSync(path.join(dir, e)); } catch {}
      }
    } catch {}
  }
  sessionDirs = [];
});

let nameCounter = 0;
function uniqueName(): string {
  // Keep names short to avoid EINVAL on Unix domain sockets (104-byte path limit on macOS)
  return `s${++nameCounter}-${Math.random().toString(36).slice(2, 6)}`;
}

describe("interactive TUI layout", () => {
  for (const cols of [80, 120, 200]) {
    it(
      `box fills width at ${cols} columns`,
      async () => {
        const sessionDir = makeSessionDir();
        const name = uniqueName();

        const { pid } = await createBackgroundSession(
          sessionDir,
          name,
          "sh",
          ["-c", "sleep 300"],
          os.tmpdir()
        );
        bgPids.push(pid);

        const tui = TuiSession.create({ sessionDir, cols, rows: 24 });
        tuiSessions.push(tui);

        const ss = await tui.waitForText(name, 10000);

        // Find the top border line (contains ╭ and ╮)
        const topLine = ss.lines.find((l) => l.includes("\u256d") && l.includes("\u256e"));
        expect(topLine).toBeDefined();
        // The top border should span nearly the full width (cols - 2 margin)
        const expectedWidth = cols - 2;
        // Count visible characters: ╭ + ─...─ + ╮ should equal expectedWidth
        const trimmed = topLine!.trim();
        expect(trimmed.length).toBe(expectedWidth);

        // Find a session row — it should NOT overflow past the right border
        const sessionLine = ss.lines.find((l) => l.includes(name));
        expect(sessionLine).toBeDefined();
        // The visible content should not exceed terminal width
        expect(sessionLine!.length).toBeLessThanOrEqual(cols);

        // The bottom border should match the top border width
        const bottomLine = ss.lines.find((l) => l.includes("\u2570") && l.includes("\u256f"));
        expect(bottomLine).toBeDefined();
        expect(bottomLine!.trim().length).toBe(expectedWidth);
      },
      15000
    );
  }

  it(
    "paths are not truncated when space is available at 120 cols",
    async () => {
      const sessionDir = makeSessionDir();
      const name = uniqueName();

      const { pid } = await createBackgroundSession(
        sessionDir,
        name,
        "sh",
        ["-c", "sleep 300"],
        // Use a path that's longish but fits in 120 cols
        os.tmpdir()
      );
      bgPids.push(pid);

      const tui = TuiSession.create({ sessionDir, cols: 120, rows: 24 });
      tuiSessions.push(tui);

      const ss = await tui.waitForText(name, 10000);
      const sessionLine = ss.lines.find((l) => l.includes(name));
      expect(sessionLine).toBeDefined();

      // The tmp dir path should NOT be truncated (no ellipsis) at 120 cols
      // os.tmpdir() is typically short like /tmp or /var/folders/...
      // At 120 cols there's plenty of room
      const tmpShort = os.tmpdir().startsWith(os.homedir())
        ? "~" + os.tmpdir().slice(os.homedir().length)
        : os.tmpdir();
      // If the path is short enough it should appear fully
      if (tmpShort.length < 50) {
        expect(sessionLine).toContain(tmpShort);
      }
    },
    15000
  );

  it(
    "session list renders correctly at narrow 60 columns",
    async () => {
      const sessionDir = makeSessionDir();
      const tui = TuiSession.create({ sessionDir, cols: 60, rows: 24 });
      tuiSessions.push(tui);

      const ss = await tui.waitForText("Create new session...", 10000);

      // Box should still be drawn
      expect(ss.text).toMatch(/[╭╮╰╯│─]/);
      // Content should fit
      const topLine = ss.lines.find((l) => l.includes("\u256d") && l.includes("\u256e"));
      expect(topLine).toBeDefined();
      expect(topLine!.trim().length).toBe(58); // 60 - 2 margin
    },
    15000
  );
});

describe("interactive TUI", () => {
  it(
    "renders session list with borders and 'Create new session...'",
    async () => {
      const sessionDir = makeSessionDir();
      const tui = TuiSession.create({ sessionDir });
      tuiSessions.push(tui);

      const ss = await tui.waitForText("Create new session...", 10000);
      expect(ss.text).toContain("pty");
      expect(ss.text).toContain("Create new session...");
      // Should have rounded box-drawing characters
      expect(ss.text).toMatch(/[╭╮╰╯│─]/);
    },
    15000
  );

  it(
    "shows active sessions in the list",
    async () => {
      const sessionDir = makeSessionDir();
      const name = uniqueName();

      const { pid } = await createBackgroundSession(
        sessionDir,
        name,
        "sh",
        ["-c", "sleep 300"],
        os.tmpdir()
      );
      bgPids.push(pid);

      const tui = TuiSession.create({ sessionDir });
      tuiSessions.push(tui);

      const ss = await tui.waitForText(name, 10000);
      expect(ss.text).toContain(name);
      expect(ss.text).toContain("Create new session...");
    },
    15000
  );

  it(
    "arrow keys move selection",
    async () => {
      const sessionDir = makeSessionDir();
      const name = uniqueName();

      const { pid } = await createBackgroundSession(
        sessionDir,
        name,
        "sh",
        ["-c", "sleep 300"],
        os.tmpdir()
      );
      bgPids.push(pid);

      const tui = TuiSession.create({ sessionDir });
      tuiSessions.push(tui);

      await tui.waitForText(name, 10000);

      // Press down then up
      tui.press("down");
      await new Promise((r) => setTimeout(r, 200));
      tui.press("up");
      await new Promise((r) => setTimeout(r, 200));

      const ss = tui.screenshot();
      expect(ss.text).toContain(name);
      expect(ss.text).toContain("Create new session...");
    },
    15000
  );

  it(
    "typing filters the list, backspace unfilters",
    async () => {
      const sessionDir = makeSessionDir();
      const name1 = uniqueName();
      const name2 = uniqueName();

      const bg1 = await createBackgroundSession(sessionDir, name1, "sh", ["-c", "sleep 300"], os.tmpdir());
      bgPids.push(bg1.pid);
      const bg2 = await createBackgroundSession(sessionDir, name2, "sh", ["-c", "sleep 300"], os.tmpdir());
      bgPids.push(bg2.pid);

      const tui = TuiSession.create({ sessionDir });
      tuiSessions.push(tui);

      await tui.waitForText(name1, 10000);
      await tui.waitForText(name2, 10000);

      // Type part of name1 to filter
      const filterText = name1.slice(-4);
      tui.type(filterText);
      await new Promise((r) => setTimeout(r, 300));

      let ss = tui.screenshot();
      expect(ss.text).toContain(name1);
      expect(ss.text).toContain("Create new session...");

      // Backspace to remove filter
      for (let i = 0; i < filterText.length; i++) {
        tui.press("backspace");
      }
      await new Promise((r) => setTimeout(r, 300));

      ss = tui.screenshot();
      expect(ss.text).toContain(name1);
      expect(ss.text).toContain(name2);
    },
    20000
  );

  it(
    "enter on session triggers attach",
    async () => {
      const sessionDir = makeSessionDir();
      const name = uniqueName();

      const { pid } = await createBackgroundSession(
        sessionDir,
        name,
        "sh",
        ["-c", "echo 'HELLO_FROM_SESSION'; sleep 300"],
        os.tmpdir()
      );
      bgPids.push(pid);

      const tui = TuiSession.create({ sessionDir });
      tuiSessions.push(tui);

      await tui.waitForText(name, 10000);
      tui.press("return");

      const ss = await tui.waitForText("HELLO_FROM_SESSION", 10000);
      expect(ss.text).toContain("HELLO_FROM_SESSION");
    },
    15000
  );

  it(
    "Ctrl+\\ detaches and returns to session list",
    async () => {
      const sessionDir = makeSessionDir();
      const name = uniqueName();

      const { pid } = await createBackgroundSession(
        sessionDir,
        name,
        "sh",
        ["-c", "echo 'IN_SESSION'; sleep 300"],
        os.tmpdir()
      );
      bgPids.push(pid);

      const tui = TuiSession.create({ sessionDir });
      tuiSessions.push(tui);

      await tui.waitForText(name, 10000);
      tui.press("return");
      await tui.waitForText("IN_SESSION", 10000);

      // Detach with Ctrl+backslash
      tui.sendKeys("\x1c");

      const ss = await tui.waitForText("Create new session...", 10000);
      expect(ss.text).toContain(name);
    },
    20000
  );

  it(
    "create wizard: shows directory picker",
    async () => {
      const sessionDir = makeSessionDir();
      const tui = TuiSession.create({ sessionDir });
      tuiSessions.push(tui);

      await tui.waitForText("Create new session...", 10000);
      tui.press("return");

      const ss = await tui.waitForText("Choose Directory", 5000);
      expect(ss.text).toContain("current directory");
    },
    15000
  );

  it(
    "create wizard: name auto-fills from directory",
    async () => {
      const sessionDir = makeSessionDir();
      const tui = TuiSession.create({ sessionDir });
      tuiSessions.push(tui);

      await tui.waitForText("Create new session...", 10000);
      tui.press("return");
      await tui.waitForText("Choose Directory", 5000);
      tui.press("return");

      const ss = await tui.waitForText("Name:", 5000);
      expect(ss.text).toContain("Command:");
    },
    15000
  );

  it(
    "empty state shows only 'Create new session...'",
    async () => {
      const sessionDir = makeSessionDir();
      const tui = TuiSession.create({ sessionDir });
      tuiSessions.push(tui);

      const ss = await tui.waitForText("Create new session...", 10000);
      expect(ss.text).toContain("Create new session...");
      expect(ss.text).toContain("select");
    },
    15000
  );

  it(
    "q quits the interactive TUI",
    async () => {
      const sessionDir = makeSessionDir();
      const tui = TuiSession.create({ sessionDir });
      tuiSessions.push(tui);

      await tui.waitForText("Create new session...", 10000);
      tui.type("q");
      await new Promise((r) => setTimeout(r, 500));

      const ss = tui.screenshot();
      expect(ss.text).not.toContain("Create new session...");
    },
    15000
  );

  it(
    "escape clears filter, then quits",
    async () => {
      const sessionDir = makeSessionDir();
      const tui = TuiSession.create({ sessionDir });
      tuiSessions.push(tui);

      await tui.waitForText("Create new session...", 10000);

      tui.type("xyz");
      await new Promise((r) => setTimeout(r, 200));

      let ss = tui.screenshot();
      expect(ss.text).toContain("xyz");

      // Escape clears filter
      tui.press("escape");
      await new Promise((r) => setTimeout(r, 200));

      ss = tui.screenshot();
      expect(ss.text).toContain("Create new session...");

      // Escape again quits
      tui.press("escape");
      await new Promise((r) => setTimeout(r, 500));

      ss = tui.screenshot();
      expect(ss.text).not.toContain("Create new session...");
    },
    15000
  );

  it(
    "multiple attach/detach cycles work without breaking input",
    async () => {
      const sessionDir = makeSessionDir();
      const name = uniqueName();

      const { pid } = await createBackgroundSession(
        sessionDir,
        name,
        "sh",
        ["-c", "echo 'CYCLE_TEST'; sleep 300"],
        os.tmpdir()
      );
      bgPids.push(pid);

      const tui = TuiSession.create({ sessionDir });
      tuiSessions.push(tui);

      // Cycle 1: attach and detach
      await tui.waitForText(name, 10000);
      tui.press("return");
      await tui.waitForText("CYCLE_TEST", 10000);
      tui.sendKeys("\x1c");
      await tui.waitForText("Create new session...", 10000);

      // Cycle 2: attach and detach again — verifies listeners are cleaned up
      tui.press("return");
      await tui.waitForText("CYCLE_TEST", 10000);
      tui.sendKeys("\x1c");
      await tui.waitForText("Create new session...", 10000);

      // Cycle 3: verify TUI still responds to input
      tui.type("q");
      await new Promise((r) => setTimeout(r, 500));
      const ss = tui.screenshot();
      expect(ss.text).not.toContain("Create new session...");
    },
    30000
  );

  it(
    "session list reloads after returning from attach",
    async () => {
      const sessionDir = makeSessionDir();
      const name = uniqueName();

      const { pid } = await createBackgroundSession(
        sessionDir,
        name,
        "sh",
        ["-c", "echo 'RELOAD_TEST'; sleep 300"],
        os.tmpdir()
      );
      bgPids.push(pid);

      const tui = TuiSession.create({ sessionDir });
      tuiSessions.push(tui);

      await tui.waitForText(name, 10000);

      // Attach then detach
      tui.press("return");
      await tui.waitForText("RELOAD_TEST", 10000);
      tui.sendKeys("\x1c");

      // After returning, session should still be in the list
      const ss = await tui.waitForText(name, 10000);
      expect(ss.text).toContain(name);
      expect(ss.text).toContain("Create new session...");
    },
    20000
  );
});
