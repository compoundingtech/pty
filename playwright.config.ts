import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./tests",
  testMatch: "web.test.ts",
  timeout: 30000,
  use: { headless: true },
  projects: [{ name: "chromium", use: { browserName: "chromium" } }],
});
