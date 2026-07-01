import { describe, it, expect, afterAll } from "vitest";
import * as net from "node:net";
import * as fs from "node:fs";
import * as os from "node:os";
import { Session } from "../src/testing/index.ts";
import {
  MessageType,
  PacketReader,
  encodeAttach,
  encodePeek,
} from "../src/protocol.ts";
import { getSocketPath } from "../src/sessions.ts";

// Isolate from real session directory.
const testSessionDir = fs.mkdtempSync(os.tmpdir() + "/pty-altscreen-");
process.env.PTY_SESSION_DIR = testSessionDir;
afterAll(() => {
  return new Promise((resolve) => {
    setTimeout(() => {
      try { fs.rmSync(testSessionDir, { recursive: true, force: true }); } catch {}
      resolve(undefined);
    }, 500);
  });
});

// Capture the raw SCREEN payload sent in response to `packet` on a fresh
// socket to `session`. Returns the payload string. Waits up to 3s.
async function captureScreen(sessionName: string, requestPacket: Buffer): Promise<string> {
  const sock = net.createConnection(getSocketPath(sessionName));
  const reader = new PacketReader();
  try {
    await new Promise<void>((resolve, reject) => {
      sock.once("connect", () => resolve());
      sock.once("error", reject);
    });
    const payload = await new Promise<string>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("timeout waiting for SCREEN")), 3000);
      sock.on("data", (data: Buffer) => {
        let packets;
        try { packets = reader.feed(data); } catch (err: any) {
          clearTimeout(timer);
          reject(err);
          return;
        }
        for (const p of packets) {
          if (p.type === MessageType.SCREEN) {
            clearTimeout(timer);
            resolve(p.payload.toString());
            return;
          }
        }
      });
      sock.write(requestPacket);
    });
    return payload;
  } finally {
    sock.destroy();
  }
}

describe("SCREEN replay carries alt-screen mode", () => {
  it("ATTACH prefixes ?1049h at position 0 when child is in the alternate screen buffer", async () => {
    // Child enters alt-screen, writes a marker, then blocks.
    const session = await Session.server(
      "sh",
      ["-c", "printf '\\033[?1049h\\033[Halt-marker'; sleep 60"],
      { rows: 24, cols: 80 },
    );
    // Wait for the daemon's xterm-headless to process the ?1049h and content.
    await new Promise((r) => setTimeout(r, 300));

    const screen = await captureScreen(session.name, encodeAttach(24, 80));

    // The alt-screen enter mode must be the very first bytes so the client's
    // real terminal switches buffers before any content is painted. Position
    // 0 is a stronger check than "contains" because xterm-addon-serialize may
    // independently emit `?1049h` mid-payload; this asserts the guarantee THIS
    // patch adds — the mode is set at the start, deterministically.
    expect(screen.startsWith("\x1b[?1049h")).toBe(true);
    expect(screen).toContain("alt-marker");

    await session.close();
  }, 15000);

  it("ATTACH does not prefix ?1049h when child is in the main screen buffer", async () => {
    const session = await Session.server(
      "sh",
      ["-c", "printf 'main-only'; sleep 60"],
      { rows: 24, cols: 80 },
    );
    await new Promise((r) => setTimeout(r, 300));

    const screen = await captureScreen(session.name, encodeAttach(24, 80));

    // The prefix guarantee is position-0. Main-screen sessions must not have
    // ?1049h at the start (would poison the client's host terminal).
    expect(screen.startsWith("\x1b[?1049h")).toBe(false);

    await session.close();
  }, 15000);

  it("ATTACH stops prefixing ?1049h after child exits alt-screen", async () => {
    // Child enters and immediately exits alt-screen, then writes to main.
    const session = await Session.server(
      "sh",
      ["-c", "printf '\\033[?1049h\\033[?1049lmain-again'; sleep 60"],
      { rows: 24, cols: 80 },
    );
    await new Promise((r) => setTimeout(r, 300));

    const screen = await captureScreen(session.name, encodeAttach(24, 80));

    // Buffer is back to main — no position-0 alt-screen prefix.
    expect(screen.startsWith("\x1b[?1049h")).toBe(false);

    await session.close();
  }, 15000);

  // Note on PEEK: we do NOT test that PEEK omits ?1049h. xterm-addon-serialize
  // independently emits `\x1b[?1049h` at the start of its output when the
  // buffer is alternate, and that's pre-existing behavior outside this patch's
  // scope. This patch's contract is `getModePrefix(includeAltScreen)` — only
  // ATTACH passes `true`, so this patch adds nothing to PEEK either way.
  // Whether peek's client-side TERMINAL_SANITIZE round-trip loses alt-content
  // is a separate concern tracked outside #41.

  it("tracks ?1047 (legacy variant) as alt-screen too", async () => {
    const session = await Session.server(
      "sh",
      ["-c", "printf '\\033[?1047h\\033[Halt-1047'; sleep 60"],
      { rows: 24, cols: 80 },
    );
    await new Promise((r) => setTimeout(r, 300));

    const screen = await captureScreen(session.name, encodeAttach(24, 80));

    // Server normalizes to ?1049h on emit (the modern combined form) —
    // callers get consistent semantics regardless of which variant the
    // child used to enter.
    expect(screen.startsWith("\x1b[?1049h")).toBe(true);

    await session.close();
  }, 15000);
});
