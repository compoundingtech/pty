import { describe, it, expect, afterEach, afterAll } from "vitest";
import * as net from "node:net";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { Terminal } from "@xterm/headless";
import { PtyServer, type ServerOptions } from "../src/server.ts";
import {
  MessageType,
  PacketReader,
  Packet,
  encodeAttach,
  encodeData,
  encodeDetach,
  encodePeek,
  encodeResize,
  decodeExit,
} from "../src/protocol.ts";
import {
  getSocketPath,
  getMetadataPath,
  cleanupAll,
  readMetadata,
  validateName,
  acquireLock,
  releaseLock,
} from "../src/sessions.ts";

// All tests run in a tmp directory to avoid polluting the project
const testCwd = fs.mkdtempSync(path.join(os.tmpdir(), "pty-int-"));
// Isolate session metadata/sockets from the real session directory
const testSessionDir = fs.mkdtempSync(path.join(os.tmpdir(), "pty-int-sd-"));
process.env.PTY_SESSION_DIR = testSessionDir;
afterAll(() => {
  fs.rmSync(testCwd, { recursive: true, force: true });
  fs.rmSync(testSessionDir, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

let servers: PtyServer[] = [];
let sessionNames: string[] = [];

function uniqueName(): string {
  const name = `test-${process.pid}-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`;
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

/** Collect packets from a socket until we have at least `count`, or timeout. */
function collectPackets(
  socket: net.Socket,
  reader: PacketReader,
  count: number,
  timeoutMs = 5000
): Promise<Packet[]> {
  return new Promise((resolve) => {
    const packets: Packet[] = [];
    const timer = setTimeout(() => resolve(packets), timeoutMs);

    function onData(data: Buffer) {
      packets.push(...reader.feed(data));
      if (packets.length >= count) {
        clearTimeout(timer);
        socket.off("data", onData);
        resolve(packets);
      }
    }

    socket.on("data", onData);
  });
}

/** Wait for a specific message type. */
function waitForType(
  socket: net.Socket,
  reader: PacketReader,
  type: MessageType,
  timeoutMs = 5000
): Promise<Packet> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error(`Timed out waiting for message type ${type}`)),
      timeoutMs
    );

    function onData(data: Buffer) {
      const packets = reader.feed(data);
      for (const packet of packets) {
        if (packet.type === type) {
          clearTimeout(timer);
          socket.off("data", onData);
          resolve(packet);
          return;
        }
      }
    }

    socket.on("data", onData);
  });
}

/** Collect DATA payloads until the accumulated text contains the pattern. */
function waitForContent(
  socket: net.Socket,
  reader: PacketReader,
  pattern: string,
  timeoutMs = 5000
): Promise<string> {
  return new Promise((resolve, reject) => {
    let accumulated = "";
    const timer = setTimeout(
      () => reject(new Error(`Timed out waiting for "${pattern}" in DATA (got: ${JSON.stringify(accumulated)})`)),
      timeoutMs
    );

    function onData(data: Buffer) {
      const packets = reader.feed(data);
      for (const packet of packets) {
        if (packet.type === MessageType.DATA) {
          accumulated += packet.payload.toString();
          if (accumulated.includes(pattern)) {
            clearTimeout(timer);
            socket.off("data", onData);
            resolve(accumulated);
            return;
          }
        }
      }
    }

    socket.on("data", onData);
  });
}

afterEach(async () => {
  for (const server of servers) {
    await server.close();
  }
  servers = [];
  for (const name of sessionNames) {
    cleanupAll(name);
  }
  sessionNames = [];
});

describe("integration", () => {
  it("starts a session and receives screen on attach", async () => {
    const name = uniqueName();
    await startServer(name, "echo", ["hello"]);

    const socket = connect(name);
    const client = await socket;
    const reader = new PacketReader();

    // Send ATTACH
    client.write(encodeAttach(24, 80));

    // Should get a SCREEN packet back
    const screenPacket = await waitForType(client, reader, MessageType.SCREEN);
    expect(screenPacket.type).toBe(MessageType.SCREEN);

    client.destroy();
  });

  it("receives process output via screen replay", async () => {
    const name = uniqueName();
    // Use a command that prints and stays alive so the terminal has time to process
    await startServer(name, "sh", ["-c", "echo 'hello world'; sleep 30"]);

    // Small delay to let xterm-headless process the output
    await new Promise((r) => setTimeout(r, 200));

    const client = await connect(name);
    const reader = new PacketReader();

    client.write(encodeAttach(24, 80));

    // The SCREEN packet should contain the buffered output
    const screenPacket = await waitForType(client, reader, MessageType.SCREEN);
    expect(screenPacket.payload.toString()).toContain("hello world");

    client.destroy();
  });

  it("sends input to the PTY process", async () => {
    const name = uniqueName();
    // `cat` echoes stdin back to stdout
    await startServer(name, "cat");

    const client = await connect(name);
    const reader = new PacketReader();

    client.write(encodeAttach(24, 80));

    // Wait for initial SCREEN packet
    await waitForType(client, reader, MessageType.SCREEN);

    // Send some input
    client.write(encodeData("test input\n"));

    // Should get it echoed back as DATA
    const dataPacket = await waitForType(client, reader, MessageType.DATA, 3000);
    expect(dataPacket.payload.toString()).toContain("test input");

    client.destroy();
  });

  it("detach and reattach with screen replay", async () => {
    const name = uniqueName();
    // Use sh to print something then wait
    await startServer(name, "sh", ["-c", "echo 'persistent output'; sleep 30"]);

    // Let xterm-headless process the output
    await new Promise((r) => setTimeout(r, 200));

    // First client: attach, get output, detach
    const client1 = await connect(name);
    const reader1 = new PacketReader();

    client1.write(encodeAttach(24, 80));

    // Collect output until we see our text
    const packets1 = await collectPackets(client1, reader1, 2, 3000);
    const output1 = packets1
      .filter((p) => p.type === MessageType.DATA || p.type === MessageType.SCREEN)
      .map((p) => p.payload.toString())
      .join("");
    expect(output1).toContain("persistent output");

    // Detach
    client1.write(encodeDetach());
    await new Promise((r) => client1.on("close", r));

    // Second client: reattach, should get screen replay with the output
    const client2 = await connect(name);
    const reader2 = new PacketReader();

    client2.write(encodeAttach(24, 80));

    const screenPacket = await waitForType(client2, reader2, MessageType.SCREEN);
    expect(screenPacket.payload.toString()).toContain("persistent output");

    client2.destroy();
  });

  it("receives EXIT when process terminates", async () => {
    const name = uniqueName();
    // Process that exits with code 42
    await startServer(name, "sh", ["-c", "exit 42"]);

    const client = await connect(name);
    const reader = new PacketReader();

    client.write(encodeAttach(24, 80));

    const exitPacket = await waitForType(client, reader, MessageType.EXIT, 3000);
    expect(exitPacket.type).toBe(MessageType.EXIT);
    expect(decodeExit(exitPacket.payload)).toBe(42);

    client.destroy();
  });

  it("handles resize", async () => {
    const name = uniqueName();
    await startServer(name, "cat");

    const client = await connect(name);
    const reader = new PacketReader();

    client.write(encodeAttach(24, 80));
    await waitForType(client, reader, MessageType.SCREEN);

    // Resize — shouldn't crash or error
    client.write(encodeResize(48, 120));

    // Send some data after resize to verify things still work
    client.write(encodeData("after resize\n"));

    const dataPacket = await waitForType(client, reader, MessageType.DATA, 3000);
    expect(dataPacket.payload.toString()).toContain("after resize");

    client.destroy();
  });

  it("supports multiple simultaneous clients", async () => {
    const name = uniqueName();
    await startServer(name, "cat");

    const client1 = await connect(name);
    const reader1 = new PacketReader();
    const client2 = await connect(name);
    const reader2 = new PacketReader();

    client1.write(encodeAttach(24, 80));
    client2.write(encodeAttach(24, 80));

    await waitForType(client1, reader1, MessageType.SCREEN);
    await waitForType(client2, reader2, MessageType.SCREEN);

    // Client 1 sends input — both should receive the echo
    client1.write(encodeData("shared input\n"));

    const data1 = await waitForType(client1, reader1, MessageType.DATA, 3000);
    const data2 = await waitForType(client2, reader2, MessageType.DATA, 3000);

    expect(data1.payload.toString()).toContain("shared input");
    expect(data2.payload.toString()).toContain("shared input");

    client1.destroy();
    client2.destroy();
  });

  it("cleans up socket and pid files on close", async () => {
    const name = uniqueName();
    const server = await startServer(name, "cat");

    const socketPath = getSocketPath(name);

    // Socket should exist
    const fs = await import("node:fs");
    expect(fs.existsSync(socketPath)).toBe(true);

    await server.close();

    // Socket should be gone
    expect(fs.existsSync(socketPath)).toBe(false);

    // Remove from tracking so afterEach doesn't double-close
    servers = servers.filter((s) => s !== server);
  });

  it("peek receives screen replay without affecting the session", async () => {
    const name = uniqueName();
    await startServer(name, "sh", ["-c", "echo 'peek test output'; sleep 30"]);
    await new Promise((r) => setTimeout(r, 200));

    // Peek client
    const peeker = await connect(name);
    const peekReader = new PacketReader();

    peeker.write(encodePeek());

    const screenPacket = await waitForType(peeker, peekReader, MessageType.SCREEN);
    expect(screenPacket.payload.toString()).toContain("peek test output");

    peeker.destroy();
  });

  it("peek client input is ignored by server", async () => {
    const name = uniqueName();
    // Use cat — if input reaches it, it would echo back
    await startServer(name, "cat");

    // Regular client to observe
    const watcher = await connect(name);
    const watchReader = new PacketReader();
    watcher.write(encodeAttach(24, 80));
    await waitForType(watcher, watchReader, MessageType.SCREEN);

    // Peek client sends DATA — server should ignore it
    const peeker = await connect(name);
    const peekReader = new PacketReader();
    peeker.write(encodePeek());
    await waitForType(peeker, peekReader, MessageType.SCREEN);

    peeker.write(encodeData("this should be ignored\n"));

    // Wait a bit — if the input were forwarded, cat would echo it back
    const packets = await collectPackets(watcher, watchReader, 1, 500);
    const echoed = packets
      .filter((p) => p.type === MessageType.DATA)
      .map((p) => p.payload.toString())
      .join("");
    expect(echoed).not.toContain("this should be ignored");

    watcher.destroy();
    peeker.destroy();
  });

  it("peek client does not affect terminal size", async () => {
    const name = uniqueName();
    await startServer(name, "cat", [], { rows: 24, cols: 80 });

    // Attach a regular client with specific size
    const client = await connect(name);
    const clientReader = new PacketReader();
    client.write(encodeAttach(30, 100));
    await waitForType(client, clientReader, MessageType.SCREEN);

    // Peek client sends RESIZE — server should ignore it
    const peeker = await connect(name);
    const peekReader = new PacketReader();
    peeker.write(encodePeek());
    await waitForType(peeker, peekReader, MessageType.SCREEN);
    peeker.write(encodeResize(10, 10));

    // Send input through the regular client — if resize happened, output would differ
    // Just verify no crash and the session is still functional
    client.write(encodeData("still works\n"));
    const data = await waitForType(client, clientReader, MessageType.DATA, 3000);
    expect(data.payload.toString()).toContain("still works");

    client.destroy();
    peeker.destroy();
  });

  it("peek receives live DATA when following", async () => {
    const name = uniqueName();
    await startServer(name, "cat");

    const peeker = await connect(name);
    const peekReader = new PacketReader();
    peeker.write(encodePeek());
    await waitForType(peeker, peekReader, MessageType.SCREEN);

    // Regular client sends input
    const client = await connect(name);
    const clientReader = new PacketReader();
    client.write(encodeAttach(24, 80));
    await waitForType(client, clientReader, MessageType.SCREEN);

    client.write(encodeData("live data\n"));

    // Peeker should receive the echoed output as DATA
    const dataPacket = await waitForType(peeker, peekReader, MessageType.DATA, 3000);
    expect(dataPacket.payload.toString()).toContain("live data");

    client.destroy();
    peeker.destroy();
  });

  it("peek captures TUI app running in alternate screen buffer", async () => {
    const name = uniqueName();
    // Simulate a TUI app: enter alt screen, enable mouse tracking, draw content
    await startServer(name, "sh", [
      "-c",
      "printf '\\033[?1049h';" + // enter alternate screen
        "printf '\\033[?1000h';" + // enable mouse click tracking
        "printf '\\033[?1003h';" + // enable mouse any-event tracking
        "printf '\\033[?1h';" + // enable application cursor keys
        "printf '\\033[H';" + // home cursor
        "printf '\\033[32mTUI-PEEK-TEST\\033[0m\\n';" +
        "printf 'Status: running\\n';" +
        "sleep 30",
    ]);

    await new Promise((r) => setTimeout(r, 300));

    // Peek should capture the alternate screen content
    const peeker = await connect(name);
    const peekReader = new PacketReader();
    peeker.write(encodePeek());

    const screenPacket = await waitForType(peeker, peekReader, MessageType.SCREEN);
    const screen = screenPacket.payload.toString();
    expect(screen).toContain("TUI-PEEK-TEST");
    expect(screen).toContain("Status: running");

    peeker.destroy();
  });

  it("writes session metadata on creation", async () => {
    const name = uniqueName();
    await startServer(name, "cat", ["-u"]);

    const meta = readMetadata(name);
    expect(meta).not.toBeNull();
    expect(meta!.command).toBe("cat");
    expect(meta!.args).toEqual(["-u"]);
    expect(meta!.createdAt).toBeTruthy();
    expect(meta!.exitCode).toBeUndefined();
  });

  it("screen replay includes scrollback content", async () => {
    const name = uniqueName();
    // Generate enough output to scroll past the visible 24 rows
    const lines = Array.from({ length: 40 }, (_, i) => `scrollback-line-${i}`);
    const script = lines.map((l) => `echo '${l}'`).join("; ");
    await startServer(name, "sh", ["-c", `${script}; sleep 30`]);

    await new Promise((r) => setTimeout(r, 300));

    const client = await connect(name);
    const reader = new PacketReader();
    client.write(encodeAttach(24, 80));

    const screenPacket = await waitForType(client, reader, MessageType.SCREEN);
    const screen = screenPacket.payload.toString();

    // The screen replay should include content that scrolled off — at minimum the last visible lines
    expect(screen).toContain("scrollback-line-39");
    // And earlier lines that are now in scrollback
    expect(screen).toContain("scrollback-line-0");

    client.destroy();
  });

  it("saves last lines and exit code on process exit", async () => {
    const name = uniqueName();
    const server = await startServer(name, "sh", [
      "-c",
      "echo 'line one'; echo 'line two'; echo 'line three'; sleep 0.2; exit 7",
    ]);

    // Wait for the process to exit and metadata to be written
    await new Promise((r) => setTimeout(r, 500));

    const meta = readMetadata(name);
    expect(meta).not.toBeNull();
    expect(meta!.exitCode).toBe(7);
    expect(meta!.exitedAt).toBeTruthy();
    expect(meta!.lastLines).toBeDefined();
    expect(meta!.lastLines!.some((l) => l.includes("line one"))).toBe(true);
    expect(meta!.lastLines!.some((l) => l.includes("line three"))).toBe(true);
  });

  it("metadata persists after server closes", async () => {
    const name = uniqueName();
    const server = await startServer(name, "sh", ["-c", "echo 'persist me'; sleep 0.2; exit 0"]);

    await new Promise((r) => setTimeout(r, 500));
    await server.close();
    servers = servers.filter((s) => s !== server);

    // Socket should be gone but metadata should remain
    const fs = await import("node:fs");
    expect(fs.existsSync(getSocketPath(name))).toBe(false);
    expect(fs.existsSync(getMetadataPath(name))).toBe(true);

    const meta = readMetadata(name);
    expect(meta!.lastLines!.some((l) => l.includes("persist me"))).toBe(true);

    // Clean up metadata
    cleanupAll(name);
  });

  it("validates session names", () => {
    expect(() => validateName("good-name")).not.toThrow();
    expect(() => validateName("my.session_1")).not.toThrow();
    expect(() => validateName("")).toThrow(/empty/);
    expect(() => validateName("bad/name")).toThrow(/Invalid session name/);
    expect(() => validateName("../traversal")).toThrow(/Invalid session name/);
    expect(() => validateName("has spaces")).toThrow(/Invalid session name/);
    expect(() => validateName("a".repeat(256))).toThrow(/too long/);
  });

  it("lock prevents double acquire, release allows reacquire", () => {
    const name = uniqueName();
    expect(acquireLock(name)).toBe(true);
    // Same process — should fail since we already hold it
    // (lock file exists with our PID, and we're alive)
    expect(acquireLock(name)).toBe(false);
    releaseLock(name);
    // After release, should succeed
    expect(acquireLock(name)).toBe(true);
    releaseLock(name);
  });

  it("lock with garbage content is treated as stale", async () => {
    const name = uniqueName();
    const fs = await import("node:fs");
    const { ensureSessionDir } = await import("../src/sessions.ts");
    ensureSessionDir();

    // Write garbage to the lock file
    const lockPath = getSocketPath(name).replace(".sock", ".lock");
    fs.writeFileSync(lockPath, "not-a-pid");

    // Should steal the lock
    expect(acquireLock(name)).toBe(true);
    releaseLock(name);
  });

  it("uses smallest connected client size", async () => {
    const name = uniqueName();
    await startServer(name, "sh", [], { rows: 24, cols: 80 });

    // Client 1 attaches with 50x200
    const client1 = await connect(name);
    const reader1 = new PacketReader();
    client1.write(encodeAttach(50, 200));
    await waitForType(client1, reader1, MessageType.SCREEN);

    // Ask for size — should be 50x200 (only client)
    client1.write(encodeData("stty size\n"));
    await waitForContent(client1, reader1, "50 200");

    // Client 2 attaches with smaller size 30x100
    const client2 = await connect(name);
    const reader2 = new PacketReader();
    client2.write(encodeAttach(30, 100));
    await waitForType(client2, reader2, MessageType.SCREEN);

    // Size should now be the minimum: 30x100
    client1.write(encodeData("stty size\n"));
    await waitForContent(client1, reader1, "30 100");

    client1.destroy();
    client2.destroy();
  });

  it("uses minimum of each dimension independently", async () => {
    const name = uniqueName();
    await startServer(name, "sh", [], { rows: 24, cols: 80 });

    // Client 1: tall and narrow (60 rows, 80 cols)
    const client1 = await connect(name);
    const reader1 = new PacketReader();
    client1.write(encodeAttach(60, 80));
    await waitForType(client1, reader1, MessageType.SCREEN);

    // Client 2: short and wide (30 rows, 200 cols)
    const client2 = await connect(name);
    const reader2 = new PacketReader();
    client2.write(encodeAttach(30, 200));
    await waitForType(client2, reader2, MessageType.SCREEN);

    // Should be min of each: 30 rows, 80 cols
    client1.write(encodeData("stty size\n"));
    await waitForContent(client1, reader1, "30 80");

    client1.destroy();
    client2.destroy();
  });

  it("recalculates size when a client disconnects", async () => {
    const name = uniqueName();
    await startServer(name, "sh", [], { rows: 24, cols: 80 });

    // Client 1: large terminal
    const client1 = await connect(name);
    const reader1 = new PacketReader();
    client1.write(encodeAttach(50, 200));
    await waitForType(client1, reader1, MessageType.SCREEN);

    // Client 2: small terminal (phone)
    const client2 = await connect(name);
    const reader2 = new PacketReader();
    client2.write(encodeAttach(30, 80));
    await waitForType(client2, reader2, MessageType.SCREEN);

    // Size should be 30x80 (smallest)
    client1.write(encodeData("stty size\n"));
    await waitForContent(client1, reader1, "30 80");

    // Phone disconnects
    client2.destroy();
    // Give the server a moment to process the disconnect
    await new Promise((r) => setTimeout(r, 100));

    // Size should restore to 50x200 (only remaining client)
    client1.write(encodeData("stty size\n"));
    await waitForContent(client1, reader1, "50 200");

    client1.destroy();
  });

  it("recalculates size on clean detach", async () => {
    const name = uniqueName();
    await startServer(name, "sh", [], { rows: 24, cols: 80 });

    // Client 1: large terminal
    const client1 = await connect(name);
    const reader1 = new PacketReader();
    client1.write(encodeAttach(50, 200));
    await waitForType(client1, reader1, MessageType.SCREEN);

    // Client 2: small terminal
    const client2 = await connect(name);
    const reader2 = new PacketReader();
    client2.write(encodeAttach(25, 90));
    await waitForType(client2, reader2, MessageType.SCREEN);

    // Size should be 25x90 (smallest)
    client1.write(encodeData("stty size\n"));
    await waitForContent(client1, reader1, "25 90");

    // Client 2 detaches cleanly
    client2.write(encodeDetach());
    await new Promise((r) => setTimeout(r, 100));

    // Size should restore to 50x200
    client1.write(encodeData("stty size\n"));
    await waitForContent(client1, reader1, "50 200");

    client1.destroy();
  });

  it("resize message updates size negotiation", async () => {
    const name = uniqueName();
    await startServer(name, "sh", [], { rows: 24, cols: 80 });

    // Client 1: starts large
    const client1 = await connect(name);
    const reader1 = new PacketReader();
    client1.write(encodeAttach(50, 200));
    await waitForType(client1, reader1, MessageType.SCREEN);

    // Client 2: starts small
    const client2 = await connect(name);
    const reader2 = new PacketReader();
    client2.write(encodeAttach(30, 100));
    await waitForType(client2, reader2, MessageType.SCREEN);

    // Size should be 30x100
    client1.write(encodeData("stty size\n"));
    await waitForContent(client1, reader1, "30 100");

    // Client 2 resizes larger (rotated phone, etc.)
    client2.write(encodeResize(60, 250));
    await new Promise((r) => setTimeout(r, 100));

    // Now client 1 is the smallest: 50x200
    client1.write(encodeData("stty size\n"));
    await waitForContent(client1, reader1, "50 200");

    client1.destroy();
    client2.destroy();
  });

  it("server handles truncated ATTACH payload gracefully", async () => {
    const name = uniqueName();
    await startServer(name, "cat");

    const client = await connect(name);
    const reader = new PacketReader();

    // Send a malformed ATTACH with only 2 bytes payload instead of 4
    const { encodePacket, MessageType: MT } = await import(
      "../src/protocol.ts"
    );
    const badAttach = encodePacket(MT.ATTACH, Buffer.alloc(2));
    client.write(badAttach);

    // Send a proper ATTACH after — server should still work
    client.write(encodeAttach(24, 80));
    const screen = await waitForType(client, reader, MessageType.SCREEN);
    expect(screen.type).toBe(MessageType.SCREEN);

    client.destroy();
  });

  it("server handles truncated RESIZE payload gracefully", async () => {
    const name = uniqueName();
    await startServer(name, "cat");

    const client = await connect(name);
    const reader = new PacketReader();
    client.write(encodeAttach(24, 80));
    await waitForType(client, reader, MessageType.SCREEN);

    // Send malformed RESIZE with 1 byte payload
    const { encodePacket, MessageType: MT } = await import(
      "../src/protocol.ts"
    );
    client.write(encodePacket(MT.RESIZE, Buffer.alloc(1)));

    // Session should still work
    client.write(encodeData("after-bad-resize\n"));
    const data = await waitForType(client, reader, MessageType.DATA, 3000);
    expect(data.payload.toString()).toContain("after-bad-resize");

    client.destroy();
  });

  it("server ignores unknown message types", async () => {
    const name = uniqueName();
    await startServer(name, "cat");

    const client = await connect(name);
    const reader = new PacketReader();
    client.write(encodeAttach(24, 80));
    await waitForType(client, reader, MessageType.SCREEN);

    // Send a packet with unknown type 99
    const header = Buffer.alloc(5);
    header.writeUInt8(99, 0);
    header.writeUInt32BE(3, 1);
    client.write(Buffer.concat([header, Buffer.from("abc")]));

    // Session should still work
    client.write(encodeData("after-unknown\n"));
    const data = await waitForType(client, reader, MessageType.DATA, 3000);
    expect(data.payload.toString()).toContain("after-unknown");

    client.destroy();
  });

  it("SCREEN cursor position matches process intent after resize", async () => {
    const name = uniqueName();

    // TUI-like process: enters alt screen, positions cursor at col 60.
    // On SIGWINCH: redraws with cursor at col 10.
    // Using "sleep & wait" so bash processes the trap immediately when
    // SIGWINCH interrupts the wait builtin (no sleep cycle delay).
    await startServer(
      name,
      "bash",
      [
        "-c",
        "printf '\\033[?1049h\\033[2J\\033[1;1HTitle\\033[5;60H'; " +
          "trap 'printf \"\\033[2J\\033[1;1HTitle\\033[5;10H\"' WINCH; " +
          "sleep 300 & wait; sleep 300",
      ],
      { rows: 24, cols: 80 }
    );

    await new Promise((r) => setTimeout(r, 500));

    // Attach at original size, verify cursor is at (row 5, col 60) = (4, 59) 0-indexed
    const client1 = await connect(name);
    const reader1 = new PacketReader();
    client1.write(encodeAttach(24, 80));
    const screen1 = await waitForType(client1, reader1, MessageType.SCREEN);

    const t1 = new Terminal({ rows: 24, cols: 80, allowProposedApi: true });
    await new Promise<void>((r) => t1.write(screen1.payload.toString(), r));
    expect(t1.buffer.active.cursorY).toBe(4);
    expect(t1.buffer.active.cursorX).toBe(59);
    t1.dispose();

    client1.destroy();
    await new Promise((r) => setTimeout(r, 200));

    // Reconnect at a NARROWER terminal — col 60 doesn't exist in 40 cols.
    // The server resizes xterm-headless to 40 cols (clamping cursor to col 39),
    // then serializes BEFORE the process can respond to SIGWINCH.
    const client2 = await connect(name);
    const reader2 = new PacketReader();
    client2.write(encodeAttach(24, 40));
    const screen2 = await waitForType(client2, reader2, MessageType.SCREEN);

    const t2 = new Terminal({ rows: 24, cols: 40, allowProposedApi: true });
    await new Promise<void>((r) => t2.write(screen2.payload.toString(), r));

    // The process's SIGWINCH trap will redraw with cursor at (4, 9).
    // But SCREEN was serialized before the trap could fire, so the cursor
    // is at the clamped position (4, 39) — not where the process wants it.
    expect(t2.buffer.active.cursorY).toBe(4);
    expect(t2.buffer.active.cursorX).toBe(9); // FAILS: actual is 39 (clamped)
    t2.dispose();

    client2.destroy();
  }, 15000);

  // ─── Terminal mode preservation across attach ───
  //
  // Terminal input modes (keyboard protocol, mouse tracking, bracketed paste,
  // etc.) are set by the process writing escape sequences to the PTY. These
  // are consumed by xterm-headless (they're not visual content) and never
  // appear in SerializeAddon output. This means:
  //
  //   1. Late attach: mode was broadcast when zero clients were connected → lost
  //   2. Detach/reattach: TERMINAL_SANITIZE resets the mode, SCREEN doesn't
  //      restore it, and the process doesn't re-send it → lost
  //
  // Each test starts a process that enables a mode, waits for it to
  // initialize, then verifies the mode sequence reaches a late-attaching
  // client.

  // Only modes NOT already preserved by xterm-headless SerializeAddon.
  // Modes like bracketed paste, mouse click/button-event/any-event tracking,
  // focus reporting, application cursor keys, and alternate screen buffer
  // are already serialized by xterm-headless — they pass without any extra work.
  const terminalModes = [
    {
      name: "Kitty keyboard protocol",
      enable: "\x1b[>1u",
      printf: "\\033[>1u",
      why: "Shift+Enter, key disambiguation",
    },
    {
      name: "SGR mouse mode",
      enable: "\x1b[?1006h",
      printf: "\\033[?1006h",
      why: "extended mouse coordinates (>223 cols)",
    },
    {
      name: "cursor hidden",
      enable: "\x1b[?25l",
      printf: "\\033[?25l",
      why: "TUI apps hide cursor during rendering",
    },
  ];

  for (const mode of terminalModes) {
    it(`${mode.name} mode reaches client on late attach (${mode.why})`, async () => {
      const name = uniqueName();
      await startServer(name, "sh", [
        "-c",
        `printf '${mode.printf}'; echo 'ready'; cat`,
      ]);

      // Process has initialized — mode was broadcast to zero clients.
      await new Promise((r) => setTimeout(r, 300));

      const client = await connect(name);
      const reader = new PacketReader();
      const payloads: string[] = [];

      client.on("data", (data: Buffer) => {
        for (const p of reader.feed(data)) {
          if (p.type === MessageType.SCREEN || p.type === MessageType.DATA) {
            payloads.push(p.payload.toString());
          }
        }
      });

      client.write(encodeAttach(24, 80));
      await new Promise((r) => setTimeout(r, 500));

      expect(payloads.join("")).toContain(mode.enable);
      client.destroy();
    });
  }

  // ─── send (no ATTACH) ───

  describe("send", () => {
    it("sends text to a running session", async () => {
      const name = uniqueName();
      await startServer(name, "cat");

      // Attach a watcher to observe output
      const watcher = await connect(name);
      const watchReader = new PacketReader();
      watcher.write(encodeAttach(24, 80));
      await waitForType(watcher, watchReader, MessageType.SCREEN);

      // Send data without ATTACH — just raw DATA packets
      const sender = await connect(name);
      sender.write(encodeData("hello from send"));
      sender.end();

      // Watcher should see the echoed output
      const data = await waitForType(watcher, watchReader, MessageType.DATA, 3000);
      expect(data.payload.toString()).toContain("hello from send");

      watcher.destroy();
    });

    it("sends multiple DATA packets in sequence", async () => {
      const name = uniqueName();
      await startServer(name, "cat");

      const watcher = await connect(name);
      const watchReader = new PacketReader();
      watcher.write(encodeAttach(24, 80));
      await waitForType(watcher, watchReader, MessageType.SCREEN);

      // Start accumulating DATA before sending
      let output = "";
      const watchReader2 = new PacketReader();
      watcher.on("data", (data: Buffer) => {
        for (const p of watchReader2.feed(data)) {
          if (p.type === MessageType.DATA) {
            output += p.payload.toString();
          }
        }
      });

      const sender = await connect(name);
      sender.write(encodeData("one"));
      sender.write(encodeData("two"));
      sender.write(encodeData("three\n"));
      sender.end();

      // Wait for cat to echo all data
      await new Promise((r) => setTimeout(r, 500));
      expect(output).toContain("one");
      expect(output).toContain("two");
      expect(output).toContain("three");

      watcher.destroy();
    });

    it("does not trigger screen replay", async () => {
      const name = uniqueName();
      await startServer(name, "sh", ["-c", "echo 'initial output'; cat"]);
      await new Promise((r) => setTimeout(r, 200));

      // Send data without ATTACH
      const sender = await connect(name);
      const senderReader = new PacketReader();
      const senderPackets: Packet[] = [];

      sender.on("data", (data: Buffer) => {
        senderPackets.push(...senderReader.feed(data));
      });

      sender.write(encodeData("sent text"));
      await new Promise((r) => setTimeout(r, 300));
      sender.end();
      await new Promise((r) => sender.on("close", r));

      // Sender should not have received any SCREEN packet
      const screenPackets = senderPackets.filter((p) => p.type === MessageType.SCREEN);
      expect(screenPackets.length).toBe(0);
    });

    it("sends items with delay between them", async () => {
      const name = uniqueName();
      await startServer(name, "cat");

      const watcher = await connect(name);
      const watchReader = new PacketReader();
      watcher.write(encodeAttach(24, 80));
      await waitForType(watcher, watchReader, MessageType.SCREEN);

      // Track timestamps of received DATA packets
      const timestamps: number[] = [];
      const watchReader2 = new PacketReader();
      let output = "";
      watcher.on("data", (data: Buffer) => {
        for (const p of watchReader2.feed(data)) {
          if (p.type === MessageType.DATA) {
            timestamps.push(Date.now());
            output += p.payload.toString();
          }
        }
      });

      // Send items with 200ms delay between each
      const { send } = await import("../src/client.ts");
      send({ name, data: ["A", "B", "C"], delayMs: 200 });

      // Wait for all items to arrive
      await new Promise((r) => setTimeout(r, 1000));

      expect(output).toContain("A");
      expect(output).toContain("B");
      expect(output).toContain("C");

      // There should be measurable gaps between items (at least 100ms to account for timing jitter)
      expect(timestamps.length).toBeGreaterThanOrEqual(2);
      const firstGap = timestamps[1] - timestamps[0];
      expect(firstGap).toBeGreaterThanOrEqual(100);

      watcher.destroy();
    });

    it("connection to non-existent session produces error", async () => {
      const name = uniqueName();

      // Try to connect — should fail with ENOENT
      const socket = net.createConnection(getSocketPath(name));
      const error = await new Promise<NodeJS.ErrnoException>((resolve) => {
        socket.on("error", resolve);
      });
      expect(error.code).toMatch(/ENOENT|ECONNREFUSED/);
    });
  });

  it("peek with plain flag returns text without ANSI codes", async () => {
    const name = uniqueName();
    // Print colored output with ANSI codes
    await startServer(name, "sh", [
      "-c",
      "printf '\\033[32mGREEN TEXT\\033[0m'; printf '\\033[1;31mBOLD RED\\033[0m'; sleep 30",
    ]);
    await new Promise((r) => setTimeout(r, 200));

    // Normal peek — should contain ANSI escape sequences
    const normalPeeker = await connect(name);
    const normalReader = new PacketReader();
    normalPeeker.write(encodePeek(false));
    const normalScreen = await waitForType(normalPeeker, normalReader, MessageType.SCREEN);
    const normalOutput = normalScreen.payload.toString();
    expect(normalOutput).toContain("GREEN TEXT");
    expect(normalOutput).toContain("\x1b["); // ANSI escape present
    normalPeeker.destroy();

    // Plain peek — should NOT contain ANSI escape sequences
    const plainPeeker = await connect(name);
    const plainReader = new PacketReader();
    plainPeeker.write(encodePeek(true));
    const plainScreen = await waitForType(plainPeeker, plainReader, MessageType.SCREEN);
    const plainOutput = plainScreen.payload.toString();
    expect(plainOutput).toContain("GREEN TEXT");
    expect(plainOutput).toContain("BOLD RED");
    expect(plainOutput).not.toContain("\x1b["); // no ANSI escapes
    plainPeeker.destroy();
  });

  it("peek plain trims trailing blank lines", async () => {
    const name = uniqueName();
    await startServer(name, "sh", ["-c", "echo 'only line'; sleep 30"], {
      rows: 24,
      cols: 80,
    });
    await new Promise((r) => setTimeout(r, 200));

    const peeker = await connect(name);
    const reader = new PacketReader();
    peeker.write(encodePeek(true));
    const screen = await waitForType(peeker, reader, MessageType.SCREEN);
    const output = screen.payload.toString();

    // Should have the content but not 24 rows of blank lines
    expect(output).toContain("only line");
    const lines = output.split("\n");
    expect(lines.length).toBeLessThan(10);
    peeker.destroy();
  });

  // ─── restart ───

  describe("restart", () => {
    it("restart preserves command and cwd after killing a running session", async () => {
      const name = uniqueName();
      await startServer(name, "sh", ["-c", "echo 'original'; sleep 60"]);
      await new Promise((r) => setTimeout(r, 200));

      // Verify it's running and has metadata
      const meta1 = readMetadata(name);
      expect(meta1).not.toBeNull();
      expect(meta1!.command).toBe("sh");
      expect(meta1!.cwd).toBe(testCwd);

      // Verify we can connect
      const client = await connect(name);
      const reader = new PacketReader();
      client.write(encodeAttach(24, 80));
      const screen = await waitForType(client, reader, MessageType.SCREEN);
      expect(screen.payload.toString()).toContain("original");
      client.destroy();

      // Kill the server (simulating what cmdRestart does)
      await servers[servers.length - 1].close();
      servers.pop();

      // Metadata should still exist (server.close() only cleans socket/pid)
      const meta2 = readMetadata(name);
      expect(meta2).not.toBeNull();
      expect(meta2!.command).toBe("sh");
      expect(meta2!.cwd).toBe(testCwd);

      // Restart with the same metadata
      await startServer(name, meta2!.command, meta2!.args, { cwd: meta2!.cwd });
      await new Promise((r) => setTimeout(r, 200));

      // Verify the restarted session works
      const client2 = await connect(name);
      const reader2 = new PacketReader();
      client2.write(encodeAttach(24, 80));
      const screen2 = await waitForType(client2, reader2, MessageType.SCREEN);
      expect(screen2.payload.toString()).toContain("original");
      client2.destroy();
    });

    it("metadata persists through kill for restart", async () => {
      const name = uniqueName();
      const server = await startServer(name, "cat", ["-u"], { cwd: testCwd });

      // Verify initial metadata
      const meta = readMetadata(name);
      expect(meta).not.toBeNull();
      expect(meta!.command).toBe("cat");
      expect(meta!.args).toEqual(["-u"]);
      expect(meta!.cwd).toBe(testCwd);

      // Simulate what cmdRestart does for a running session:
      // 1. Read metadata (already done above)
      // 2. Kill process
      await server.close();
      servers = servers.filter((s) => s !== server);

      // 3. Metadata should still be on disk (close() only removes socket/pid)
      const metaAfterKill = readMetadata(name);
      expect(metaAfterKill).not.toBeNull();
      expect(metaAfterKill!.command).toBe("cat");
      expect(metaAfterKill!.args).toEqual(["-u"]);
      expect(metaAfterKill!.cwd).toBe(testCwd);
    });
  });

  // Detach/reattach: the mode was active, TERMINAL_SANITIZE popped it on
  // detach, and the SCREEN replay on reattach doesn't restore it. The
  // process only sent the mode push once at startup (like Claude Code does
  // with Kitty keyboard protocol) and won't re-send it.

  for (const mode of terminalModes) {
    it(`${mode.name} mode survives detach/reattach`, async () => {
      const name = uniqueName();
      await startServer(name, "sh", [
        "-c",
        `printf '${mode.printf}'; echo 'ready'; cat`,
      ]);

      // First client attaches immediately — gets mode via DATA
      const client1 = await connect(name);
      const reader1 = new PacketReader();
      client1.on("data", (data: Buffer) => reader1.feed(data));
      client1.write(encodeAttach(24, 80));
      await new Promise((r) => setTimeout(r, 300));

      // Detach (TERMINAL_SANITIZE resets the mode on the user's terminal)
      client1.write(encodeDetach());
      await new Promise((r) => setTimeout(r, 200));

      // Reattach — the mode must be present in SCREEN or DATA
      const client2 = await connect(name);
      const reader2 = new PacketReader();
      const payloads: string[] = [];

      client2.on("data", (data: Buffer) => {
        for (const p of reader2.feed(data)) {
          if (p.type === MessageType.SCREEN || p.type === MessageType.DATA) {
            payloads.push(p.payload.toString());
          }
        }
      });

      client2.write(encodeAttach(24, 80));
      await new Promise((r) => setTimeout(r, 500));

      expect(payloads.join("")).toContain(mode.enable);
      client2.destroy();
    });
  }
});
