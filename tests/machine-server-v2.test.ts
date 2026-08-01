import { afterAll, afterEach, describe, expect, it } from "vitest";
import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { Terminal } from "@xterm/headless";
import {
  MACHINE_PROTOCOL_VERSION,
  MachineFrameReader,
  decodeDaemonAdmissionV2,
  decodeMachineResponse,
  encodeDaemonOpenV2,
  encodeMachineRequest,
  type MachineOpenV2,
  type MachineWireFrame,
} from "../src/machine-protocol.ts";
import { MessageType, PacketReader, encodeStatus } from "../src/protocol.ts";
import { PtyServer } from "../src/server.ts";
import {
  cleanupAll,
  getSocketPath,
  readMetadata,
} from "../src/sessions.ts";

const testCwd = fs.mkdtempSync(path.join(os.tmpdir(), "pty-machine-v2-"));
const testSessionDir = fs.mkdtempSync(path.join(os.tmpdir(), "pty-machine-v2-sd-"));
process.env.PTY_SESSION_DIR = testSessionDir;

const servers: PtyServer[] = [];
const names: string[] = [];

afterEach(async () => {
  for (const server of servers.splice(0)) await server.close();
  for (const name of names.splice(0)) cleanupAll(name);
});

afterAll(() => {
  fs.rmSync(testCwd, { recursive: true, force: true });
  fs.rmSync(testSessionDir, { recursive: true, force: true });
});

function uniqueName(): string {
  const name = `machine-v2-${process.pid}-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`;
  names.push(name);
  return name;
}

async function startServer(
  name: string,
  command = "cat",
  args: string[] = [],
  generation = "generation-a",
): Promise<PtyServer> {
  const server = new PtyServer({
    name,
    generation,
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

function connect(name: string): Promise<net.Socket> {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection(getSocketPath(name));
    socket.once("connect", () => resolve(socket));
    socket.once("error", reject);
  });
}

function recordFrames(socket: net.Socket): {
  readonly frames: MachineWireFrame[];
  waitFor: (predicate: (frames: readonly MachineWireFrame[]) => boolean) => Promise<void>;
} {
  const reader = new MachineFrameReader();
  const frames: MachineWireFrame[] = [];
  let wake: (() => void) | undefined;
  socket.on("data", (chunk: Buffer) => {
    frames.push(...reader.feed(chunk));
    wake?.();
  });
  return {
    frames,
    async waitFor(predicate) {
      const deadline = performance.now() + 5_000;
      while (!predicate(frames)) {
        await new Promise<void>((resolve, reject) => {
          const timer = setTimeout(
            () => reject(new Error("Timed out waiting for machine frames")),
            Math.max(0, deadline - performance.now()),
          );
          wake = () => {
            clearTimeout(timer);
            resolve();
          };
        });
        wake = undefined;
      }
    },
  };
}

function openRequest(name: string, generation = "generation-a"): MachineOpenV2 {
  return {
    _tag: "Open",
    protocol: MACHINE_PROTOCOL_VERSION,
    sessionId: name,
    expectedGeneration: generation,
    rows: 24,
    cols: 80,
    requiredCapabilities: [
      "framed-utf8-input",
      "typed-outcome",
      "input-mode-snapshot",
    ],
  };
}

async function openMachine(name: string): Promise<{
  socket: net.Socket;
  frames: ReturnType<typeof recordFrames>;
}> {
  const socket = await connect(name);
  const frames = recordFrames(socket);
  socket.write(Buffer.concat([encodeDaemonOpenV2(openRequest(name)), encodeStatus()]));
  await frames.waitFor((received) => received.length >= 2);
  expect(decodeDaemonAdmissionV2(frames.frames[0])).toMatchObject({
    _tag: "Accepted",
    generation: "generation-a",
  });
  expect(decodeMachineResponse(frames.frames[1])._tag).toBe("Ready");
  return { socket, frames };
}

describe("machine attach v2 daemon admission", () => {
  it("rejects a stale generation before role, geometry, or metadata mutation", async () => {
    const name = uniqueName();
    await startServer(name);
    const before = readMetadata(name)!;
    const socket = await connect(name);
    const frames = recordFrames(socket);
    const closed = new Promise<void>((resolve) => socket.once("close", () => resolve()));
    socket.write(Buffer.concat([
      encodeDaemonOpenV2(openRequest(name, "stale-generation")),
      encodeStatus(),
    ]));
    await frames.waitFor((received) => received.length >= 1);

    expect(decodeDaemonAdmissionV2(frames.frames[0])).toMatchObject({
      _tag: "Rejected",
      reason: "generation-mismatch",
    });
    await closed;
    expect(frames.frames).toHaveLength(1);
    expect(readMetadata(name)).toEqual(before);

    const statusSocket = await connect(name);
    const statusReader = new PacketReader();
    const status = new Promise<Record<string, unknown>>((resolve) => {
      statusSocket.on("data", (chunk: Buffer) => {
        const packet = statusReader.feed(chunk).find((item) => item.type === MessageType.STATUS);
        if (packet) resolve(JSON.parse(packet.payload.toString()));
      });
    });
    statusSocket.write(encodeStatus());
    await expect(status).resolves.toMatchObject({
      terminal: { rows: 24, cols: 80 },
      clients: { attached: 0, readOnly: 0 },
    });
    socket.destroy();
    statusSocket.destroy();
  });

  it("commits admission before Accepted and emits one atomic Ready without Geometry", async () => {
    const name = uniqueName();
    await startServer(name);
    const { socket, frames } = await openMachine(name);

    expect(frames.frames.slice(0, 2).map((frame) => frame.type)).toEqual([9, 2]);
    expect(frames.frames.some((frame) => frame.type === 4)).toBe(false);
    expect(readMetadata(name)?.lastAttachAt).toBeTypeOf("string");
    socket.destroy();
  });

  it("treats C0 bytes as input and invalid UTF-8 as a typed stream failure", async () => {
    const name = uniqueName();
    await startServer(name, "sh", [
      "-c",
      "stty raw -echo -isig; printf 'RAW_READY\\r\\n'; od -An -tx1 -N 3",
    ]);
    const { socket, frames } = await openMachine(name);
    const hasRawReady = () => frames.frames.slice(1).map(decodeMachineResponse).some(
      (response) =>
        (response._tag === "Ready" && response.screen.includes(Buffer.from("RAW_READY"))) ||
        (response._tag === "Data" && response.bytes.includes(Buffer.from("RAW_READY")))
    );
    if (!hasRawReady()) await frames.waitFor(hasRawReady);
    socket.write(encodeMachineRequest({
      _tag: "Input",
      bytes: Buffer.from([0x41, 0x1c, 0x42]),
    }));
    await frames.waitFor((received) => received.some((frame, index) =>
      index >= 2 && decodeMachineResponse(frame)._tag === "Exited"
    ));
    const responses = frames.frames.slice(1).map(decodeMachineResponse);
    expect(
      Buffer.concat(
        responses.flatMap((response) => response._tag === "Data" ? [response.bytes] : [])
      ).toString()
    ).toMatch(/41\s+1c\s+42/);
    expect(responses.some((response) => response._tag === "Detached")).toBe(false);
    socket.destroy();

    const second = uniqueName();
    await startServer(second);
    const opened = await openMachine(second);
    opened.socket.write(encodeMachineRequest({
      _tag: "Input",
      bytes: Buffer.from([0xff]),
    }));
    await opened.frames.waitFor((received) => received.some((frame, index) =>
      index >= 2 && decodeMachineResponse(frame)._tag === "StreamFailure"
    ));
    expect(opened.frames.frames.slice(2).map(decodeMachineResponse)).toContainEqual({
      _tag: "StreamFailure",
      phase: "stream",
      reason: "invalid-utf8-input",
    });
    opened.socket.destroy();
  });

  it("orders mode-establishing Data before one complete mode snapshot", async () => {
    const name = uniqueName();
    const modeBytes = [
      "\x1b[?1h",
      "\x1b=",
      "\x1b[?2004h",
      "\x1b[?1004h",
      "\x1b[?9h",
      "\x1b[?1016h",
      "\x1b[>4;2m",
      "\x1b[=5;1u",
      "\x1b[=2;2u",
      "\x1b[=1;3u",
      "\x1b[>9u",
      "\x1b[>10u",
      "\x1b[<2u",
    ].join("");
    const script = [
      "process.stdin.once('data', () => process.stdout.write(Buffer.from(process.argv[1], 'base64')))",
      "setInterval(() => {}, 1000)",
    ].join(";");
    await startServer(name, process.execPath, ["-e", script, Buffer.from(modeBytes).toString("base64")]);
    const { socket, frames } = await openMachine(name);
    socket.write(encodeMachineRequest({ _tag: "Input", bytes: Buffer.from("go\n") }));
    await frames.waitFor((received) => received.some((frame, index) =>
      index >= 2 && decodeMachineResponse(frame)._tag === "InputModes"
    ));

    const responses = frames.frames.slice(2).map(decodeMachineResponse);
    const inputModesIndex = responses.findIndex((response) => response._tag === "InputModes");
    const establishingDataIndex = responses.findIndex((response) =>
      response._tag === "Data" && response.bytes.includes(Buffer.from("\x1b[?1h"))
    );
    expect(establishingDataIndex).toBeGreaterThanOrEqual(0);
    expect(inputModesIndex).toBeGreaterThan(establishingDataIndex);
    expect(responses[inputModesIndex]).toMatchObject({
      _tag: "InputModes",
      inputModes: {
        applicationCursorKeys: true,
        applicationKeypad: true,
        bracketedPaste: true,
        focusReporting: true,
        modifyOtherKeys: 2,
        mouseTracking: "x10",
        mouseEncoding: "sgr-pixels",
        kittyKeyboardFlags: [6],
      },
    });
    expect(responses.filter((response) => response._tag === "InputModes")).toHaveLength(1);
    socket.destroy();
  });

  it("allows explicit detach after admission while Ready is still syncing", async () => {
    const name = uniqueName();
    const server = await startServer(name);
    const terminal = (server as unknown as { terminal: Terminal }).terminal;
    const originalWrite = terminal.write.bind(terminal);
    terminal.write = () => undefined;
    try {
      const socket = await connect(name);
      const frames = recordFrames(socket);
      socket.write(Buffer.concat([encodeDaemonOpenV2(openRequest(name)), encodeStatus()]));
      await frames.waitFor((received) => received.length >= 1);
      expect(decodeDaemonAdmissionV2(frames.frames[0])._tag).toBe("Accepted");
      socket.write(encodeMachineRequest({ _tag: "Detach" }));
      await frames.waitFor((received) => received.length >= 2);
      expect(decodeMachineResponse(frames.frames[1])).toEqual({ _tag: "Detached" });
      socket.destroy();
    } finally {
      terminal.write = originalWrite;
    }
  });
});
