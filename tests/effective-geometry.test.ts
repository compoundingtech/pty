import { afterAll, afterEach, describe, expect, it } from "vitest";
import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { PtyServer, type ServerOptions } from "../src/server.ts";
import { SessionConnection } from "../src/connection.ts";
import { Session } from "../src/testing/session.ts";
import {
  MessageType,
  PacketReader,
  encodeAttach,
  encodePeek,
  encodeResize,
  decodeGeometry,
  type Packet,
} from "../src/protocol.ts";
import { cleanupAll, getSocketPath } from "../src/sessions.ts";
import { queryStats } from "../src/client.ts";

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-geometry-"));
const sessionDir = fs.mkdtempSync(path.join(testRoot, "sessions-"));
process.env.PTY_SESSION_DIR = sessionDir;

let servers: PtyServer[] = [];
let names: string[] = [];

const widthOutputScript = [
  "process.on('SIGWINCH', () => setImmediate(() => {",
  "  const width = process.stdout.columns || 0;",
  "  process.stdout.write('\\x1b[2J\\x1b[H' + 'X'.repeat(width * 2));",
  "}));",
  "setInterval(() => {}, 1000);",
].join("\n");

afterAll(() => {
  fs.rmSync(testRoot, { recursive: true, force: true });
});

afterEach(async () => {
  for (const server of servers) await server.close();
  for (const name of names) cleanupAll(name);
  servers = [];
  names = [];
});

function uniqueName(): string {
  const name = `geometry-${process.pid}-${Date.now()}-${Math.random().toString(36).slice(2, 7)}`;
  names.push(name);
  return name;
}

async function startServer(
  name: string,
  options: Partial<ServerOptions> = {},
): Promise<PtyServer> {
  const server = new PtyServer({
    name,
    command: process.execPath,
    args: ["-e", widthOutputScript],
    displayCommand: "geometry-child",
    cwd: testRoot,
    rows: 24,
    cols: 20,
    ...options,
  });
  servers.push(server);
  await server.ready;
  return server;
}

function connect(name: string): Promise<net.Socket> {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection(getSocketPath(name));
    socket.once("connect", () => resolve(socket));
    socket.once("error", reject);
  });
}

class Recorder {
  packets: Packet[] = [];
  readonly reader = new PacketReader();

  constructor(readonly socket: net.Socket) {
    socket.on("data", (data) => {
      this.packets.push(...this.reader.feed(
        Buffer.isBuffer(data) ? data : Buffer.from(data),
      ));
    });
  }

  clear(): void {
    this.packets = [];
  }
}

async function waitFor(
  predicate: () => boolean | Promise<boolean>,
  description: string,
  timeoutMs = 5000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  throw new Error(`timed out waiting for ${description}`);
}

function geometryIndex(
  packets: Packet[],
  rows: number,
  cols: number,
): number {
  return packets.findIndex((packet) =>
    packet.type === MessageType.GEOMETRY &&
    JSON.stringify(decodeGeometry(packet.payload)) === JSON.stringify({ rows, cols })
  );
}

function outputIndices(packets: Packet[]): number[] {
  return packets.flatMap((packet, index) =>
    packet.type === MessageType.SCREEN || packet.type === MessageType.DATA
      ? [index]
      : []
  );
}

function expectGeometryBeforeAllOutput(
  packets: Packet[],
  rows: number,
  cols: number,
): void {
  const geometry = geometryIndex(packets, rows, cols);
  const output = outputIndices(packets);
  expect(geometry).toBeGreaterThanOrEqual(0);
  expect(output.length).toBeGreaterThan(0);
  expect(output.every((index) => geometry < index)).toBe(true);
}

function hasTwoWrappedLines(session: Session, width: number): boolean {
  const expected = "X".repeat(width);
  const lines = session.screenshot().lines.filter((line) => line.length > 0);
  return lines[0] === expected && lines[1] === expected;
}

async function settle(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 250));
}

describe("stream-ordered effective geometry", () => {
  it("orders geometry before affected output for existing and smaller attaching clients", async () => {
    const name = uniqueName();
    await startServer(name);
    const large = new Recorder(await connect(name));
    large.socket.write(encodeAttach(24, 20));
    await waitFor(
      () => geometryIndex(large.packets, 24, 20) >= 0 &&
        large.packets.some((packet) => packet.type === MessageType.SCREEN),
      "large initial attach",
    );
    expectGeometryBeforeAllOutput(large.packets, 24, 20);
    await settle();
    large.clear();

    const small = new Recorder(await connect(name));
    small.socket.write(encodeAttach(24, 10));
    await waitFor(
      () => geometryIndex(large.packets, 24, 10) >= 0 &&
        large.packets.some((packet) => packet.type === MessageType.DATA) &&
        geometryIndex(small.packets, 24, 10) >= 0 &&
        outputIndices(small.packets).length > 0,
      "smaller peer attach output",
    );
    expectGeometryBeforeAllOutput(large.packets, 24, 10);
    expectGeometryBeforeAllOutput(small.packets, 24, 10);

    large.socket.destroy();
    small.socket.destroy();
  });

  it("orders geometry before resize and disconnect output", async () => {
    const name = uniqueName();
    await startServer(name);
    const large = new Recorder(await connect(name));
    large.socket.write(encodeAttach(24, 20));
    await waitFor(() => geometryIndex(large.packets, 24, 20) >= 0, "large geometry");
    const small = new Recorder(await connect(name));
    small.socket.write(encodeAttach(24, 10));
    await waitFor(() => geometryIndex(large.packets, 24, 10) >= 0, "small geometry");
    await settle();

    large.clear();
    small.clear();
    small.socket.write(encodeResize(24, 8));
    await waitFor(
      () => geometryIndex(large.packets, 24, 8) >= 0 &&
        large.packets.some((packet) => packet.type === MessageType.DATA) &&
        geometryIndex(small.packets, 24, 8) >= 0,
      "peer resize output",
    );
    expectGeometryBeforeAllOutput(large.packets, 24, 8);
    expectGeometryBeforeAllOutput(small.packets, 24, 8);
    await settle();

    large.clear();
    small.socket.destroy();
    await waitFor(
      () => geometryIndex(large.packets, 24, 20) >= 0 &&
        large.packets.some((packet) => packet.type === MessageType.DATA),
      "peer disconnect output",
    );
    expectGeometryBeforeAllOutput(large.packets, 24, 20);
    large.socket.destroy();
  });

  it("streams geometry to read-only viewers without letting them select size", async () => {
    const name = uniqueName();
    await startServer(name);
    const writable = new Recorder(await connect(name));
    writable.socket.write(encodeAttach(24, 20));
    await waitFor(() => geometryIndex(writable.packets, 24, 20) >= 0, "writable geometry");

    const readOnly = new Recorder(await connect(name));
    readOnly.socket.write(encodePeek());
    await waitFor(
      () => geometryIndex(readOnly.packets, 24, 20) >= 0 &&
        readOnly.packets.some((packet) => packet.type === MessageType.SCREEN),
      "read-only initial geometry",
    );
    expectGeometryBeforeAllOutput(readOnly.packets, 24, 20);
    expect((await queryStats(name)).terminal.cols).toBe(20);
    await settle();

    readOnly.clear();
    writable.socket.write(encodeResize(24, 10));
    await waitFor(
      () => geometryIndex(readOnly.packets, 24, 10) >= 0 &&
        readOnly.packets.some((packet) => packet.type === MessageType.DATA),
      "read-only resized geometry",
    );
    expectGeometryBeforeAllOutput(readOnly.packets, 24, 10);

    writable.socket.destroy();
    readOnly.socket.destroy();
    await waitFor(
      async () => (await queryStats(name)).terminal.cols === 10,
      "zero-viewer last size",
    );
  });

  it("updates SessionConnection effective geometry before subsequent data", async () => {
    const name = uniqueName();
    await startServer(name);
    const large = new SessionConnection({ name, rows: 24, cols: 20 });
    const geometries: Array<{ rows: number; cols: number }> = [];
    large.on("geometry", (geometry) => geometries.push(geometry));
    await large.connect();
    expect({ rows: large.effectiveRows, cols: large.effectiveCols })
      .toEqual({ rows: 24, cols: 20 });

    const small = new SessionConnection({ name, rows: 24, cols: 10 });
    await small.connect();
    await waitFor(() => large.effectiveCols === 10, "SessionConnection attach geometry");
    large.resize(24, 30);
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(large.effectiveCols).toBe(10);
    small.resize(24, 8);
    await waitFor(() => large.effectiveCols === 8, "SessionConnection resize geometry");
    small.disconnect();
    await waitFor(() => large.effectiveCols === 30, "SessionConnection disconnect geometry");

    expect(geometries.map((geometry) => geometry.cols)).toEqual([20, 10, 8, 30]);
    large.disconnect();
  });

  it("resizes server-mode testing grids on peer attach, resize, and disconnect", async () => {
    const name = uniqueName();
    const large = await Session.server(process.execPath, ["-e", widthOutputScript], {
      name,
      cwd: testRoot,
      rows: 24,
      cols: 20,
    });
    await large.attach();
    const small = await Session.connectToExisting(large, { rows: 24, cols: 10 });
    await small.attach();
    await waitFor(() => large.cols === 10, "testing attach geometry");
    await waitFor(() => hasTwoWrappedLines(large, 10), "testing attach grid");
    large.resize(24, 30);
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(large.cols).toBe(10);

    small.resize(24, 8);
    await waitFor(() => large.cols === 8, "testing resize geometry");
    await waitFor(() => hasTwoWrappedLines(large, 8), "testing resize grid");

    await small.close();
    await waitFor(() => large.cols === 30, "testing disconnect geometry");
    await waitFor(() => hasTwoWrappedLines(large, 30), "testing disconnect grid");
    await large.close();
  });

  it("resizes attachPty grids on peer attach, resize, and disconnect", async () => {
    const name = uniqueName();
    await startServer(name, {
      command: "sleep",
      args: ["30"],
      displayCommand: "sleep 30",
    });
    const { attachPty } = await import("../dist/tui/index.js") as typeof import("../src/tui/index.ts");
    const large = await attachPty(name, { rows: 24, cols: 20 });
    const small = await attachPty(name, { rows: 24, cols: 10 });
    await waitFor(() => large.cols === 10, "attachPty attach geometry");
    expect(large.readCells()[0]).toHaveLength(10);
    large.resize(30, 24);
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(large.cols).toBe(10);

    small.resize(8, 24);
    await waitFor(() => large.cols === 8, "attachPty resize geometry");
    expect(large.readCells()[0]).toHaveLength(8);

    small.kill();
    await waitFor(() => large.cols === 30, "attachPty disconnect geometry");
    expect(large.readCells()[0]).toHaveLength(30);
    large.kill();
  });
});
