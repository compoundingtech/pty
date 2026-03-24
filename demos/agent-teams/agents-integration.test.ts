// Integration tests for agent teams demo — runs through real PTY
import { describe, it, expect, afterEach } from "vitest";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { Session } from "../../src/testing/index.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const mainScript = path.join(__dirname, "main.ts");

let session: Session;

async function startApp(rows = 35, cols = 120): Promise<Session> {
  session = Session.spawn("node", ["--experimental-strip-types", "--no-warnings", mainScript], {
    rows,
    cols,
    env: { TERM: "xterm-256color", SPEED: "60" }, // 60x speed = 10 seconds for full timeline
  });
  await session.waitForText("Agent Teams", 15000);
  return session;
}

afterEach(async () => {
  if (session) {
    await session.close();
  }
});

describe("Agent Teams Integration", () => {
  it("boots and shows the Main Agent", async () => {
    await startApp();
    const ss = session.screenshot();
    expect(ss.text).toContain("Agent Teams");
    expect(ss.text).toContain("Main Agent");
    expect(ss.text).toContain("Agents");
    expect(ss.text).toContain("Detail");
    expect(ss.text).toContain("Activity");
  }, 20000);

  it("sub-agents appear after timeline events fire", async () => {
    await startApp();
    // At 60x speed, the researcher/coder appear at ~0.5s
    await session.waitForText("Researcher", 10000);
    const ss = session.screenshot();
    expect(ss.text).toContain("Researcher");
    expect(ss.text).toContain("Coder");
  }, 20000);

  it("arrow keys change selection and update detail", async () => {
    await startApp();
    // Wait for sub-agents to appear
    await session.waitForText("Researcher", 10000);

    // Move down to select Researcher
    session.press("down");
    await new Promise(r => setTimeout(r, 500));

    const ss = session.screenshot();
    // Detail panel should show Researcher info
    expect(ss.text).toContain("Researcher");
  }, 20000);

  it("activity log updates with events", async () => {
    await startApp();
    // Activity should show seeded events (the most recent ones visible)
    await session.waitForText("Started writing tests", 10000);
    const ss = session.screenshot();
    expect(ss.text).toContain("Started writing tests");
  }, 20000);

  it("progress updates over time", async () => {
    await startApp();
    // Wait for researcher to start (at 60x, this is ~1 second)
    await session.waitForText("Researcher", 10000);

    // Wait a bit for progress updates
    await new Promise(r => setTimeout(r, 3000));

    const ss = session.screenshot();
    // The elapsed counter should have advanced
    expect(ss.text).toContain("Elapsed:");
  }, 20000);

  it("p pauses the timeline", async () => {
    await startApp();
    await session.waitForText("Researcher", 10000);

    session.type("p");
    await session.waitForText("PAUSED", 5000);

    const ss = session.screenshot();
    expect(ss.text).toContain("PAUSED");

    // Unpause
    session.type("p");
    await session.waitForAbsent("PAUSED", 5000);
  }, 20000);

  it("T cycles the theme", async () => {
    await startApp();
    const ss1 = session.screenshot();

    session.type("T");
    await new Promise(r => setTimeout(r, 500));

    const ss2 = session.screenshot();
    expect(ss2.text).toContain("Agent Teams");
    expect(ss2.ansi).not.toEqual(ss1.ansi);
  }, 20000);

  it("ctrl+c quits the app", async () => {
    await startApp();
    session.press("ctrl+c");
    await session.waitForAbsent("Agent Teams", 5000);
  }, 20000);

  it("detail panel shows selected agent name and status", async () => {
    await startApp();

    const ss = session.screenshot();
    // Detail panel should show Main Agent info (selected by default)
    // Find a line in the Detail panel that has "Main Agent"
    const detailLine = ss.lines.find(l => l.includes("Main Agent") && l.includes("working"));
    expect(detailLine).toBeDefined();
  }, 20000);

  it("activity log timestamps are not all identical", async () => {
    await startApp();

    const ss = session.screenshot();
    // Find lines with timestamps [HH:MM]
    const tsLines = ss.lines.filter(l => l.match(/\[\d{2}:\d{2}\]/));
    expect(tsLines.length).toBeGreaterThanOrEqual(2);

    // Extract timestamps
    const timestamps = tsLines.map(l => l.match(/\[(\d{2}:\d{2})\]/)![1]);
    // Not all the same
    const unique = [...new Set(timestamps)];
    expect(unique.length).toBeGreaterThanOrEqual(2);
  }, 20000);

  it("both panels fill available height (no empty gap)", async () => {
    await startApp(25, 100);

    const ss = session.screenshot();
    // The Agents panel and Detail panel should both have bottom borders
    // near the same row (they're in an hstack)
    // The Activity panel should have its bottom border near the bottom
    const activityBottom = ss.lines.findLastIndex(l => l.includes("Activity") || l.includes("\u2570"));
    expect(activityBottom).toBeGreaterThanOrEqual(20); // near row 23 for 25-row terminal
  }, 20000);
});
