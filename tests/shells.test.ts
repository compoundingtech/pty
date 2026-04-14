import { describe, it, expect, afterAll } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import { Session } from "../src/testing/index.ts";

// Shell integration tests — verify that common shells start up,
// accept input, and produce output when run inside a pty session.
//
// bash and zsh use Session.spawn() (direct PTY, no server).
// fish uses Session.server() (PtyServer) to exercise the DA1 response
// handler — fish 4.x sends DA1 at startup and blocks 10s without it.
//
// Set PTY_SESSION_DIR to an isolated temp directory so PtyServer
// doesn't pollute real sessions. Vitest runs each file in its own
// worker, so this doesn't affect other test files.

const testSessionDir = fs.mkdtempSync(os.tmpdir() + "/pty-shells-");
process.env.PTY_SESSION_DIR = testSessionDir;

afterAll(() => {
  // Allow async exit metadata writes to complete before cleanup
  return new Promise((resolve) => {
    setTimeout(() => {
      try { fs.rmSync(testSessionDir, { recursive: true, force: true }); } catch {}
      resolve(undefined);
    }, 500);
  });
});

describe("shell integration", () => {
  describe("bash", () => {
    it("starts up and accepts commands", async () => {
      const session = Session.spawn("bash", ["--norc", "--noprofile"], {
        rows: 24,
        cols: 80,
      });

      await session.waitForText("$", 5000);

      session.type("echo hello-bash\r");
      await session.waitForText("hello-bash", 5000);

      session.press("ctrl+d");
      await session.close();
    }, 15000);
  });

  describe("zsh", () => {
    it("starts up and accepts commands", async () => {
      const session = Session.spawn("zsh", ["--no-rcs"], {
        rows: 24,
        cols: 80,
        env: { PROMPT: "zsh> " },
      });

      await session.waitForText("zsh>", 5000);

      session.type("echo hello-zsh\r");
      await session.waitForText("hello-zsh", 5000);

      session.press("ctrl+d");
      await session.close();
    }, 15000);
  });

  describe("fish", () => {
    it("starts up within 3 seconds (requires DA1 response)", async () => {
      // fish 4.x sends a DA1 query (ESC[c) at startup and blocks for up to
      // 10s waiting for a terminal response. PtyServer must respond to DA1
      // for fish to start promptly. This test uses Session.server() to go
      // through PtyServer.
      const session = await Session.server("fish", ["--no-config"], {
        rows: 24,
        cols: 80,
      });
      await session.attach();

      // If DA1 is not handled, this will timeout at 3s
      await session.waitForText(">", 3000);

      session.type("echo hello-fish\r");
      await session.waitForText("hello-fish", 5000);

      session.type("exit\r");
      await session.close();
    }, 15000);
  });
});
