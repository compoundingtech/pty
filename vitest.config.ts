import { defineConfig } from "vitest/config";

// Heavy, timing-sensitive real-PTY tests. Each spawns real PTYs and/or drives an
// xterm and asserts layout / screen-replay / resize / screenshot timing. They
// pass in isolation but flake under sustained parallel load (many PTYs spawned
// at once starve each other's scheduling) — the "green in CI, red under real
// load" class that erodes trust. Run them in a NON-PARALLEL project so at most
// one heavy PTY test runs at a time; the rest of the suite stays fully parallel.
const HEAVY_PTY_TESTS = [
  "demos/agent-teams/agents-integration.test.ts",
  "tests/tui.test.ts",
  "tests/scrollback-fidelity.test.ts",
  "tests/integration.test.ts",
  "tests/screenshot.test.ts",
  "tests/resize-tui.test.ts",
  "tests/remote-fabric.test.ts",
  "tests/remote-reconnect.test.ts",
];

// Runs once per worker before any test module — scrubs ambient
// PTY_ROOT/PTY_SESSION/PTY_SESSION_DIR so a suite launched from inside a pty
// session can't leak the real live session dir into spawned `pty`s.
const setupFiles = ["./tests/setup/isolate-env.ts"];

// Timing flakes still get retried (a genuine bug fails all attempts). Serializing
// the heavy tests removes most contention; retry mops up any residual jitter.
const retry = 2;

export default defineConfig({
  test: {
    // Once for the whole run — creates the isolated tmp run root. See the file.
    globalSetup: ["./tests/setup/vitest-global.ts"],
    projects: [
      {
        test: {
          name: "unit",
          include: ["tests/**/*.test.ts", "demos/**/*.test.ts"],
          exclude: ["node_modules/**", ...HEAVY_PTY_TESTS],
          setupFiles,
          retry,
        },
      },
      {
        test: {
          name: "pty",
          include: HEAVY_PTY_TESTS,
          setupFiles,
          retry,
          // Serialize: run these files one at a time so concurrent real-PTY
          // spawns can't starve each other's timing.
          fileParallelism: false,
        },
      },
    ],
  },
});
