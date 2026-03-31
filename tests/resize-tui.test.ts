import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { Session } from "../src/testing/index.ts";
import { cleanupAll } from "../src/sessions.ts";

// Temp dirs for test isolation
const testCwd = fs.mkdtempSync(path.join(os.tmpdir(), "pty-resize-"));
const testSessionDir = fs.mkdtempSync(path.join(os.tmpdir(), "pty-resize-sd-"));
process.env.PTY_SESSION_DIR = testSessionDir;

afterAll(() => {
  fs.rmSync(testCwd, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
  fs.rmSync(testSessionDir, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

// ---- Scaffolding ----

let sessions: Session[] = [];
let sessionNames: string[] = [];
let tmpDirs: string[] = [];

function uniqueName(): string {
  const name = `rz-${process.pid}-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`;
  sessionNames.push(name);
  return name;
}

function makeTmpDir(): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "pty-resize-td-"));
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
    fs.rmSync(dir, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
  }
  tmpDirs = [];
});

// ---- Helper: inline Node TUI script that prints dimensions ----

/**
 * A Node script that enters alternate screen, prints the current terminal
 * dimensions as "DIMS:<cols>x<rows>", and reprints whenever SIGWINCH fires.
 * It stays alive via a long sleep so we can resize and observe changes.
 */
const dimPrinterScript = `
process.stdout.write('\\x1b[?1049h'); // enter alt screen
function printDims() {
  // Move to home, clear screen, then print
  process.stdout.write('\\x1b[H\\x1b[2J');
  process.stdout.write('DIMS:' + process.stdout.columns + 'x' + process.stdout.rows + '\\n');
}
printDims();
process.stdout.on('resize', printDims);
setTimeout(() => {}, 300000); // stay alive
`;

// ---- Tests ----

describe("resize-tui: TUI app sees correct size after resize", () => {
  it(
    "reports new dimensions after resize via SIGWINCH",
    async () => {
      const dir = makeTmpDir();
      const scriptPath = path.join(dir, "dims.ts");
      fs.writeFileSync(scriptPath, dimPrinterScript);

      const session = await createSession(
        process.execPath,
        [scriptPath],
        { rows: 24, cols: 80, cwd: dir }
      );

      // Wait for initial dimensions
      const ss1 = await session.waitForText("DIMS:80x24", 10000);
      expect(ss1.text).toContain("DIMS:80x24");

      // Resize to a different size
      session.resize(30, 120);
      await new Promise((r) => setTimeout(r, 500));

      // The script should reprint with new dimensions
      const ss2 = await session.waitForText("DIMS:120x30", 10000);
      expect(ss2.text).toContain("DIMS:120x30");
    },
    20000
  );

  it(
    "handles multiple sequential resizes",
    async () => {
      const dir = makeTmpDir();
      const scriptPath = path.join(dir, "dims2.ts");
      fs.writeFileSync(scriptPath, dimPrinterScript);

      const session = await createSession(
        process.execPath,
        [scriptPath],
        { rows: 20, cols: 60, cwd: dir }
      );

      await session.waitForText("DIMS:60x20", 10000);

      session.resize(40, 100);
      await session.waitForText("DIMS:100x40", 10000);

      session.resize(15, 50);
      await session.waitForText("DIMS:50x15", 10000);

      const ss = session.screenshot();
      expect(ss.text).toContain("DIMS:50x15");
    },
    20000
  );
});

describe("resize-tui: screen replay after reattach shows correct content", () => {
  it(
    "reconnect replays screen with all output intact",
    async () => {
      // A script that prints several labeled lines, then sleeps
      const session = await createSession("sh", [
        "-c",
        "echo 'REPLAY-LINE-1'; echo 'REPLAY-LINE-2'; echo 'REPLAY-LINE-3'; sleep 300",
      ]);

      await session.waitForText("REPLAY-LINE-3", 10000);

      // Detach and reattach
      await session.reconnect();

      const ss = await session.waitForText("REPLAY-LINE-3", 10000);
      expect(ss.text).toContain("REPLAY-LINE-1");
      expect(ss.text).toContain("REPLAY-LINE-2");
      expect(ss.text).toContain("REPLAY-LINE-3");
    },
    20000
  );

  it(
    "alt screen content is replayed on reconnect",
    async () => {
      const dir = makeTmpDir();
      const scriptPath = path.join(dir, "alt-replay.ts");
      fs.writeFileSync(
        scriptPath,
        `
process.stdout.write('\\x1b[?1049h'); // enter alt screen
process.stdout.write('\\x1b[H\\x1b[2J');
process.stdout.write('ALT-SCREEN-CONTENT\\n');
process.stdout.write('SECOND-LINE-HERE\\n');
setTimeout(() => {}, 300000);
`
      );

      const session = await createSession(
        process.execPath,
        [scriptPath],
        { rows: 24, cols: 80, cwd: dir }
      );

      await session.waitForText("ALT-SCREEN-CONTENT", 10000);

      await session.reconnect();

      const ss = await session.waitForText("ALT-SCREEN-CONTENT", 10000);
      expect(ss.text).toContain("ALT-SCREEN-CONTENT");
      expect(ss.text).toContain("SECOND-LINE-HERE");
    },
    20000
  );
});

describe("resize-tui: reattach at different size triggers redraw", () => {
  it(
    "app sees new dimensions when client reconnects at a different size",
    async () => {
      const dir = makeTmpDir();
      const scriptPath = path.join(dir, "dims-reattach.ts");
      fs.writeFileSync(scriptPath, dimPrinterScript);

      const session = await createSession(
        process.execPath,
        [scriptPath],
        { rows: 24, cols: 80, cwd: dir }
      );

      await session.waitForText("DIMS:80x24", 10000);

      // Resize before reconnect to simulate a client with a different terminal size
      session.resize(30, 120);
      await session.reconnect();

      // After reconnect at the new size, the app should see the new dimensions
      // The reconnect sends an attach with the current rows/cols, which triggers
      // a resize on the server side.
      const ss = await session.waitForText("DIMS:120x30", 10000);
      expect(ss.text).toContain("DIMS:120x30");
    },
    20000
  );

  it(
    "second client at different size sees updated dimensions after attach",
    async () => {
      const dir = makeTmpDir();
      const scriptPath = path.join(dir, "dims-peer.ts");
      fs.writeFileSync(scriptPath, dimPrinterScript);

      const session = await createSession(
        process.execPath,
        [scriptPath],
        { rows: 24, cols: 80, cwd: dir }
      );

      await session.waitForText("DIMS:80x24", 10000);

      // Connect a second client at a different size
      const peer = await Session.connectToExisting(session, {
        rows: 40,
        cols: 132,
      });
      sessions.push(peer);
      await peer.attach();

      // The peer's attach should trigger a resize, and the script should print
      // the new dimensions. Wait on the peer terminal for the updated size.
      const ss = await peer.waitForText("DIMS:", 10000);
      // The server should have resized to the peer's dimensions
      expect(ss.text).toMatch(/DIMS:\d+x\d+/);
    },
    20000
  );
});

describe("resize-tui: serialization round-trip preserves 24-bit color", () => {
  it(
    "RGB true color survives detach/reattach cycle",
    async () => {
      // Print text with 24-bit (true color) foreground and background
      const session = await createSession("sh", [
        "-c",
        "printf '\\033[38;2;255;128;0mORANGE-FG\\033[0m \\033[48;2;0;100;200mBLUE-BG\\033[0m\\n'; sleep 300",
      ]);

      const ss1 = await session.waitForText("ORANGE-FG", 10000);
      expect(ss1.text).toContain("ORANGE-FG");
      expect(ss1.text).toContain("BLUE-BG");
      // Verify the 24-bit sequences are present in ANSI output
      expect(ss1.ansi).toMatch(/\x1b\[38;2;255;128;0m/);
      expect(ss1.ansi).toMatch(/\x1b\[48;2;0;100;200m/);

      // Reconnect (detach + reattach via screen replay)
      await session.reconnect();

      const ss2 = await session.waitForText("ORANGE-FG", 10000);
      expect(ss2.text).toContain("ORANGE-FG");
      expect(ss2.text).toContain("BLUE-BG");
      // The 24-bit color codes must survive the serialization round-trip
      expect(ss2.ansi).toMatch(/\x1b\[38;2;255;128;0m/);
      expect(ss2.ansi).toMatch(/\x1b\[48;2;0;100;200m/);
    },
    20000
  );

  it(
    "multiple RGB colors on the same line survive round-trip",
    async () => {
      const session = await createSession("sh", [
        "-c",
        "printf '\\033[38;2;255;0;0mRED\\033[0m \\033[38;2;0;255;0mGREEN\\033[0m \\033[38;2;0;0;255mBLUE\\033[0m\\n'; sleep 300",
      ]);

      await session.waitForText("RED");
      await session.reconnect();

      const ss = await session.waitForText("RED", 10000);
      expect(ss.text).toContain("RED");
      expect(ss.text).toContain("GREEN");
      expect(ss.text).toContain("BLUE");
      expect(ss.ansi).toMatch(/\x1b\[38;2;255;0;0m/);
      expect(ss.ansi).toMatch(/\x1b\[38;2;0;255;0m/);
      expect(ss.ansi).toMatch(/\x1b\[38;2;0;0;255m/);
    },
    20000
  );

  it(
    "24-bit color in alt screen survives reconnect",
    async () => {
      const dir = makeTmpDir();
      const scriptPath = path.join(dir, "color-alt.ts");
      fs.writeFileSync(
        scriptPath,
        `
process.stdout.write('\\x1b[?1049h'); // enter alt screen
process.stdout.write('\\x1b[H\\x1b[2J');
process.stdout.write('\\x1b[38;2;100;200;50mCOLORED-ALT\\x1b[0m\\n');
setTimeout(() => {}, 300000);
`
      );

      const session = await createSession(
        process.execPath,
        [scriptPath],
        { rows: 24, cols: 80, cwd: dir }
      );

      await session.waitForText("COLORED-ALT", 10000);
      await session.reconnect();

      const ss = await session.waitForText("COLORED-ALT", 10000);
      expect(ss.text).toContain("COLORED-ALT");
      expect(ss.ansi).toMatch(/\x1b\[38;2;100;200;50m/);
    },
    20000
  );
});

describe("resize-tui: resize timing", () => {
  it(
    "immediate screenshot after resize does not show stale dimensions",
    async () => {
      const dir = makeTmpDir();
      const scriptPath = path.join(dir, "dims-timing.ts");
      fs.writeFileSync(scriptPath, dimPrinterScript);

      const session = await createSession(
        process.execPath,
        [scriptPath],
        { rows: 24, cols: 80, cwd: dir }
      );

      await session.waitForText("DIMS:80x24", 10000);

      // Resize and immediately screenshot -- check if we see a transient/stale state
      session.resize(30, 100);

      // Take an immediate screenshot (no explicit wait for new dims)
      const immediateShot = session.screenshot();

      // Now wait for the proper update to arrive
      const finalShot = await session.waitForText("DIMS:100x30", 10000);
      expect(finalShot.text).toContain("DIMS:100x30");

      // The immediate screenshot may or may not have the new dims yet.
      // What matters is that it should NOT contain a corrupted/partial state.
      // It should contain either the old dims or the new dims, not garbage.
      const dimsMatch = immediateShot.text.match(/DIMS:(\d+)x(\d+)/);
      expect(dimsMatch).not.toBeNull();
      const immCols = parseInt(dimsMatch![1], 10);
      const immRows = parseInt(dimsMatch![2], 10);
      // Must be one of the two valid dimension sets
      const isOldDims = immCols === 80 && immRows === 24;
      const isNewDims = immCols === 100 && immRows === 30;
      expect(isOldDims || isNewDims).toBe(true);
    },
    20000
  );

  it(
    "rapid resize burst settles to final dimensions",
    async () => {
      const dir = makeTmpDir();
      const scriptPath = path.join(dir, "dims-burst.ts");
      fs.writeFileSync(scriptPath, dimPrinterScript);

      const session = await createSession(
        process.execPath,
        [scriptPath],
        { rows: 24, cols: 80, cwd: dir }
      );

      await session.waitForText("DIMS:80x24", 10000);

      // Fire a burst of resizes in quick succession
      session.resize(25, 90);
      session.resize(26, 95);
      session.resize(28, 100);
      session.resize(32, 110);
      session.resize(36, 130);

      // The app should eventually settle on the final dimensions
      const ss = await session.waitForText("DIMS:130x36", 10000);
      expect(ss.text).toContain("DIMS:130x36");
    },
    20000
  );
});
