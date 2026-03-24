// Integration tests for file browser demo — runs through real PTY
import { describe, it, expect, afterEach, beforeAll } from "vitest";
import * as path from "node:path";
import * as fs from "node:fs";
import * as os from "node:os";
import { fileURLToPath } from "node:url";
import { Session } from "../../src/testing/index.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const mainScript = path.join(__dirname, "main.ts");

let testDir: string;
let session: Session;

beforeAll(() => {
  // Create a test directory with known structure
  testDir = fs.mkdtempSync(path.join(os.tmpdir(), "fb-integ-"));
  fs.mkdirSync(path.join(testDir, "docs"));
  fs.writeFileSync(path.join(testDir, "docs", "readme.txt"), "This is the readme.\nLine 2.\nLine 3.\n");
  fs.mkdirSync(path.join(testDir, "src"));
  fs.writeFileSync(path.join(testDir, "src", "main.ts"), "console.log('hello');\nconst x = 42;\n");
  fs.writeFileSync(path.join(testDir, "package.json"), '{\n  "name": "test"\n}\n');
  fs.writeFileSync(path.join(testDir, "notes.txt"), "Some notes here.\n");
  fs.writeFileSync(path.join(testDir, "readme.md"), "# Project Title\n\nThis is a description.\n\n## Getting Started\n\nRun the app.\n");
});

async function startApp(rows = 30, cols = 100): Promise<Session> {
  session = Session.spawn("node", ["--experimental-strip-types", "--no-warnings", mainScript, testDir], {
    rows,
    cols,
    env: { TERM: "xterm-256color" },
  });
  // Wait for the File Browser status bar to appear
  await session.waitForText("File Browser", 15000);
  return session;
}

afterEach(async () => {
  if (session) {
    await session.close();
  }
});

describe("File Browser Integration", () => {
  it("boots and shows the directory listing", async () => {
    await startApp();
    const ss = session.screenshot();
    expect(ss.text).toContain("File Browser");
    expect(ss.text).toContain("Files");
    expect(ss.text).toContain("docs");
    expect(ss.text).toContain("src");
    expect(ss.text).toContain("notes.txt");
    expect(ss.text).toContain("package.json");
  }, 20000);

  it("arrow keys move selection through the tree", async () => {
    await startApp();

    // First item should be "docs" (dirs first)
    let ss = session.screenshot();
    expect(ss.text).toContain("docs");

    // Move down to select next item
    session.press("down");
    await new Promise(r => setTimeout(r, 300));

    ss = session.screenshot();
    // Should still show all items
    expect(ss.text).toContain("src");
  }, 20000);

  it("right arrow expands a directory", async () => {
    await startApp();

    // First item should be "docs"
    session.press("right");
    await session.waitForText("readme.txt", 5000);

    const ss = session.screenshot();
    expect(ss.text).toContain("readme.txt");
  }, 20000);

  it("left arrow collapses an expanded directory", async () => {
    await startApp();

    // Expand docs
    session.press("right");
    await session.waitForText("readme.txt", 5000);

    // Collapse docs
    session.press("left");
    await session.waitForAbsent("readme.txt", 5000);
  }, 20000);

  it("enter on a text file shows preview content", async () => {
    await startApp();

    // Navigate: docs(0), src(1), notes.txt(2), package.json(3)
    // Move to notes.txt
    session.press("down");
    session.press("down");
    await new Promise(r => setTimeout(r, 300));

    session.press("return");
    await session.waitForText("Some notes", 5000);

    const ss = session.screenshot();
    expect(ss.text).toContain("Preview");
    expect(ss.text).toContain("Some notes");
  }, 20000);

  it("tab switches between panes", async () => {
    await startApp();

    // Load a file first
    session.press("down");
    session.press("down");
    session.press("return");
    await session.waitForText("Some notes", 5000);

    // Switch to preview pane
    session.press("tab");
    await new Promise(r => setTimeout(r, 300));

    // Now up/down should scroll preview, not tree
    // We can't directly verify focus visually in a simple way,
    // but the app shouldn't crash
    session.press("down");
    await new Promise(r => setTimeout(r, 200));

    const ss = session.screenshot();
    expect(ss.text).toContain("File Browser");
  }, 20000);

  it("T cycles the theme", async () => {
    await startApp();
    const ss1 = session.screenshot();

    session.type("T");
    await new Promise(r => setTimeout(r, 500));

    const ss2 = session.screenshot();
    // The screen should still show content but ANSI codes change
    expect(ss2.text).toContain("File Browser");
    // The ansi output should differ due to theme change
    expect(ss2.ansi).not.toEqual(ss1.ansi);
  }, 20000);

  it("ctrl+c quits the app", async () => {
    await startApp();
    session.press("ctrl+c");
    // App should exit and leave alt screen — File Browser will disappear
    await session.waitForAbsent("File Browser", 5000);
  }, 20000);

  it("q quits the app", async () => {
    await startApp();
    session.type("q");
    // App should exit and leave alt screen
    await session.waitForAbsent("File Browser", 5000);
  }, 20000);

  it("markdown file preview shows heading content", async () => {
    await startApp();

    // Navigate to readme.md (dirs first: docs, src, then files alphabetically)
    // Files: notes.txt, package.json, readme.md — so readme.md is at index 4
    // Let's navigate down to it
    session.press("down"); // src
    session.press("down"); // notes.txt
    session.press("down"); // package.json
    session.press("down"); // readme.md
    await new Promise(r => setTimeout(r, 300));

    session.press("return");
    await session.waitForText("Project Title", 5000);

    const ss = session.screenshot();
    expect(ss.text).toContain("Project Title");
    expect(ss.text).toContain("Getting Started");
  }, 20000);

  it("wrapped lines: continuation text starts after the gutter, not at the panel edge", async () => {
    // Create a file with a long line that will wrap
    const longLine = "word ".repeat(30); // 150 chars
    fs.writeFileSync(path.join(testDir, "long.txt"), longLine + "\nshort\n");

    await startApp(20, 60);

    // Navigate: docs(0), src(1), long.txt(2)
    session.press("down"); // src
    session.press("down"); // long.txt
    await new Promise(r => setTimeout(r, 300));

    session.press("return");
    await session.waitForText("word", 5000);

    const ss = session.screenshot();

    // Find the first preview line — it has line number "1 │" followed by content
    const gutterLine = ss.lines.find(l => l.includes("1 \u2502 word"));
    expect(gutterLine).toBeDefined();

    // Find where the content text starts (the "w" in "word" after the gutter)
    const contentCol = gutterLine!.indexOf("word");

    // The next screen row should be a continuation line (wrapped content)
    const gutterLineIdx = ss.lines.indexOf(gutterLine!);
    const nextLine = ss.lines[gutterLineIdx + 1];

    // Continuation content should start at or near the same column as the first line's content
    // (off-by-one is expected: word-wrap breaks before the space, so continuation
    //  may start with a space before the first word)
    const nextWordCol = nextLine.indexOf("word");
    expect(nextWordCol).toBeGreaterThanOrEqual(0);
    expect(Math.abs(nextWordCol - contentCol)).toBeLessThanOrEqual(1);

    // The continuation should NOT have a line number
    expect(nextLine).not.toMatch(/\d\s*\u2502\s*word/);
  }, 20000);

  it("short lines do not wrap: line number and content on the same row", async () => {
    await startApp(20, 100);

    // notes.txt has a short line
    session.press("down"); // src
    session.press("down"); // notes.txt (after long.txt created above)
    session.press("down");
    await new Promise(r => setTimeout(r, 300));

    session.press("return");
    await session.waitForText("Some notes", 5000);

    const ss = session.screenshot();

    // Line number and content should be on the same row
    const notesLine = ss.lines.find(l => l.includes("Some notes here"));
    expect(notesLine).toBeDefined();
    // Same line must have gutter separator
    expect(notesLine).toMatch(/\d\s*\u2502/);
  }, 20000);

  it("preview panel fills the available height, not just content height", async () => {
    await startApp(25, 80);

    // Select any file — navigate past dirs to first file, press enter
    // Keep pressing down until we're past the dirs, then load preview
    for (let i = 0; i < 8; i++) session.press("down");
    await new Promise(r => setTimeout(r, 300));
    session.press("return");
    await new Promise(r => setTimeout(r, 500));

    const ss = session.screenshot();

    // The preview panel's bottom border ╰ should be near the bottom of the screen,
    // not right after the content. Find the last row with ╰ on the right side.
    const previewBottomBorder = ss.lines.findLastIndex(l => {
      // The right panel's bottom border contains ╰ after column 30ish
      const rightHalf = l.slice(25);
      return rightHalf.includes("\u2570");
    });

    // The Files panel bottom border
    const filesBottomBorder = ss.lines.findLastIndex(l => {
      const leftHalf = l.slice(0, 30);
      return leftHalf.includes("\u2570");
    });

    // Both panels should end at roughly the same row (near the bottom)
    expect(Math.abs(previewBottomBorder - filesBottomBorder)).toBeLessThanOrEqual(1);
    // And that row should be close to the bottom (row 23 for 25-row terminal)
    expect(previewBottomBorder).toBeGreaterThanOrEqual(20);
  }, 20000);

  it("each source line gets its own line number", async () => {
    fs.writeFileSync(path.join(testDir, "multi.txt"), "alpha\nbeta\ngamma\n");

    await startApp(20, 80);

    // Navigate to multi.txt
    session.press("down"); // src
    session.press("down"); // long.txt
    session.press("down"); // multi.txt
    await new Promise(r => setTimeout(r, 300));

    session.press("return");
    await session.waitForText("alpha", 5000);

    const ss = session.screenshot();

    // Each line should have its own line number with gutter
    const alphaLine = ss.lines.find(l => l.match(/1\s*\u2502/) && l.includes("alpha"));
    const betaLine = ss.lines.find(l => l.match(/2\s*\u2502/) && l.includes("beta"));
    const gammaLine = ss.lines.find(l => l.match(/3\s*\u2502/) && l.includes("gamma"));
    expect(alphaLine).toBeDefined();
    expect(betaLine).toBeDefined();
    expect(gammaLine).toBeDefined();
  }, 20000);
});
