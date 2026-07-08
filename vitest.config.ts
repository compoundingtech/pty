import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    exclude: [
      "node_modules/**",
    ],
    globalSetup: ["./tests/setup/vitest-global.ts"],
    // Runs once per worker before any test module — scrubs ambient
    // PTY_ROOT/PTY_SESSION/PTY_SESSION_DIR so a suite launched from inside a
    // pty session can't leak the real live session dir into spawned `pty`s.
    setupFiles: ["./tests/setup/isolate-env.ts"],
  },
});
