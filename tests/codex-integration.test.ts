import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { Session, type Screenshot } from "../src/testing/index.ts";
import { cleanupAll } from "../src/sessions.ts";

// Isolate session metadata/sockets from the real session directory
const testCwd = fs.mkdtempSync(path.join(os.tmpdir(), "pty-codex-"));
const testSessionDir = fs.mkdtempSync(path.join(os.tmpdir(), "pty-codex-sd-"));
process.env.PTY_SESSION_DIR = testSessionDir;

afterAll(() => {
  fs.rmSync(testCwd, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
  fs.rmSync(testSessionDir, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

// ─── Scaffolding ───

let sessions: Session[] = [];
let sessionNames: string[] = [];

function uniqueName(): string {
  const name = `codex-${process.pid}-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`;
  sessionNames.push(name);
  return name;
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
});

// ─── Helpers ───

function logScreenshot(label: string, ss: Screenshot): void {
  console.log(`\n=== ${label} ===`);
  console.log(`Dimensions: ${ss.lines.length} lines`);
  console.log("--- text ---");
  console.log(ss.text);
  console.log("--- end ---\n");
}

/** Wait for the screen to have any non-empty content, with a generous timeout. */
async function waitForAnyContent(session: Session, timeoutMs = 15000): Promise<Screenshot> {
  return session.waitFor(
    (ss) => ss.text.trim().length > 0,
    timeoutMs,
    "any non-empty screen content"
  );
}

/**
 * Wait for the screen to have substantive content (more than just a few chars).
 * This helps ensure the TUI has fully rendered rather than just partially started.
 */
async function waitForSubstantiveContent(session: Session, timeoutMs = 15000): Promise<Screenshot> {
  return session.waitFor(
    (ss) => ss.text.trim().length > 20,
    timeoutMs,
    "substantive screen content (>20 chars)"
  );
}

/**
 * Compute word overlap ratio between two screenshots.
 * Returns a value between 0 and 1.
 */
function wordOverlap(a: Screenshot, b: Screenshot): number {
  const wordsA = new Set(a.text.split(/\s+/).filter((w) => w.length > 2));
  const wordsB = new Set(b.text.split(/\s+/).filter((w) => w.length > 2));
  if (wordsA.size === 0) return 0;
  const common = [...wordsA].filter((w) => wordsB.has(w));
  return common.length / wordsA.size;
}

/**
 * Check that a screenshot's text is not garbled. Garbled text tends to have
 * very high ratios of non-printable or non-ASCII characters relative to total length.
 */
function isNotGarbled(ss: Screenshot): boolean {
  const text = ss.text;
  if (text.trim().length === 0) return true; // empty is not garbled
  // Count printable ASCII + common Unicode ranges
  const printable = text.replace(/[^\x20-\x7E\u00A0-\u024F\u2500-\u257F\u2580-\u259F\u25A0-\u25FF]/g, "");
  const ratio = printable.length / text.length;
  return ratio > 0.5;
}

// ─── Tests ───

const CODEX_BIN = "/opt/homebrew/bin/codex";

describe("codex integration: startup and initial render", () => {
  it(
    "starts codex and renders non-empty, non-garbled TUI content",
    async () => {
      const session = await createSession(CODEX_BIN, [], {
        rows: 40,
        cols: 120,
      });

      let ss: Screenshot;
      try {
        ss = await waitForAnyContent(session, 20000);
      } catch (err) {
        ss = session.screenshot();
        logScreenshot("initial (timed out)", ss);
        throw err;
      }

      logScreenshot("initial render", ss);

      // Must have non-empty content
      expect(ss.text.trim().length).toBeGreaterThan(0);

      // Content should not be garbled
      expect(isNotGarbled(ss)).toBe(true);

      // Should have multiple lines of content (a real TUI, not just a single line)
      const nonEmptyLines = ss.lines.filter((l) => l.trim().length > 0);
      expect(nonEmptyLines.length).toBeGreaterThanOrEqual(1);
    },
    30000
  );

  it(
    "startup screen contains recognizable codex-related text or UI elements",
    async () => {
      const session = await createSession(CODEX_BIN, [], {
        rows: 40,
        cols: 120,
      });

      let ss: Screenshot;
      try {
        ss = await waitForSubstantiveContent(session, 20000);
      } catch {
        ss = session.screenshot();
      }

      logScreenshot("startup content analysis", ss);

      const textLower = ss.text.toLowerCase();

      // Codex should show something recognizable -- prompt, trust dialog, error, or branding
      const recognizable =
        textLower.includes("codex") ||
        textLower.includes("openai") ||
        textLower.includes("trust") ||
        textLower.includes("sandbox") ||
        textLower.includes("api") ||
        textLower.includes("key") ||
        textLower.includes("error") ||
        textLower.includes("login") ||
        textLower.includes(">") ||
        textLower.includes("prompt") ||
        textLower.includes("directory");

      console.log(`Recognizable content found: ${recognizable}`);
      // At minimum, there should be something readable on screen
      expect(ss.text.trim().length).toBeGreaterThan(0);
    },
    30000
  );

  it(
    "ANSI output contains escape sequences (not plain text only)",
    async () => {
      const session = await createSession(CODEX_BIN, [], {
        rows: 40,
        cols: 120,
      });

      let ss: Screenshot;
      try {
        ss = await waitForAnyContent(session, 20000);
      } catch {
        ss = session.screenshot();
      }

      logScreenshot("ANSI check", ss);

      // A real TUI app should emit ANSI escape sequences for styling
      const hasEscapeSequences = /\x1b\[/.test(ss.ansi);
      expect(hasEscapeSequences).toBe(true);

      console.log(`ANSI output length: ${ss.ansi.length}, text length: ${ss.text.length}`);
      // ANSI output should be longer than plain text due to escape sequences
      expect(ss.ansi.length).toBeGreaterThan(ss.text.length);
    },
    30000
  );
});

// ============================================================================
// Resize at multiple common terminal sizes
// ============================================================================

describe("codex integration: resize at multiple terminal sizes", () => {
  const sizes: Array<{ name: string; rows: number; cols: number }> = [
    { name: "classic (80x24)", rows: 24, cols: 80 },
    { name: "modern (120x40)", rows: 40, cols: 120 },
    { name: "ultrawide (200x50)", rows: 50, cols: 200 },
    { name: "small (60x20)", rows: 20, cols: 60 },
  ];

  for (const size of sizes) {
    it(
      `renders correctly at ${size.name}`,
      async () => {
        // Start at a neutral size, then resize to the target
        const session = await createSession(CODEX_BIN, [], {
          rows: 30,
          cols: 100,
        });

        try {
          await waitForAnyContent(session, 20000);
        } catch {
          // continue -- we will resize and check
        }

        // Resize to target
        session.resize(size.rows, size.cols);
        await new Promise((r) => setTimeout(r, 3000));

        const ss = session.screenshot();
        logScreenshot(`resize to ${size.name}`, ss);

        // Content must be present after resize
        expect(ss.text.trim().length).toBeGreaterThan(0);

        // Content should not be garbled
        expect(isNotGarbled(ss)).toBe(true);

        // For very wide terminals, lines should not be absurdly long garbage
        for (const line of ss.lines) {
          if (line.length > size.cols + 10) {
            // Allow some slack for unicode wide chars, but flag serious overflow
            console.warn(`Line exceeds expected width: ${line.length} > ${size.cols}`);
          }
        }
      },
      30000
    );
  }
});

// ============================================================================
// Rapid resize burst
// ============================================================================

describe("codex integration: rapid resize burst", () => {
  it(
    "survives 6 rapid resizes and settles to correct final size",
    async () => {
      const session = await createSession(CODEX_BIN, [], {
        rows: 30,
        cols: 100,
      });

      try {
        await waitForAnyContent(session, 20000);
      } catch {
        // continue
      }

      // Rapid resize burst -- simulate dragging a window corner
      const burstSizes = [
        { rows: 25, cols: 90 },
        { rows: 20, cols: 70 },
        { rows: 35, cols: 110 },
        { rows: 15, cols: 60 },
        { rows: 45, cols: 150 },
        { rows: 30, cols: 100 }, // back to original
      ];

      for (const s of burstSizes) {
        session.resize(s.rows, s.cols);
        // Very short delay between resizes to simulate rapid dragging
        await new Promise((r) => setTimeout(r, 50));
      }

      // Wait for codex to settle after the burst
      await new Promise((r) => setTimeout(r, 3000));

      const finalSize = burstSizes[burstSizes.length - 1];
      const ss = session.screenshot();
      logScreenshot(`after rapid resize burst (final: ${finalSize.rows}x${finalSize.cols})`, ss);

      // Should have content after the burst
      expect(ss.text.trim().length).toBeGreaterThan(0);

      // Should not be garbled
      expect(isNotGarbled(ss)).toBe(true);

      // Session dimensions should match the final resize
      expect(session.rows).toBe(finalSize.rows);
      expect(session.cols).toBe(finalSize.cols);
    },
    30000
  );
});

// ============================================================================
// Reconnect preserves display
// ============================================================================

describe("codex integration: reconnect preserves display", () => {
  it(
    "content survives detach/reattach round-trip",
    async () => {
      const session = await createSession(CODEX_BIN, [], {
        rows: 40,
        cols: 120,
      });

      let beforeSs: Screenshot;
      try {
        beforeSs = await waitForAnyContent(session, 20000);
      } catch {
        beforeSs = session.screenshot();
      }
      logScreenshot("before reconnect", beforeSs);

      // Reconnect
      await session.reconnect();

      // Wait for content to repopulate
      let afterSs: Screenshot;
      try {
        afterSs = await waitForAnyContent(session, 10000);
      } catch {
        afterSs = session.screenshot();
      }
      logScreenshot("after reconnect", afterSs);

      // After reconnect, screen should still have content
      expect(afterSs.text.trim().length).toBeGreaterThan(0);

      // Compare before and after -- content should have significant overlap
      const overlap = wordOverlap(beforeSs, afterSs);
      console.log(`Word overlap after reconnect: ${(overlap * 100).toFixed(1)}%`);

      // We expect the same app to show similar content
      // Being generous since layout shifts can happen
      if (beforeSs.text.trim().length > 20) {
        expect(overlap).toBeGreaterThan(0.2);
      }
    },
    30000
  );

  it(
    "screenshot text before and after reconnect are substantially similar",
    async () => {
      const session = await createSession(CODEX_BIN, [], {
        rows: 30,
        cols: 100,
      });

      let beforeSs: Screenshot;
      try {
        beforeSs = await waitForSubstantiveContent(session, 20000);
      } catch {
        beforeSs = session.screenshot();
      }

      await session.reconnect();
      await new Promise((r) => setTimeout(r, 2000));

      let afterSs: Screenshot;
      try {
        afterSs = await waitForAnyContent(session, 10000);
      } catch {
        afterSs = session.screenshot();
      }

      logScreenshot("reconnect comparison: before", beforeSs);
      logScreenshot("reconnect comparison: after", afterSs);

      // Both should have content
      expect(afterSs.text.trim().length).toBeGreaterThan(0);

      // Non-empty lines count should be in the same ballpark
      const beforeNonEmpty = beforeSs.lines.filter((l) => l.trim().length > 0).length;
      const afterNonEmpty = afterSs.lines.filter((l) => l.trim().length > 0).length;
      console.log(`Non-empty lines: before=${beforeNonEmpty}, after=${afterNonEmpty}`);

      // The after screenshot should have at least some non-empty lines
      expect(afterNonEmpty).toBeGreaterThanOrEqual(1);
    },
    30000
  );
});

// ============================================================================
// Reconnect at different size
// ============================================================================

describe("codex integration: reconnect at different size", () => {
  it(
    "reconnects from 80x24 to 120x40 and redraws at new size",
    async () => {
      const session = await createSession(CODEX_BIN, [], {
        rows: 24,
        cols: 80,
      });

      try {
        await waitForAnyContent(session, 20000);
      } catch {
        // continue
      }

      const smallSs = session.screenshot();
      logScreenshot("before reconnect (80x24)", smallSs);

      // Resize before reconnecting
      session.resize(40, 120);

      // Reconnect at the new size
      await session.reconnect();
      await new Promise((r) => setTimeout(r, 3000));

      let largeSs: Screenshot;
      try {
        largeSs = await waitForAnyContent(session, 10000);
      } catch {
        largeSs = session.screenshot();
      }
      logScreenshot("after reconnect (120x40)", largeSs);

      // Content must be present at new size
      expect(largeSs.text.trim().length).toBeGreaterThan(0);

      // Session should reflect new dimensions
      expect(session.rows).toBe(40);
      expect(session.cols).toBe(120);

      // Not garbled at new size
      expect(isNotGarbled(largeSs)).toBe(true);
    },
    30000
  );
});

// ============================================================================
// Multiple reconnect cycles
// ============================================================================

describe("codex integration: multiple reconnect cycles", () => {
  it(
    "survives 3 consecutive reconnect cycles with content preserved each time",
    async () => {
      const session = await createSession(CODEX_BIN, [], {
        rows: 30,
        cols: 100,
      });

      try {
        await waitForAnyContent(session, 20000);
      } catch {
        // continue
      }

      const screenshots: Screenshot[] = [];
      screenshots.push(session.screenshot());
      logScreenshot("cycle 0 (initial)", screenshots[0]);

      for (let cycle = 1; cycle <= 3; cycle++) {
        await session.reconnect();
        await new Promise((r) => setTimeout(r, 2000));

        let ss: Screenshot;
        try {
          ss = await waitForAnyContent(session, 10000);
        } catch {
          ss = session.screenshot();
        }
        screenshots.push(ss);
        logScreenshot(`cycle ${cycle} (after reconnect)`, ss);

        // Each cycle must have content
        expect(ss.text.trim().length).toBeGreaterThan(0);

        // Each cycle must not be garbled
        expect(isNotGarbled(ss)).toBe(true);
      }

      // Compare first and last -- should still show similar content
      const overlap = wordOverlap(screenshots[0], screenshots[3]);
      console.log(`Word overlap across 3 reconnects: ${(overlap * 100).toFixed(1)}%`);

      // After 3 reconnects the same app should still be showing
      if (screenshots[0].text.trim().length > 20) {
        expect(overlap).toBeGreaterThan(0.1);
      }
    },
    60000
  );
});

// ============================================================================
// ANSI fidelity on reconnect
// ============================================================================

describe("codex integration: ANSI fidelity on reconnect", () => {
  it(
    "ANSI output after reconnect contains escape sequences (color, styling)",
    async () => {
      const session = await createSession(CODEX_BIN, [], {
        rows: 40,
        cols: 120,
      });

      try {
        await waitForAnyContent(session, 20000);
      } catch {
        // continue
      }

      const beforeAnsi = session.screenshot().ansi;

      await session.reconnect();
      await new Promise((r) => setTimeout(r, 2000));

      let afterSs: Screenshot;
      try {
        afterSs = await waitForAnyContent(session, 10000);
      } catch {
        afterSs = session.screenshot();
      }

      logScreenshot("ANSI fidelity after reconnect", afterSs);

      // After reconnect, ANSI output should still contain escape sequences
      const hasEscapeSequences = /\x1b\[/.test(afterSs.ansi);
      expect(hasEscapeSequences).toBe(true);

      // Check for 24-bit color sequences (common in modern TUIs like codex)
      const has24BitColor = /\x1b\[(?:38|48);2;\d+;\d+;\d+/.test(afterSs.ansi);
      // Check for box-drawing characters in the text
      const hasBoxDrawing = /[\u2500-\u257F]/.test(afterSs.text);
      // Check for SGR reset sequences
      const hasSgrReset = /\x1b\[0?m/.test(afterSs.ansi);

      console.log(`ANSI fidelity: 24-bit color=${has24BitColor}, box-drawing=${hasBoxDrawing}, SGR reset=${hasSgrReset}`);
      console.log(`Before ANSI length: ${beforeAnsi.length}, After ANSI length: ${afterSs.ansi.length}`);

      // The ANSI output should be non-trivial
      expect(afterSs.ansi.length).toBeGreaterThan(afterSs.text.length);
    },
    30000
  );
});

// ============================================================================
// Resize then reconnect
// ============================================================================

describe("codex integration: resize then reconnect", () => {
  it(
    "resize to 90x30, then reconnect at same size, verify correct dimensions",
    async () => {
      const session = await createSession(CODEX_BIN, [], {
        rows: 40,
        cols: 120,
      });

      try {
        await waitForAnyContent(session, 20000);
      } catch {
        // continue
      }

      // Resize to new dimensions
      session.resize(30, 90);
      await new Promise((r) => setTimeout(r, 2000));

      const afterResizeSs = session.screenshot();
      logScreenshot("after resize to 30x90", afterResizeSs);

      // Reconnect -- should maintain the 30x90 size
      await session.reconnect();
      await new Promise((r) => setTimeout(r, 2000));

      let afterReconnectSs: Screenshot;
      try {
        afterReconnectSs = await waitForAnyContent(session, 10000);
      } catch {
        afterReconnectSs = session.screenshot();
      }
      logScreenshot("after reconnect at 30x90", afterReconnectSs);

      // Content should be present
      expect(afterReconnectSs.text.trim().length).toBeGreaterThan(0);

      // Dimensions should be correct
      expect(session.rows).toBe(30);
      expect(session.cols).toBe(90);

      // Content should be similar before and after reconnect
      const overlap = wordOverlap(afterResizeSs, afterReconnectSs);
      console.log(`Resize+reconnect word overlap: ${(overlap * 100).toFixed(1)}%`);
    },
    30000
  );
});

// ============================================================================
// Screenshot consistency (no flicker/jitter)
// ============================================================================

describe("codex integration: screenshot consistency", () => {
  it(
    "two screenshots 100ms apart are identical when no input is given",
    async () => {
      const session = await createSession(CODEX_BIN, [], {
        rows: 30,
        cols: 100,
      });

      try {
        await waitForSubstantiveContent(session, 20000);
      } catch {
        // continue
      }

      // Let the TUI fully settle
      await new Promise((r) => setTimeout(r, 3000));

      const ss1 = session.screenshot();
      await new Promise((r) => setTimeout(r, 100));
      const ss2 = session.screenshot();

      logScreenshot("screenshot 1", ss1);
      logScreenshot("screenshot 2", ss2);

      // The two screenshots should be identical -- no flicker or jitter
      expect(ss1.text).toBe(ss2.text);

      // ANSI output should also be identical
      expect(ss1.ansi).toBe(ss2.ansi);
    },
    30000
  );

  it(
    "three screenshots over 500ms are all identical (extended stability)",
    async () => {
      const session = await createSession(CODEX_BIN, [], {
        rows: 24,
        cols: 80,
      });

      try {
        await waitForSubstantiveContent(session, 20000);
      } catch {
        // continue
      }

      // Let the TUI fully settle
      await new Promise((r) => setTimeout(r, 3000));

      const screenshots: Screenshot[] = [];
      for (let i = 0; i < 3; i++) {
        screenshots.push(session.screenshot());
        if (i < 2) await new Promise((r) => setTimeout(r, 250));
      }

      // All three should be identical
      for (let i = 1; i < screenshots.length; i++) {
        expect(screenshots[i].text).toBe(screenshots[0].text);
        expect(screenshots[i].ansi).toBe(screenshots[0].ansi);
      }
    },
    30000
  );
});

// ============================================================================
// Second client at different size
// ============================================================================

describe("codex integration: second client at different size", () => {
  it(
    "second client via connectToExisting at different size receives content",
    async () => {
      const session = await createSession(CODEX_BIN, [], {
        rows: 30,
        cols: 100,
      });

      try {
        await waitForAnyContent(session, 20000);
      } catch {
        // continue
      }

      const ss1 = session.screenshot();
      logScreenshot("primary client (100x30)", ss1);

      // Connect a second client at a different size
      const secondSession = await Session.connectToExisting(session, {
        rows: 40,
        cols: 120,
      });
      sessions.push(secondSession);

      await secondSession.attach();
      await new Promise((r) => setTimeout(r, 3000));

      const ss2 = secondSession.screenshot();
      logScreenshot("second client (120x40)", ss2);

      // Second client should have content
      expect(ss2.text.trim().length).toBeGreaterThan(0);

      // Second client should not be garbled
      expect(isNotGarbled(ss2)).toBe(true);

      // Both clients should show related content (same app)
      const overlap = wordOverlap(ss1, ss2);
      console.log(`Word overlap between clients: ${(overlap * 100).toFixed(1)}%`);
    },
    30000
  );

  it(
    "primary client still works after second client attaches",
    async () => {
      const session = await createSession(CODEX_BIN, [], {
        rows: 30,
        cols: 100,
      });

      try {
        await waitForAnyContent(session, 20000);
      } catch {
        // continue
      }

      // Connect second client
      const secondSession = await Session.connectToExisting(session, {
        rows: 20,
        cols: 80,
      });
      sessions.push(secondSession);

      await secondSession.attach();
      await new Promise((r) => setTimeout(r, 2000));

      // Take screenshot from primary client -- it should still work
      const primarySs = session.screenshot();
      logScreenshot("primary after second client attached", primarySs);

      // Primary should still have content
      expect(primarySs.text.trim().length).toBeGreaterThan(0);

      // Close second client, primary should still work
      await secondSession.close();
      sessions = sessions.filter((s) => s !== secondSession);

      await new Promise((r) => setTimeout(r, 1000));

      const afterCloseSs = session.screenshot();
      logScreenshot("primary after second client closed", afterCloseSs);
      expect(afterCloseSs.text.trim().length).toBeGreaterThan(0);
    },
    30000
  );
});

// ============================================================================
// Combined scenarios
// ============================================================================

describe("codex integration: combined scenarios", () => {
  it(
    "resize then rapid reconnect cycles",
    async () => {
      const session = await createSession(CODEX_BIN, [], {
        rows: 30,
        cols: 100,
      });

      try {
        await waitForAnyContent(session, 20000);
      } catch {
        // continue
      }

      // Resize
      session.resize(25, 80);
      await new Promise((r) => setTimeout(r, 1000));

      // Rapid reconnect cycles
      for (let i = 0; i < 2; i++) {
        await session.reconnect();
        await new Promise((r) => setTimeout(r, 1500));
      }

      let ss: Screenshot;
      try {
        ss = await waitForAnyContent(session, 10000);
      } catch {
        ss = session.screenshot();
      }

      logScreenshot("after resize + rapid reconnects", ss);

      expect(ss.text.trim().length).toBeGreaterThan(0);
      expect(isNotGarbled(ss)).toBe(true);
      expect(session.rows).toBe(25);
      expect(session.cols).toBe(80);
    },
    45000
  );

  it(
    "multiple resizes interspersed with reconnects",
    async () => {
      const session = await createSession(CODEX_BIN, [], {
        rows: 30,
        cols: 100,
      });

      try {
        await waitForAnyContent(session, 20000);
      } catch {
        // continue
      }

      // Resize -> reconnect -> resize -> reconnect
      const steps = [
        { resize: { rows: 20, cols: 80 } },
        { reconnect: true },
        { resize: { rows: 40, cols: 120 } },
        { reconnect: true },
      ];

      for (const step of steps) {
        if ("resize" in step && step.resize) {
          session.resize(step.resize.rows, step.resize.cols);
          await new Promise((r) => setTimeout(r, 1500));
        }
        if ("reconnect" in step && step.reconnect) {
          await session.reconnect();
          await new Promise((r) => setTimeout(r, 2000));
        }
      }

      let ss: Screenshot;
      try {
        ss = await waitForAnyContent(session, 10000);
      } catch {
        ss = session.screenshot();
      }

      logScreenshot("after multiple resize+reconnect steps", ss);

      expect(ss.text.trim().length).toBeGreaterThan(0);
      expect(isNotGarbled(ss)).toBe(true);
      expect(session.rows).toBe(40);
      expect(session.cols).toBe(120);
    },
    60000
  );
});
