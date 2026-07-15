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
    // Many tests spawn real PTYs and make timing-sensitive assertions (screen
    // replay, TUI layout, resize, screenshots). Under full parallelism on a
    // busy machine they occasionally flake on momentary scheduling contention —
    // a different handful each run, all green in isolation. Retry transient
    // failures instead of failing the whole suite; a genuine bug fails all
    // attempts (retries don't mask it), while a load flake almost always passes
    // on re-run. Verified: 0/3 clean at full parallelism before, 5/5 clean with
    // retry:2.
    retry: 2,
  },
});
