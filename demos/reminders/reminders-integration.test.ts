// Integration tests for reminders demo — runs through real PTY
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
    env: { TERM: "xterm-256color" },
  });
  await session.waitForText("Reminders", 15000);
  return session;
}

afterEach(async () => {
  if (session) {
    await session.close();
  }
});

describe("Reminders Integration", () => {
  it("boots with seeded data and shows reminders", async () => {
    await startApp();
    const ss = session.screenshot();
    expect(ss.text).toContain("Reminders");
    expect(ss.text).toContain("Buy groceries");
  }, 20000);

  it("arrow keys navigate the list", async () => {
    await startApp();

    session.press("down");
    await new Promise(r => setTimeout(r, 300));

    session.press("down");
    await new Promise(r => setTimeout(r, 300));

    const ss = session.screenshot();
    // Should still show the reminders app
    expect(ss.text).toContain("Reminders");
  }, 20000);

  it("v cycles through views", async () => {
    await startApp();

    // Start in list view
    let ss = session.screenshot();
    expect(ss.text).toContain("list");

    // Switch to board view
    session.type("v");
    await session.waitForText("board", 5000);
    ss = session.screenshot();
    expect(ss.text).toContain("board");
    expect(ss.text).toContain("Todo");
    expect(ss.text).toContain("Done");

    // Switch to calendar view
    session.type("v");
    await session.waitForText("calendar", 5000);
    ss = session.screenshot();
    expect(ss.text).toContain("calendar");

    // Back to list
    session.type("v");
    await session.waitForText("list", 5000);
  }, 20000);

  it("space toggles completion", async () => {
    await startApp();

    // Find "Buy groceries" and check its initial state
    let ss = session.screenshot();
    expect(ss.text).toContain("Buy groceries");

    // Toggle completion on first item
    session.press("space");
    await new Promise(r => setTimeout(r, 500));

    ss = session.screenshot();
    // The item should still be present (moved to Completed group)
    expect(ss.text).toContain("Buy groceries");
  }, 20000);

  it("n opens new reminder overlay", async () => {
    await startApp();

    session.type("n");
    await session.waitForText("New Reminder", 5000);

    const ss = session.screenshot();
    expect(ss.text).toContain("New Reminder");
    expect(ss.text).toContain("Title");
    expect(ss.text).toContain("Priority");

    // Close with escape
    session.press("escape");
    await session.waitForAbsent("New Reminder", 5000);
  }, 20000);

  it("d + y deletes a reminder", async () => {
    await startApp();

    session.type("d");
    await session.waitForText("Delete", 5000);

    let ss = session.screenshot();
    expect(ss.text).toContain("Delete");

    session.type("y");
    await session.waitForAbsent("Delete", 5000);

    // Should have fewer reminders
    ss = session.screenshot();
    expect(ss.text).toContain("Reminders");
  }, 20000);

  it("T cycles theme", async () => {
    await startApp();
    const ss1 = session.screenshot();

    session.type("T");
    await new Promise(r => setTimeout(r, 500));

    const ss2 = session.screenshot();
    expect(ss2.text).toContain("Reminders");
    expect(ss2.ansi).not.toEqual(ss1.ansi);
  }, 20000);

  it("ctrl+c quits", async () => {
    await startApp();
    session.press("ctrl+c");
    await session.waitForAbsent("Reminders", 5000);
  }, 20000);

  it("overlay form fields render cleanly without bleed-through", async () => {
    await startApp(25, 80);

    session.type("n");
    await session.waitForText("New Reminder", 5000);

    const ss = session.screenshot();
    // The Title field should render as "▸ Title: █" — no garbled characters
    const titleLine = ss.lines.find(l => l.includes("Title:"));
    expect(titleLine).toBeDefined();
    expect(titleLine).toMatch(/▸ Title:/);
    // No stray characters between ▸ and Title
    expect(titleLine).not.toMatch(/▸\S+Title/);

    session.press("escape");
    await session.waitForAbsent("New Reminder", 5000);
  }, 20000);

  it("space toggles the visually selected item", async () => {
    await startApp();

    // The first item should have ▸ indicator
    let ss = session.screenshot();
    const selLine = ss.lines.find(l => l.includes("\u25b8"));
    expect(selLine).toBeDefined();

    // Extract the title of the selected item (text after the indicator + checkbox + priority)
    const selTitle = selLine!.replace(/.*[□■][^a-zA-Z]*/, "").replace(/\s+\d{4}.*/, "").trim();
    expect(selTitle.length).toBeGreaterThan(0);

    // Toggle it
    session.press("space");
    await new Promise(r => setTimeout(r, 500));

    ss = session.screenshot();
    // The toggled item should now appear in the Completed section
    expect(ss.text).toContain("Completed");
    // The item title should still exist on screen
    expect(ss.text).toContain(selTitle);
  }, 20000);
});
