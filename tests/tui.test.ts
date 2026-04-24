import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, type ChildProcess } from "node:child_process";
import { Session } from "../src/testing/index.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const serverModule = path.join(__dirname, "..", "dist", "server.js");

// Each test gets its own temp session dir
const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "ptui-"));
afterAll(() => {
  fs.rmSync(testRoot, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

let tuiSessions: Session[] = [];
let bgPids: number[] = [];
let sessionDirs: string[] = [];

function makeSessionDir(): string {
  const dir = fs.mkdtempSync(path.join(testRoot, "d-"));
  sessionDirs.push(dir);
  return dir;
}

afterEach(async () => {
  for (const s of tuiSessions) {
    await s.close();
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

function createTuiSession(sessionDir: string, opts: { rows?: number; cols?: number } = {}): Session {
  const rows = opts.rows ?? 24;
  const cols = opts.cols ?? 80;
  const session = Session.spawn(nodeBin, [cliPath], {
    rows,
    cols,
    env: {
      PTY_SESSION_DIR: sessionDir,
      TERM: "xterm-256color",
    },
  });
  tuiSessions.push(session);
  return session;
}

/**
 * Spawn a background session as a separate daemon process with PTY_SESSION_DIR set.
 * Returns the child process PID for cleanup.
 */
async function createBackgroundSession(
  sessionDir: string,
  name: string,
  command: string,
  args: string[] = [],
  cwd?: string
): Promise<{ child: ChildProcess; pid: number }> {
  const config = JSON.stringify({
    name,
    command,
    args,
    displayCommand: command,
    cwd: cwd ?? os.tmpdir(),
    rows: 24,
    cols: 80,
  });

  const child = spawn(nodeBin, [serverModule], {
    detached: true,
    stdio: ["ignore", "ignore", "pipe"],
    env: {
      ...process.env,
      PTY_SERVER_CONFIG: config,
      PTY_SESSION_DIR: sessionDir,
    },
  });

  let stderr = "";
  child.stderr?.on("data", (d: Buffer) => {
    stderr += d.toString();
  });

  let exitCode: number | null = null;
  child.on("exit", (code) => {
    exitCode = code;
  });

  (child.stderr as any)?.unref?.();
  child.unref();

  // Wait for socket to appear
  const socketPath = path.join(sessionDir, `${name}.sock`);
  const start = Date.now();
  while (Date.now() - start < 5000) {
    if (exitCode !== null) {
      throw new Error(`Background session daemon exited with code ${exitCode}. stderr:\n${stderr}`);
    }
    try {
      fs.statSync(socketPath);
      await new Promise((r) => setTimeout(r, 100));
      return { child, pid: child.pid! };
    } catch {}
    await new Promise((r) => setTimeout(r, 50));
  }
  throw new Error(`Timeout waiting for background session "${name}" socket at ${socketPath}. stderr:\n${stderr}`);
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

        const tui = createTuiSession(sessionDir, { cols, rows: 24 });

        const ss = await tui.waitForText(name, 10000);

        // Find the top border line (contains ╭ and ╮)
        const topLine = ss.lines.find((l) => l.includes("\u256d") && l.includes("\u256e"));
        expect(topLine).toBeDefined();
        // The top border should span most of the terminal width
        const trimmed = topLine!.trim();
        expect(trimmed.length).toBeGreaterThanOrEqual(cols - 2);
        expect(trimmed.length).toBeLessThanOrEqual(cols);

        // Find a session row — it should NOT overflow past the right border
        const sessionLine = ss.lines.find((l) => l.includes(name));
        expect(sessionLine).toBeDefined();
        // The visible content should not exceed terminal width
        expect(sessionLine!.length).toBeLessThanOrEqual(cols);

        // The bottom border should match the top border width
        const bottomLine = ss.lines.find((l) => l.includes("\u2570") && l.includes("\u256f"));
        expect(bottomLine).toBeDefined();
        expect(bottomLine!.trim().length).toBe(trimmed.length);
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

      const tui = createTuiSession(sessionDir, { cols: 120, rows: 24 });

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
      const tui = createTuiSession(sessionDir, { cols: 60, rows: 24 });

      const ss = await tui.waitForText("Create new session...", 10000);

      // Box should still be drawn
      expect(ss.text).toMatch(/[╭╮╰╯│─]/);
      // Content should fit
      const topLine = ss.lines.find((l) => l.includes("\u256d") && l.includes("\u256e"));
      expect(topLine).toBeDefined();
      expect(topLine!.trim().length).toBeGreaterThanOrEqual(58);
      expect(topLine!.trim().length).toBeLessThanOrEqual(60);
    },
    15000
  );
});

describe("interactive TUI", () => {
  it(
    "renders session list with borders and 'Create new session...'",
    async () => {
      const sessionDir = makeSessionDir();
      const tui = createTuiSession(sessionDir);

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

      const tui = createTuiSession(sessionDir);

      const ss = await tui.waitForText(name, 10000);
      expect(ss.text).toContain(name);
      expect(ss.text).toContain("Create new session...");
    },
    15000
  );

  it(
    "--preselect-new pre-selects the Create new session item",
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

      const tui = Session.spawn(nodeBin, [cliPath, "--preselect-new"], {
        rows: 24,
        cols: 80,
        env: { PTY_SESSION_DIR: sessionDir, TERM: "xterm-256color" },
      });
      tuiSessions.push(tui);

      const ss = await tui.waitForText("\u25b8 + Create new session", 10000);
      // Selected marker (▸) should precede the create item, not the session
      expect(ss.text).toMatch(/\u25b8 \+ Create new session/);
      const sessionLine = ss.lines.find((l) => l.includes(name));
      expect(sessionLine).toBeDefined();
      expect(sessionLine!).not.toMatch(/\u25b8 /);
    },
    15000
  );

  it(
    "interactive list renders user tags inline as #key=value next to session name",
    async () => {
      const sessionDir = makeSessionDir();
      const name = uniqueName();

      const { pid } = await createBackgroundSession(sessionDir, name, "sh", ["-c", "sleep 300"], os.tmpdir());
      bgPids.push(pid);

      // Tag the session via the CLI
      spawn(nodeBin, [cliPath, "tag", name, "role=web"], {
        env: { ...process.env, PTY_SESSION_DIR: sessionDir },
        stdio: "ignore",
      }).on("exit", () => {});
      await new Promise((r) => setTimeout(r, 500));

      const tui = createTuiSession(sessionDir);
      const ss = await tui.waitForText("#role=web", 10000);
      const sessionLine = ss.lines.find((l) => l.includes(name));
      expect(sessionLine).toBeDefined();
      // Tag sits between the name and the rest of the detail (cwd/command)
      const nameIdx = sessionLine!.indexOf(name);
      const tagIdx = sessionLine!.indexOf("#role=web");
      expect(tagIdx).toBeGreaterThan(nameIdx);
      // Internal bookkeeping keys should not render as hashtags
      expect(ss.text).not.toContain("#ptyfile=");
    },
    15000
  );

  it(
    "--filter-tag auto-applies the tag to sessions created from the TUI (one-keystroke create)",
    async () => {
      const sessionDir = makeSessionDir();

      const tui = Session.spawn(nodeBin, [cliPath, "--preselect-new", "--filter-tag", "role=web"], {
        rows: 24,
        cols: 80,
        env: { PTY_SESSION_DIR: sessionDir, TERM: "xterm-256color" },
      });
      tuiSessions.push(tui);

      // Create item is preselected; a single Enter now spawns the shell
      // directly — no prompts.
      await tui.waitForText("#role=web", 10000);
      tui.press("return");

      // Wait for the session metadata file to appear with the tag applied.
      const start = Date.now();
      let found: any = null;
      while (Date.now() - start < 5000) {
        const entries = fs.readdirSync(sessionDir).filter((f) => f.endsWith(".json"));
        for (const e of entries) {
          try {
            const meta = JSON.parse(fs.readFileSync(path.join(sessionDir, e), "utf-8"));
            if (meta.tags?.role === "web") { found = meta; break; }
          } catch {}
        }
        if (found) break;
        await new Promise((r) => setTimeout(r, 100));
      }
      expect(found).not.toBeNull();
      expect(found.tags.role).toBe("web");
      // No displayName should be set — the "new anonymous" flow matches
      // pty run --no-display-name.
      expect(found.displayName).toBeUndefined();

      const pidFiles = fs.readdirSync(sessionDir).filter((f) => f.endsWith(".pid"));
      for (const p of pidFiles) {
        try {
          const pid = parseInt(fs.readFileSync(path.join(sessionDir, p), "utf-8").trim(), 10);
          bgPids.push(pid);
        } catch {}
      }
    },
    20000
  );

  it(
    "--filter-tag hides sessions that lack the tag",
    async () => {
      const sessionDir = makeSessionDir();
      const matchName = uniqueName();
      const otherName = uniqueName();

      const { pid: p1 } = await createBackgroundSession(sessionDir, matchName, "sh", ["-c", "sleep 300"], os.tmpdir());
      bgPids.push(p1);
      const { pid: p2 } = await createBackgroundSession(sessionDir, otherName, "sh", ["-c", "sleep 300"], os.tmpdir());
      bgPids.push(p2);

      // Tag one of them — use the CLI so the metadata write goes through the
      // same code path as a real user.
      spawn(nodeBin, [cliPath, "tag", matchName, "layout=work"], {
        env: { ...process.env, PTY_SESSION_DIR: sessionDir },
        stdio: "ignore",
      }).on("exit", () => {});
      await new Promise((r) => setTimeout(r, 500));

      const tui = Session.spawn(nodeBin, [cliPath, "--filter-tag", "layout=work"], {
        rows: 24,
        cols: 80,
        env: { PTY_SESSION_DIR: sessionDir, TERM: "xterm-256color" },
      });
      tuiSessions.push(tui);

      const ss = await tui.waitForText(matchName, 10000);
      expect(ss.text).toContain(matchName);
      expect(ss.text).not.toContain(otherName);
      // Tag filter is shown in the Filter line
      expect(ss.text).toContain("#layout=work");
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

      const tui = createTuiSession(sessionDir);

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
    "ctrl+u clears the filter in one keystroke",
    async () => {
      const sessionDir = makeSessionDir();
      const name1 = uniqueName();
      const name2 = uniqueName();

      const bg1 = await createBackgroundSession(sessionDir, name1, "sh", ["-c", "sleep 300"], os.tmpdir());
      bgPids.push(bg1.pid);
      const bg2 = await createBackgroundSession(sessionDir, name2, "sh", ["-c", "sleep 300"], os.tmpdir());
      bgPids.push(bg2.pid);

      const tui = createTuiSession(sessionDir);
      await tui.waitForText(name1, 10000);
      await tui.waitForText(name2, 10000);

      // Type part of name1 to filter. Wait for name2 to disappear so we're
      // sure the filter has rendered before we measure the clear.
      tui.type(name1.slice(-4));
      await tui.waitForAbsent(name2, 5000);

      // ctrl+u clears the whole filter.
      tui.press("ctrl+u");
      await tui.waitForText(name2, 5000);

      const ss = tui.screenshot();
      expect(ss.text).toContain(name1);
      expect(ss.text).toContain(name2);
    },
    20000,
  );

  it(
    "ctrl+w deletes the previous word from the filter",
    async () => {
      const sessionDir = makeSessionDir();
      const name1 = uniqueName();

      const bg1 = await createBackgroundSession(sessionDir, name1, "sh", ["-c", "sleep 300"], os.tmpdir());
      bgPids.push(bg1.pid);

      const tui = createTuiSession(sessionDir);
      await tui.waitForText(name1, 10000);

      // Type "foo bar" — two words. ctrl+w should remove "bar" and leave
      // "foo " in the filter line. The underlying fuzzy matcher probably
      // won't match a session name, so we check the rendered filter line
      // directly.
      tui.type("foo bar");
      // Wait for both words to appear in the "Filter: ..." line.
      await tui.waitForText("foo bar", 5000);

      tui.press("ctrl+w");
      // After ctrl+w, "bar" should be gone from the filter line; the filter
      // should still contain "foo ".
      await tui.waitForAbsent("foo bar", 5000);
      const ss = tui.screenshot();
      expect(ss.text).toContain("Filter: foo");
      expect(ss.text).not.toContain("foo bar");
    },
    20000,
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

      const tui = createTuiSession(sessionDir);

      await tui.waitForText(name1, 10000);
      await tui.waitForText(name2, 10000);

      // Type part of name1 to filter — wait for name2 to disappear,
      // which proves the filter rendered (not just that the input updated)
      const filterText = name1.slice(-4);
      tui.type(filterText);
      await tui.waitForAbsent(name2, 5000);

      let ss = tui.screenshot();
      expect(ss.text).toContain(name1);
      // "Create new session..." should be hidden when filter doesn't match "new"
      expect(ss.text).not.toContain("Create new session...");

      // Backspace to remove filter
      for (let i = 0; i < filterText.length; i++) {
        tui.press("backspace");
      }
      await tui.waitForText(name2, 5000);

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

      const tui = createTuiSession(sessionDir);

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

      const tui = createTuiSession(sessionDir);

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
    "Enter on 'Create new session...' spawns a shell with a random id and no displayName",
    async () => {
      const sessionDir = makeSessionDir();
      const tui = createTuiSession(sessionDir);

      await tui.waitForText("Create new session...", 10000);
      tui.press("return");

      // Wait for a session metadata file to materialize; verify it has
      // no displayName and a random-looking name.
      const start = Date.now();
      let meta: any = null;
      while (Date.now() - start < 5000) {
        const entries = fs.readdirSync(sessionDir).filter((f) => f.endsWith(".json"));
        if (entries.length > 0) {
          try { meta = JSON.parse(fs.readFileSync(path.join(sessionDir, entries[0]), "utf-8")); } catch {}
          if (meta) break;
        }
        await new Promise((r) => setTimeout(r, 100));
      }
      expect(meta).not.toBeNull();
      expect(meta.displayName).toBeUndefined();

      // Record the spawned session pid for cleanup
      const pidFiles = fs.readdirSync(sessionDir).filter((f) => f.endsWith(".pid"));
      for (const p of pidFiles) {
        try {
          const pid = parseInt(fs.readFileSync(path.join(sessionDir, p), "utf-8").trim(), 10);
          bgPids.push(pid);
        } catch {}
      }
    },
    20000
  );

  it(
    "empty state shows only 'Create new session...'",
    async () => {
      const sessionDir = makeSessionDir();
      const tui = createTuiSession(sessionDir);

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
      const tui = createTuiSession(sessionDir);

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
      const tui = createTuiSession(sessionDir);

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

      const tui = createTuiSession(sessionDir);

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

      const tui = createTuiSession(sessionDir);

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

  it(
    "after session exits and returning to list, keystrokes are not doubled",
    async () => {
      const sessionDir = makeSessionDir();
      const name = uniqueName();

      // A session that exits immediately after printing a marker
      const { pid } = await createBackgroundSession(
        sessionDir,
        name,
        "sh",
        ["-c", "echo WILL_EXIT; exec cat"],
        os.tmpdir()
      );
      bgPids.push(pid);

      const tui = createTuiSession(sessionDir);

      // Attach
      await tui.waitForText(name, 10000);
      tui.press("return");
      await tui.waitForText("WILL_EXIT", 10000);

      // Make the session exit (Ctrl+D sends EOF to cat)
      tui.sendKeys("\x04");

      // Wait for return to list
      await tui.waitForText("Create new session...", 10000);

      // Type a filter string and verify each character appears exactly once.
      // If stdin has duplicate listeners, each keystroke fires twice and
      // the filter will contain doubled characters (e.g. "xxyz" instead of "xyz").
      tui.type("x");
      await new Promise(r => setTimeout(r, 300));
      tui.type("y");
      await new Promise(r => setTimeout(r, 300));
      tui.type("z");
      await new Promise(r => setTimeout(r, 300));

      const ss = tui.screenshot();
      expect(ss.text).toContain("xyz");
      expect(ss.text).not.toContain("xxyyzz");
    },
    20000
  );

  it(
    "exited session shows as exited when returning to list during cleanup window",
    async () => {
      const sessionDir = makeSessionDir();
      const name = uniqueName();

      // Use a command that prints a marker then waits for input
      const { pid } = await createBackgroundSession(
        sessionDir,
        name,
        "sh",
        ["-c", "echo EXIT_RACE; exec cat"],
        os.tmpdir()
      );
      bgPids.push(pid);

      const tui = createTuiSession(sessionDir);

      // Attach to the session
      await tui.waitForText(name, 10000);
      tui.press("return");
      await tui.waitForText("EXIT_RACE", 10000);

      // Send EOF to make cat exit — triggers the race where the daemon
      // is still alive (500ms cleanup delay) but metadata has exitedAt set
      tui.sendKeys("\x04"); // Ctrl+D

      // Wait for TUI to return to the list and show the session as exited.
      // There's a brief window after Ctrl+D where the daemon is still alive
      // but exitedAt has been written — the TUI needs to refresh to pick it up.
      await tui.waitForText("Create new session...", 10000);
      await tui.waitForText("exited", 5000);

      const ss = tui.screenshot();
      expect(ss.text).toContain(name);
    },
    20000
  );
});
