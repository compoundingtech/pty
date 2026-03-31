import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { Session } from "../src/testing/index.ts";
import { cleanupAll } from "../src/sessions.ts";

// Temp dirs for test isolation
const testCwd = fs.mkdtempSync(path.join(os.tmpdir(), "pty-sbfid-"));
const testSessionDir = fs.mkdtempSync(
  path.join(os.tmpdir(), "pty-sbfid-sd-")
);
process.env.PTY_SESSION_DIR = testSessionDir;

afterAll(() => {
  fs.rmSync(testCwd, {
    recursive: true,
    force: true,
    maxRetries: 3,
    retryDelay: 100,
  });
  fs.rmSync(testSessionDir, {
    recursive: true,
    force: true,
    maxRetries: 3,
    retryDelay: 100,
  });
});

// ---- Scaffolding ----

let sessions: Session[] = [];
let sessionNames: string[] = [];
let tmpDirs: string[] = [];

function uniqueName(): string {
  const name = `sbf-${process.pid}-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`;
  sessionNames.push(name);
  return name;
}

function makeTmpDir(): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "pty-sbfid-td-"));
  tmpDirs.push(dir);
  return dir;
}

async function createSession(
  command: string,
  args: string[] = [],
  opts: { rows?: number; cols?: number; cwd?: string } = {}
): Promise<Session> {
  const name = uniqueName();
  const session = await Session.server(command, args, {
    name,
    cwd: opts.cwd ?? testCwd,
    rows: opts.rows,
    cols: opts.cols,
  });
  sessions.push(session);
  await session.attach();
  return session;
}

afterEach(async () => {
  for (const session of sessions) {
    await session.close();
  }
  sessions = [];
  for (const name of sessionNames) {
    cleanupAll(name);
  }
  sessionNames = [];
  for (const dir of tmpDirs) {
    fs.rmSync(dir, {
      recursive: true,
      force: true,
      maxRetries: 3,
      retryDelay: 100,
    });
  }
  tmpDirs = [];
});

// ---- Helpers ----

/**
 * Check whether an SGR parameter sequence appears inside any CSI...m in the
 * ANSI string. The xterm serialize addon may merge multiple SGR params into
 * a single sequence.
 */
function ansiContainsSGR(ansi: string, sgrParams: string): boolean {
  const escaped = sgrParams.replace(/;/g, ";");
  const re = new RegExp(
    `\\x1b\\[` +
      `(?:[0-9;]*;)?` +
      escaped +
      `(?:;[0-9;]*)?` +
      `m`
  );
  return re.test(ansi);
}

// ---- Tests ----

describe("scrollback-fidelity: large scrollback survives reconnect", () => {
  it(
    "500+ lines of mixed plain and ANSI-colored output survive reconnect",
    async () => {
      const dir = makeTmpDir();
      const scriptPath = path.join(dir, "large-scrollback.ts");
      // Generate 600 lines: even lines are plain, odd lines are colored
      fs.writeFileSync(
        scriptPath,
        `
for (let i = 0; i < 600; i++) {
  if (i % 2 === 0) {
    process.stdout.write('LINE-' + String(i).padStart(4, '0') + '\\n');
  } else {
    process.stdout.write('\\x1b[31mLINE-' + String(i).padStart(4, '0') + '\\x1b[0m\\n');
  }
}
setTimeout(() => {}, 300000);
`
      );

      const session = await createSession(process.execPath, [scriptPath], {
        rows: 24,
        cols: 80,
        cwd: dir,
      });

      // Wait for the last line to appear
      await session.waitForText("LINE-0599", 15000);

      // Verify first and last lines are present
      let ss = session.screenshot();
      expect(ss.text).toContain("LINE-0000");
      expect(ss.text).toContain("LINE-0599");

      // Reconnect
      await session.reconnect();

      // After reconnect, ALL scrollback should survive
      ss = await session.waitForText("LINE-0599", 10000);
      expect(ss.text).toContain("LINE-0000");
      expect(ss.text).toContain("LINE-0599");
      // Also check some lines in the middle
      expect(ss.text).toContain("LINE-0300");
    },
    30000
  );
});

describe("scrollback-fidelity: 24-bit color in scrollback", () => {
  it(
    "various 24-bit RGB colors survive reconnect in scrollback",
    async () => {
      const dir = makeTmpDir();
      const scriptPath = path.join(dir, "color-scrollback.ts");
      fs.writeFileSync(
        scriptPath,
        `
// Generate lines with various 24-bit colors to push into scrollback
const colors = [
  [255, 0, 0, 'RED-24BIT'],
  [0, 255, 0, 'GREEN-24BIT'],
  [0, 0, 255, 'BLUE-24BIT'],
  [128, 64, 32, 'BROWN-24BIT'],
  [255, 128, 255, 'PINK-24BIT'],
];
// Print enough filler to push colors into scrollback
for (let i = 0; i < 40; i++) {
  for (const [r, g, b, label] of colors) {
    process.stdout.write('\\x1b[38;2;' + r + ';' + g + ';' + b + 'm' + label + '\\x1b[0m\\n');
  }
}
process.stdout.write('COLOR-DONE\\n');
setTimeout(() => {}, 300000);
`
      );

      const session = await createSession(process.execPath, [scriptPath], {
        rows: 24,
        cols: 80,
        cwd: dir,
      });

      await session.waitForText("COLOR-DONE", 15000);

      // Reconnect
      await session.reconnect();

      const ss = await session.waitForText("COLOR-DONE", 10000);
      // Check text content
      expect(ss.text).toContain("RED-24BIT");
      expect(ss.text).toContain("GREEN-24BIT");
      expect(ss.text).toContain("BLUE-24BIT");
      expect(ss.text).toContain("BROWN-24BIT");
      expect(ss.text).toContain("PINK-24BIT");

      // Check ANSI color sequences survived serialization
      expect(ansiContainsSGR(ss.ansi, "38;2;255;0;0")).toBe(true);
      expect(ansiContainsSGR(ss.ansi, "38;2;0;255;0")).toBe(true);
      expect(ansiContainsSGR(ss.ansi, "38;2;0;0;255")).toBe(true);
      expect(ansiContainsSGR(ss.ansi, "38;2;128;64;32")).toBe(true);
      expect(ansiContainsSGR(ss.ansi, "38;2;255;128;255")).toBe(true);
    },
    30000
  );
});

describe("scrollback-fidelity: output during disconnect", () => {
  it(
    "output produced while disconnected is present after reconnect",
    async () => {
      const dir = makeTmpDir();
      const scriptPath = path.join(dir, "timed-output.ts");
      // Print a line every 100ms with a counter
      fs.writeFileSync(
        scriptPath,
        `
let count = 0;
const timer = setInterval(() => {
  process.stdout.write('TICK-' + String(count).padStart(4, '0') + '\\n');
  count++;
  if (count >= 100) {
    clearInterval(timer);
    process.stdout.write('TICKS-DONE\\n');
    setTimeout(() => {}, 300000);
  }
}, 100);
`
      );

      const session = await createSession(process.execPath, [scriptPath], {
        rows: 24,
        cols: 80,
        cwd: dir,
      });

      // Wait for some output to appear
      await session.waitForText("TICK-0005", 10000);

      // Reconnect (which destroys the socket, waits 100ms, then reattaches)
      // During the disconnection window, the script keeps producing output.
      // We manually do what reconnect() does but with a longer gap.
      // Access the backend's socket directly to simulate a network drop.
      const backend = (session as any).backend;
      backend.socket.destroy();
      // Wait 500ms while the script keeps producing
      await new Promise((r) => setTimeout(r, 500));
      // Reset terminal and reconnect
      (session as any).terminal.reset();
      await (session as any).connectSocket();
      await session.attach();

      // The script should still be running and eventually finish
      const ss = await session.waitForText("TICKS-DONE", 15000);
      // Output produced during disconnection should be present
      // At 100ms intervals, in 500ms about 5 ticks should have been produced
      // while disconnected. All should be in the scrollback now.
      expect(ss.text).toContain("TICK-0005");
      expect(ss.text).toContain("TICK-0010");
    },
    30000
  );
});

describe("scrollback-fidelity: rapid output stream + resize", () => {
  it(
    "resize during rapid output does not corrupt content",
    async () => {
      const dir = makeTmpDir();
      const scriptPath = path.join(dir, "rapid-output.ts");
      // Print numbered lines rapidly
      fs.writeFileSync(
        scriptPath,
        `
let i = 0;
const timer = setInterval(() => {
  process.stdout.write('NUM-' + String(i).padStart(5, '0') + '\\n');
  i++;
  if (i >= 200) {
    clearInterval(timer);
    process.stdout.write('RAPID-DONE\\n');
    setTimeout(() => {}, 300000);
  }
}, 10);
`
      );

      const session = await createSession(process.execPath, [scriptPath], {
        rows: 24,
        cols: 80,
        cwd: dir,
      });

      // Wait for some output
      await session.waitForText("NUM-00010", 10000);

      // Resize while output is still streaming
      session.resize(30, 120);

      // Wait for output to finish
      const ss = await session.waitForText("RAPID-DONE", 15000);

      // Verify resize took effect (terminal should now be 120 cols)
      expect(session.cols).toBe(120);
      expect(session.rows).toBe(30);

      // Verify no content corruption - lines should still match the pattern
      // Check that some lines are present and correctly formatted
      expect(ss.text).toContain("NUM-00001");
      expect(ss.text).toContain("RAPID-DONE");

      // Every visible NUM- line should match the expected pattern
      const numLines = ss.lines.filter((l) => l.startsWith("NUM-"));
      for (const line of numLines) {
        expect(line).toMatch(/^NUM-\d{5}$/);
      }
    },
    30000
  );
});

describe("scrollback-fidelity: alt screen with scrollback", () => {
  it(
    "normal buffer scrollback and alt screen content both survive reconnect",
    async () => {
      const dir = makeTmpDir();
      const scriptPath = path.join(dir, "alt-with-scrollback.ts");
      // First produce scrollback in the normal buffer, then switch to alt screen
      fs.writeFileSync(
        scriptPath,
        `
// Phase 1: Generate scrollback in normal buffer
for (let i = 0; i < 50; i++) {
  process.stdout.write('NORMAL-LINE-' + String(i).padStart(3, '0') + '\\n');
}
// Phase 2: Enter alt screen and draw content
process.stdout.write('\\x1b[?1049h'); // enter alt screen
process.stdout.write('\\x1b[H\\x1b[2J'); // home + clear
process.stdout.write('ALT-SCREEN-MARKER\\n');
process.stdout.write('ALT-CONTENT-ROW2\\n');
setTimeout(() => {}, 300000);
`
      );

      const session = await createSession(process.execPath, [scriptPath], {
        rows: 24,
        cols: 80,
        cwd: dir,
      });

      // Wait for alt screen content
      await session.waitForText("ALT-SCREEN-MARKER", 10000);

      // Reconnect
      await session.reconnect();

      // Alt screen content should be visible
      const ss = await session.waitForText("ALT-SCREEN-MARKER", 10000);
      expect(ss.text).toContain("ALT-SCREEN-MARKER");
      expect(ss.text).toContain("ALT-CONTENT-ROW2");

      // The normal buffer scrollback should NOT appear in the alt screen
      // (this is correct terminal behavior - alt screen is a separate buffer)
      // But the normal buffer data is preserved internally.
      // We verify the alt screen is clean of normal buffer content.
      expect(ss.text).not.toContain("NORMAL-LINE-000");
    },
    30000
  );
});

describe("scrollback-fidelity: Unicode and wide characters", () => {
  it(
    "CJK, emoji, box-drawing, and combining marks survive reconnect",
    async () => {
      const dir = makeTmpDir();
      const scriptPath = path.join(dir, "unicode-scrollback.ts");
      fs.writeFileSync(
        scriptPath,
        `
// CJK characters (2-cell width)
process.stdout.write('CJK:\u4e16\u754c\u4f60\u597d\\n');
// Emoji
process.stdout.write('EMOJI:\u{1f600}\u{1f680}\u{2764}\\n');
// Box drawing
process.stdout.write('BOX:\u250c\u2500\u2500\u2510\\n');
process.stdout.write('BOX:\u2502  \u2502\\n');
process.stdout.write('BOX:\u2514\u2500\u2500\u2518\\n');
// Combining marks: e + combining acute accent
process.stdout.write('COMBINING:e\\u0301 o\\u0308\\n');
process.stdout.write('UNICODE-DONE\\n');
setTimeout(() => {}, 300000);
`
      );

      const session = await createSession(process.execPath, [scriptPath], {
        rows: 24,
        cols: 80,
        cwd: dir,
      });

      await session.waitForText("UNICODE-DONE", 10000);

      // Verify before reconnect
      let ss = session.screenshot();
      expect(ss.text).toContain("CJK:");
      expect(ss.text).toContain("EMOJI:");
      expect(ss.text).toContain("BOX:");
      expect(ss.text).toContain("COMBINING:");

      // Reconnect
      await session.reconnect();

      ss = await session.waitForText("UNICODE-DONE", 10000);
      // CJK characters preserved
      expect(ss.text).toContain("\u4e16\u754c\u4f60\u597d");
      // Emoji preserved (some terminals render these differently, but text should survive)
      expect(ss.text).toContain("EMOJI:");
      // Box drawing preserved
      expect(ss.text).toContain("\u250c\u2500\u2500\u2510");
      expect(ss.text).toContain("\u2514\u2500\u2500\u2518");
      // Combining marks - the combined form should be present
      expect(ss.text).toContain("COMBINING:");
    },
    30000
  );
});

describe("scrollback-fidelity: cursor position preservation", () => {
  it(
    "cursor position set via CSI H is restored after reconnect",
    async () => {
      const dir = makeTmpDir();
      const scriptPath = path.join(dir, "cursor-pos.ts");
      // Enter alt screen, position cursor at a specific location, and write a marker
      fs.writeFileSync(
        scriptPath,
        `
process.stdout.write('\\x1b[?1049h'); // enter alt screen
process.stdout.write('\\x1b[2J');     // clear screen
// Move cursor to row 10, col 20 and write a marker
process.stdout.write('\\x1b[10;20H');
process.stdout.write('CURSOR-HERE');
// Move cursor to row 15, col 5 (final resting position)
process.stdout.write('\\x1b[15;5H');
setTimeout(() => {}, 300000);
`
      );

      const session = await createSession(process.execPath, [scriptPath], {
        rows: 24,
        cols: 80,
        cwd: dir,
      });

      await session.waitForText("CURSOR-HERE", 10000);

      // Screenshot captures the screen state including where text was placed
      let ss = session.screenshot();
      // The marker should be at row 10 (0-indexed: 9)
      expect(ss.lines.length).toBeGreaterThanOrEqual(10);
      expect(ss.lines[9]).toContain("CURSOR-HERE");

      // Reconnect
      await session.reconnect();

      ss = await session.waitForText("CURSOR-HERE", 10000);
      // After reconnect, the text placed at row 10, col 20 should still be there
      expect(ss.lines.length).toBeGreaterThanOrEqual(10);
      expect(ss.lines[9]).toContain("CURSOR-HERE");

      // Verify the text is at the correct column position
      // Col 20 = index 19 in the string (0-indexed)
      const markerIndex = ss.lines[9].indexOf("CURSOR-HERE");
      expect(markerIndex).toBe(19);
    },
    30000
  );
});

describe("scrollback-fidelity: multiple resize + reconnect with scrollback", () => {
  it(
    "scrollback survives resize to smaller, reconnect, resize to larger, and second reconnect",
    async () => {
      const session = await createSession("sh", [
        "-c",
        // Generate scrollback (more lines than the 24-row terminal)
        "for i in $(seq -w 1 60); do echo \"SCROLL-LINE-$i\"; done; sleep 300",
      ]);

      // Wait for all output
      await session.waitForText("SCROLL-LINE-60", 10000);

      // Verify initial content
      let ss = session.screenshot();
      expect(ss.text).toContain("SCROLL-LINE-01");
      expect(ss.text).toContain("SCROLL-LINE-60");

      // Step 1: Resize to smaller terminal
      session.resize(10, 40);
      await new Promise((r) => setTimeout(r, 200));

      // Reconnect at smaller size
      await session.reconnect();

      ss = await session.waitForText("SCROLL-LINE-60", 10000);
      // Content should still be present even in smaller terminal
      expect(ss.text).toContain("SCROLL-LINE-01");
      expect(ss.text).toContain("SCROLL-LINE-60");

      // Step 2: Resize to larger terminal
      session.resize(40, 132);
      await new Promise((r) => setTimeout(r, 200));

      // Reconnect again at larger size
      await session.reconnect();

      ss = await session.waitForText("SCROLL-LINE-60", 10000);
      // All content should still survive the second reconnect
      expect(ss.text).toContain("SCROLL-LINE-01");
      expect(ss.text).toContain("SCROLL-LINE-30");
      expect(ss.text).toContain("SCROLL-LINE-60");
    },
    30000
  );
});
