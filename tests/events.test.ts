import { describe, it, expect, afterAll, beforeEach } from "vitest";
import * as net from "node:net";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { PtyServer, type ServerOptions } from "../src/server.ts";
import {
  MessageType,
  PacketReader,
  encodeAttach,
  encodeData,
} from "../src/protocol.ts";
import { getSocketPath, getEventsPath, cleanupAll } from "../src/sessions.ts";
import {
  EventWriter,
  clearEvents,
  readRecentEvents,
  EventFollower,
  formatEvent,
  type EventRecord,
} from "../src/events.ts";

const testCwd = fs.mkdtempSync(path.join(os.tmpdir(), "pty-ev-"));
const testSessionDir = fs.mkdtempSync(path.join(os.tmpdir(), "pty-ev-sd-"));
process.env.PTY_SESSION_DIR = testSessionDir;

afterAll(() => {
  fs.rmSync(testCwd, { recursive: true, force: true });
  fs.rmSync(testSessionDir, {
    recursive: true,
    force: true,
    maxRetries: 3,
    retryDelay: 100,
  });
});

let servers: PtyServer[] = [];
let sessionNames: string[] = [];

function uniqueName(): string {
  const name = `ev-${process.pid}-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`;
  sessionNames.push(name);
  return name;
}

async function startServer(
  name: string,
  command: string,
  args: string[] = [],
  opts: Partial<ServerOptions> = {}
): Promise<PtyServer> {
  const server = new PtyServer({
    name,
    command,
    args,
    displayCommand: command,
    cwd: testCwd,
    rows: 24,
    cols: 80,
    ...opts,
  });
  servers.push(server);
  await server.ready;
  return server;
}

function connect(name: string): Promise<net.Socket> {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection(getSocketPath(name));
    socket.on("connect", () => resolve(socket));
    socket.on("error", reject);
  });
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Wait for the events file to contain at least `count` events. */
async function waitForEvents(
  name: string,
  count: number,
  timeoutMs = 5000
): Promise<EventRecord[]> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const events = readRecentEvents(name, 1000);
    if (events.length >= count) return events;
    await sleep(50);
  }
  return readRecentEvents(name, 1000);
}

afterAll(async () => {
  for (const server of servers) {
    try {
      await server.close();
    } catch {}
  }
  for (const name of sessionNames) {
    try {
      cleanupAll(name);
    } catch {}
  }
});

describe("EventWriter", () => {
  it("appends events as JSONL lines", async () => {
    const name = uniqueName();
    clearEvents(name);
    const writer = new EventWriter(name);

    writer.append({
      session: name,
      type: "bell",
      ts: "2026-04-05T00:00:00.000Z",
    });
    writer.append({
      session: name,
      type: "bell",
      ts: "2026-04-05T00:00:01.000Z",
    });
    await writer.flush();

    const events = readRecentEvents(name);
    expect(events).toHaveLength(2);
    expect(events[0].type).toBe("bell");
    expect(events[1].ts).toBe("2026-04-05T00:00:01.000Z");
  });

  it("truncates when exceeding MAX_LINES", async () => {
    const name = uniqueName();
    clearEvents(name);
    const writer = new EventWriter(name);

    // Write 1050 events — should trigger truncation at the 100th append
    // (TRUNCATE_CHECK_INTERVAL), keeping 500 lines
    for (let i = 0; i < 1050; i++) {
      writer.append({
        session: name,
        type: "bell",
        ts: `2026-04-05T00:00:${String(i).padStart(4, "0")}Z`,
      });
    }
    await writer.flush();

    const content = fs
      .readFileSync(getEventsPath(name), "utf-8")
      .trimEnd()
      .split("\n")
      .filter((l) => l.length > 0);
    // After truncation at 1000+ lines, should keep 500,
    // then the remaining appends add more, so total should be <= ~600
    expect(content.length).toBeLessThanOrEqual(650);
    expect(content.length).toBeGreaterThan(0);

    // The last event should be preserved
    const last = JSON.parse(content[content.length - 1]);
    expect(last.ts).toBe("2026-04-05T00:00:1049Z");
  });
});

describe("readRecentEvents", () => {
  it("returns last N events", () => {
    const name = uniqueName();
    clearEvents(name);
    const lines: string[] = [];
    for (let i = 0; i < 10; i++) {
      lines.push(
        JSON.stringify({
          session: name,
          type: "bell",
          ts: `2026-04-05T00:00:0${i}Z`,
        })
      );
    }
    fs.writeFileSync(getEventsPath(name), lines.join("\n") + "\n");

    const events = readRecentEvents(name, 3);
    expect(events).toHaveLength(3);
    expect(events[0].ts).toBe("2026-04-05T00:00:07Z");
    expect(events[2].ts).toBe("2026-04-05T00:00:09Z");
  });

  it("returns empty array for missing file", () => {
    const events = readRecentEvents("nonexistent-session-xyz");
    expect(events).toEqual([]);
  });
});

describe("clearEvents", () => {
  it("creates an empty file", () => {
    const name = uniqueName();
    clearEvents(name);
    const content = fs.readFileSync(getEventsPath(name), "utf-8");
    expect(content).toBe("");
  });
});

describe("cleanupAll removes events file", () => {
  it("removes .events.jsonl", () => {
    const name = uniqueName();
    clearEvents(name);
    fs.writeFileSync(
      getEventsPath(name),
      JSON.stringify({ session: name, type: "bell", ts: "t" }) + "\n"
    );
    expect(fs.existsSync(getEventsPath(name))).toBe(true);

    cleanupAll(name);
    expect(fs.existsSync(getEventsPath(name))).toBe(false);
  });
});

describe("formatEvent", () => {
  it("formats bell", () => {
    const result = formatEvent({
      session: "test",
      type: "bell",
      ts: "2026-04-05T10:15:03.000Z",
    });
    expect(result).toContain("test:");
    expect(result).toContain("bell");
  });

  it("formats title change", () => {
    const result = formatEvent({
      session: "test",
      type: "title_change",
      ts: "2026-04-05T10:15:03.000Z",
      value: "Building...",
    });
    expect(result).toContain('title -> "Building..."');
  });

  it("formats notification with title and body", () => {
    const result = formatEvent({
      session: "test",
      type: "notification",
      ts: "2026-04-05T10:15:03.000Z",
      title: "Done",
      body: "Build succeeded",
      source: "osc9",
    });
    expect(result).toContain('-- "Done"');
    expect(result).toContain("Build succeeded");
  });

  it("formats focus request", () => {
    const result = formatEvent({
      session: "test",
      type: "focus_request",
      ts: "2026-04-05T10:15:03.000Z",
    });
    expect(result).toContain("focus requested");
  });

  it("formats cursor visible", () => {
    const result = formatEvent({
      session: "test",
      type: "cursor_visible",
      ts: "2026-04-05T10:15:03.000Z",
    });
    expect(result).toContain("cursor restored");
  });
});

describe("EventFollower", () => {
  it("follows new events appended to a file", async () => {
    const name = uniqueName();
    clearEvents(name);

    const received: EventRecord[] = [];
    const follower = new EventFollower({
      names: [name],
      onEvent: (event) => received.push(event),
    });
    follower.start();

    // Give fs.watch a moment to initialize
    await sleep(100);

    // Append events
    const writer = new EventWriter(name);
    writer.append({
      session: name,
      type: "bell",
      ts: "2026-04-05T00:00:00Z",
    });
    writer.append({
      session: name,
      type: "bell",
      ts: "2026-04-05T00:00:01Z",
    });
    await writer.flush();

    // Wait for fs.watch to pick up the changes
    await sleep(300);

    follower.stop();

    expect(received.length).toBeGreaterThanOrEqual(2);
    expect(received[0].type).toBe("bell");
  });

  it("dirWatcher-detected new files start at offset 0 so session_start is not skipped", async () => {
    const name = uniqueName();
    // Make sure the file does not exist yet
    try { fs.unlinkSync(getEventsPath(name)); } catch {}

    // Start a follower with no specific names — it uses scanAndWatchAll and
    // relies on dirWatcher to pick up new files.
    const received: EventRecord[] = [];
    const follower = new EventFollower({
      onEvent: (event) => { if (event.session === name) received.push(event); },
    });
    follower.start();
    await sleep(100);

    // Create the file with a session_start already present. In the real
    // daemon, session_start is appended immediately on socket listen, and
    // the dirWatcher usually fires *after* that line is on disk.
    const writer = new EventWriter(name);
    writer.append({
      session: name,
      type: "session_start",
      ts: "2026-04-05T00:00:00Z",
    });
    await writer.flush();

    // Wait for dirWatcher + readNewLines
    await sleep(500);
    follower.stop();

    const starts = received.filter((e) => e.type === "session_start");
    expect(starts.length).toBe(1);
  });

  it("handles file truncation gracefully", async () => {
    const name = uniqueName();
    clearEvents(name);

    // Pre-fill the file so the follower starts at a non-zero offset
    const line =
      JSON.stringify({
        session: name,
        type: "bell",
        ts: "2026-04-05T00:00:00Z",
      }) + "\n";
    fs.writeFileSync(getEventsPath(name), line.repeat(5));

    const received: EventRecord[] = [];
    const follower = new EventFollower({
      names: [name],
      onEvent: (event) => received.push(event),
    });
    follower.start();
    await sleep(100);

    // Truncate the file (simulating the writer's truncation)
    fs.writeFileSync(getEventsPath(name), "");
    await sleep(100);

    // Write new events after truncation
    const writer = new EventWriter(name);
    writer.append({
      session: name,
      type: "notification",
      ts: "2026-04-05T00:01:00Z",
      title: "After truncation",
      source: "osc9",
    });
    await writer.flush();
    await sleep(300);

    follower.stop();

    const notifications = received.filter((e) => e.type === "notification");
    expect(notifications.length).toBeGreaterThanOrEqual(1);
  });
});

describe("PtyServer event detection", () => {
  it("logs bell events", async () => {
    const name = uniqueName();
    await startServer(name, "sh", []);

    const socket = await connect(name);
    const reader = new PacketReader();

    // Attach
    socket.write(encodeAttach(24, 80));
    await sleep(200);

    // Send a bell character
    socket.write(encodeData("printf '\\a'\n"));
    const events = await waitForEvents(name, 2);
    socket.destroy();

    const bells = events.filter((e) => e.type === "bell");
    expect(bells.length).toBeGreaterThanOrEqual(1);
    expect(bells[0].session).toBe(name);
  });

  it("logs title change events", async () => {
    const name = uniqueName();
    await startServer(name, "sh", []);

    const socket = await connect(name);
    socket.write(encodeAttach(24, 80));
    await sleep(200);

    // Set the terminal title via OSC 0
    socket.write(encodeData("printf '\\033]0;My Custom Title\\a'\n"));
    const events = await waitForEvents(name, 2);
    socket.destroy();

    const titles = events.filter((e) => e.type === "title_change");
    expect(titles.length).toBeGreaterThanOrEqual(1);
    expect((titles[0] as any).value).toBe("My Custom Title");
  });

  it("deduplicates identical title changes", async () => {
    const name = uniqueName();
    await startServer(name, "sh", []);

    const socket = await connect(name);
    socket.write(encodeAttach(24, 80));
    await sleep(200);

    // Set the same title twice
    socket.write(encodeData("printf '\\033]0;Same Title\\a'\n"));
    await sleep(200);
    socket.write(encodeData("printf '\\033]0;Same Title\\a'\n"));
    await sleep(200);

    // Set a different title
    socket.write(encodeData("printf '\\033]0;Different Title\\a'\n"));
    await sleep(300);

    const events = readRecentEvents(name, 1000);
    socket.destroy();

    const titles = events.filter((e) => e.type === "title_change");
    const titleValues = titles.map((e) => (e as any).value);

    // "Same Title" should appear only once, "Different Title" should also appear
    expect(
      titleValues.filter((v: string) => v === "Same Title").length
    ).toBe(1);
    expect(titleValues).toContain("Different Title");
  });

  it("logs OSC 9 notification events", async () => {
    const name = uniqueName();
    await startServer(name, "sh", []);

    const socket = await connect(name);
    socket.write(encodeAttach(24, 80));
    await sleep(200);

    // Send an iTerm2-style notification
    socket.write(encodeData("printf '\\033]9;Build complete\\a'\n"));
    const events = await waitForEvents(name, 2);
    socket.destroy();

    const notifs = events.filter((e) => e.type === "notification");
    expect(notifs.length).toBeGreaterThanOrEqual(1);
    expect((notifs[0] as any).body).toBe("Build complete");
    expect((notifs[0] as any).source).toBe("osc9");
  });

  it("logs OSC 777 notification events", async () => {
    const name = uniqueName();
    await startServer(name, "sh", []);

    const socket = await connect(name);
    socket.write(encodeAttach(24, 80));
    await sleep(200);

    // Send an rxvt-style notification
    socket.write(
      encodeData("printf '\\033]777;notify;Build;All tests passed\\a'\n")
    );
    const events = await waitForEvents(name, 2);
    socket.destroy();

    const notifs = events.filter((e) => e.type === "notification");
    expect(notifs.length).toBeGreaterThanOrEqual(1);
    expect((notifs[0] as any).title).toBe("Build");
    expect((notifs[0] as any).body).toBe("All tests passed");
    expect((notifs[0] as any).source).toBe("osc777");
  });

  it("clears events file on session start", async () => {
    const name = uniqueName();

    // Write some stale events
    clearEvents(name);
    fs.appendFileSync(
      getEventsPath(name),
      JSON.stringify({ session: name, type: "bell", ts: "old" }) + "\n"
    );
    expect(readRecentEvents(name)).toHaveLength(1);

    // Starting a new server should clear the events
    await startServer(name, "sh", []);
    await sleep(100);
    const events = readRecentEvents(name);
    // Should have no stale events (may have new ones from shell startup)
    const stale = events.filter((e) => e.ts === "old");
    expect(stale).toHaveLength(0);
  });
});
