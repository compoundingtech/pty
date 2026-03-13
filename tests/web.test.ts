import { test, expect } from "@playwright/test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { PtyServer } from "../src/server.ts";
import { WebServer } from "../src/web/server.ts";
import { cleanupAll } from "../src/sessions.ts";

// Each test gets isolated temp directories
let testCwd: string;
let testSessionDir: string;
let servers: PtyServer[] = [];
let webServers: WebServer[] = [];
let sessionNames: string[] = [];

function uniqueName(): string {
  const name = `web-${process.pid}-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`;
  sessionNames.push(name);
  return name;
}

async function startPty(
  name: string,
  command: string,
  args: string[] = []
): Promise<PtyServer> {
  const server = new PtyServer({
    name,
    command,
    args,
    displayCommand: command,
    cwd: testCwd,
    rows: 24,
    cols: 80,
  });
  servers.push(server);
  await server.ready;
  return server;
}

async function startWeb(
  opts: { connectCode?: string } = {}
): Promise<{ server: WebServer; url: string }> {
  const server = new WebServer({ port: 0, ...opts });
  webServers.push(server);
  const addr = await server.ready;
  return { server, url: `http://${addr.host}:${addr.port}` };
}

test.beforeEach(() => {
  testCwd = fs.mkdtempSync(path.join(os.tmpdir(), "pty-web-"));
  testSessionDir = fs.mkdtempSync(path.join(os.tmpdir(), "pty-web-state-"));
  process.env.PTY_SESSION_DIR = testSessionDir;
});

test.afterEach(async () => {
  for (const s of webServers) await s.close();
  webServers = [];
  for (const s of servers) await s.close();
  servers = [];
  for (const name of sessionNames) cleanupAll(name);
  sessionNames = [];
  fs.rmSync(testCwd, { recursive: true, force: true });
  fs.rmSync(testSessionDir, { recursive: true, force: true });
});

test("session list renders", async ({ page }) => {
  const name = uniqueName();
  await startPty(name, "cat");
  const { url } = await startWeb();

  await page.goto(url);
  await expect(page.locator(".session-item")).toBeVisible({ timeout: 5000 });
  await expect(page.locator(".session-name")).toHaveText(name);
});

test("connect and see output", async ({ page }) => {
  const name = uniqueName();
  await startPty(name, "sh", ["-c", "echo hello; sleep 30"]);
  // Wait for xterm-headless to process
  await new Promise((r) => setTimeout(r, 200));
  const { url } = await startWeb();

  await page.goto(url);
  await page.locator(".session-item").click();

  // Wait for terminal to contain the output
  await expect(page.locator(".xterm-rows")).toContainText("hello", {
    timeout: 5000,
  });
});

test("send input", async ({ page }) => {
  const name = uniqueName();
  await startPty(name, "cat");
  const { url } = await startWeb();

  await page.goto(url);
  await page.locator(".session-item").click();

  // Wait for terminal to be ready
  await expect(page.locator(".xterm-rows")).toBeVisible({ timeout: 5000 });
  await new Promise((r) => setTimeout(r, 300));

  // Click the terminal to focus, then type via keyboard
  await page.locator("#terminal-container").click();
  await page.keyboard.type("test123");

  // Should see echoed text (cat echoes input)
  await expect(page.locator(".xterm-rows")).toContainText("test123", {
    timeout: 5000,
  });
});

test("detach button returns to list", async ({ page }) => {
  const name = uniqueName();
  await startPty(name, "cat");
  const { url } = await startWeb();

  await page.goto(url);
  await page.locator(".session-item").click();
  await expect(page.locator("#terminal-view")).toBeVisible({ timeout: 5000 });

  await page.locator("#detach-btn").click();
  await expect(page.locator("#list-view")).toBeVisible({ timeout: 5000 });
});

test("auth required with connect code", async ({ page }) => {
  const name = uniqueName();
  await startPty(name, "cat");
  const { url } = await startWeb({ connectCode: "123456" });

  await page.goto(url);
  await expect(page.locator("#login-view")).toBeVisible({ timeout: 5000 });

  // Wrong code
  await page.locator("#code-input").fill("wrong");
  await page.locator("#login-btn").click();
  await expect(page.locator("#login-error")).toBeVisible({ timeout: 5000 });

  // Correct code
  await page.locator("#code-input").fill("123456");
  await page.locator("#login-btn").click();
  await expect(page.locator("#list-view")).toBeVisible({ timeout: 5000 });
});

test("numeric code shows numeric keyboard", async ({ page }) => {
  const name = uniqueName();
  await startPty(name, "cat");
  const { url } = await startWeb({ connectCode: "9999" });

  await page.goto(url);
  await expect(page.locator("#login-view")).toBeVisible({ timeout: 5000 });
  // After the first session fetch returns 401 with numeric=true,
  // the input should have inputmode="numeric"
  await expect(page.locator("#code-input")).toHaveAttribute(
    "inputmode",
    "numeric",
    { timeout: 3000 }
  );
});

test("no auth when no code", async ({ page }) => {
  const name = uniqueName();
  await startPty(name, "cat");
  const { url } = await startWeb();

  await page.goto(url);
  // Should show list directly (no login form)
  await expect(page.locator("#list-view")).toBeVisible({ timeout: 5000 });
});

test("session exit displayed", async ({ page }) => {
  const name = uniqueName();
  // Process that lives long enough for the browser to attach, then exits
  await startPty(name, "sh", ["-c", "echo goodbye; sleep 0.5; exit 42"]);
  const { url } = await startWeb();

  await page.goto(url);
  await page.locator(".session-item").click();

  // Should see exit overlay after the process exits
  await expect(page.locator("#exit-overlay")).toContainText("exited", {
    timeout: 10000,
  });
});

test("auto-refresh shows new session", async ({ page }) => {
  const { url } = await startWeb();
  await page.goto(url);
  await expect(page.locator("#list-view")).toBeVisible({ timeout: 5000 });

  // No sessions yet
  await expect(page.locator("#no-sessions")).toBeVisible();

  // Start a session after the page loaded
  const name = uniqueName();
  await startPty(name, "cat");

  // Should appear within the refresh interval (3s + margin)
  await expect(page.locator(".session-name")).toHaveText(name, {
    timeout: 6000,
  });
});

test("multiple sessions listed", async ({ page }) => {
  const name1 = uniqueName();
  const name2 = uniqueName();
  await startPty(name1, "cat");
  await startPty(name2, "cat");
  const { url } = await startWeb();

  await page.goto(url);
  await expect(page.locator(".session-item")).toHaveCount(2, { timeout: 5000 });
});
