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
  encodeMachineResponse,
  type MachineOpenV2,
  type MachineResponse,
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
  maxClientOutputBytes?: number,
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
    maxClientOutputBytes,
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

function openRequest(
  name: string,
  generation = "generation-a",
  hostTerminalReplay = false,
): MachineOpenV2 {
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
      ...(hostTerminalReplay ? ["host-terminal-replay" as const] : []),
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

  it("prefixes the Ready screen only when a human host requests terminal replay", async () => {
    const name = uniqueName();
    const server = await startServer(name);
    const handlePtyOutput = (server as unknown as {
      handlePtyOutput: (data: string) => void;
    }).handlePtyOutput.bind(server);
    handlePtyOutput("\x1b[?2004h\x1b[>1uHOST_READY");

    const socket = await connect(name);
    const frames = recordFrames(socket);
    socket.write(Buffer.concat([
      encodeDaemonOpenV2(openRequest(name, "generation-a", true)),
      encodeStatus(),
    ]));
    await frames.waitFor((received) => received.length >= 2);
    const ready = decodeMachineResponse(frames.frames[1]);
    if (ready._tag !== "Ready") throw new Error("expected Ready baseline");
    expect(ready.screen.subarray(0, "\x1b[?2004h\x1b[>1u".length).toString()).toBe(
      "\x1b[?2004h\x1b[>1u",
    );
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

  it("atomically stamps a mode-changing burst and omits unchanged snapshots", async () => {
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
    const server = await startServer(name);
    const { socket, frames } = await openMachine(name);
    const handlePtyOutput = (server as unknown as {
      handlePtyOutput: (data: string) => void;
    }).handlePtyOutput.bind(server);
    handlePtyOutput(modeBytes);
    handlePtyOutput("PLAIN_OUTPUT");
    await frames.waitFor((received) => received.some((frame, index) => {
      if (index < 2) return false;
      const response = decodeMachineResponse(frame);
      return response._tag === "Data" && response.inputModes !== undefined;
    }));

    const ready = decodeMachineResponse(frames.frames[1]);
    if (ready._tag !== "Ready") throw new Error("expected Ready baseline");
    const responses = frames.frames.slice(2).map(decodeMachineResponse);
    const establishingData = responses.find((response) =>
      response._tag === "Data" && response.bytes.includes(Buffer.from("\x1b[?1h"))
    );
    expect(establishingData).toMatchObject({
      _tag: "Data",
      inputModes: {
        schema: "pty.input-mode.v1",
        wireEncoder: "xterm-input.v1",
        applicationCursorKeys: true,
        applicationKeypad: true,
        bracketedPaste: true,
        focusReporting: true,
        modifyOtherKeys: 2,
        mouseTracking: "X10Press",
        mouseEncoding: "Sgr",
        mouseCoordinates: "Pixel",
        kittyKeyboardFlagsStack: [6],
      },
    });
    if (establishingData?._tag !== "Data") throw new Error("expected mode-establishing Data");
    expect(establishingData.outputRevision).toBe(ready.outputRevision + 1);
    expect(establishingData.inputModeRevision).toBe(establishingData.inputModes?.revision);

    await frames.waitFor((received) => received.some((frame, index) => {
      if (index < 2) return false;
      const response = decodeMachineResponse(frame);
      return response._tag === "Data" && response.bytes.includes(Buffer.from("PLAIN_OUTPUT"));
    }));
    const unchangedData = frames.frames.slice(2).map(decodeMachineResponse).find((response) =>
      response._tag === "Data" && response.bytes.includes(Buffer.from("PLAIN_OUTPUT"))
    );
    expect(unchangedData).toMatchObject({
      _tag: "Data",
      inputModeRevision: establishingData.inputModeRevision,
    });
    if (unchangedData?._tag !== "Data") throw new Error("expected unchanged-mode Data");
    expect(unchangedData.outputRevision).toBe(establishingData.outputRevision + 1);
    expect(unchangedData.inputModes).toBeUndefined();
    socket.destroy();
  });

  it("commits revisions at the parser cut and does not count stripped query output", async () => {
    const name = uniqueName();
    const script = [
      "process.stdin.setRawMode(true)",
      "process.stdin.on('data', chunk => {",
      "  if (chunk.includes(0x71)) process.stdout.write('\\x1b[c')",
      "  if (chunk.includes(0x76)) process.stdout.write('VISIBLE')",
      "})",
      "setInterval(() => {}, 1000)",
    ].join(";");
    const server = await startServer(name, process.execPath, ["-e", script]);
    const terminal = (server as unknown as { terminal: Terminal }).terminal;
    const ptyProcess = (server as unknown as { ptyProcess: { write: (data: string) => void } }).ptyProcess;
    const originalWrite = terminal.write.bind(terminal);
    const pending: Array<{ data: string; callback?: () => void }> = [];
    terminal.write = ((data: string, callback?: () => void) => {
      pending.push({ data, callback });
    }) as typeof terminal.write;

    const socket = await connect(name);
    const frames = recordFrames(socket);
    try {
      socket.write(Buffer.concat([encodeDaemonOpenV2(openRequest(name)), encodeStatus()]));
      await frames.waitFor((received) => received.length >= 1);
      ptyProcess.write("late");
      await expect.poll(() => pending.some((item) => item.data.length > 0)).toBe(true);

      const cut = pending.find((item) => item.data.length === 0);
      const late = pending.find((item) => item.data.length > 0);
      expect(cut).toBeDefined();
      expect(late).toBeDefined();
      cut!.callback?.();
      await frames.waitFor((received) => received.length >= 2);
      const ready = decodeMachineResponse(frames.frames[1]);
      expect(ready).toMatchObject({ _tag: "Ready", outputRevision: 0 });

      late!.callback?.();
      await frames.waitFor((received) => received.length >= 3);
      const lateData = decodeMachineResponse(frames.frames[2]);
      expect(lateData).toMatchObject({ _tag: "Data", outputRevision: 1 });
    } finally {
      terminal.write = originalWrite;
      socket.destroy();
    }

    const queryName = uniqueName();
    const queryObserved = path.join(testCwd, `${queryName}-query-observed`);
    const queryScript = [
      "const fs = require('node:fs')",
      "process.stdin.setRawMode(true)",
      "let awaitingReply = false",
      "process.stdout.write('RAW_READY')",
      "process.stdin.on('data', chunk => {",
      "  if (chunk.includes(0x71)) { awaitingReply = true; process.stdout.write('\\x1b[c'); return }",
      "  if (awaitingReply && chunk.includes(0x1b)) { awaitingReply = false; fs.writeFileSync(process.argv[1], '') }",
      "  if (chunk.includes(0x76)) process.stdout.write('VISIBLE')",
      "})",
      "setInterval(() => {}, 1000)",
    ].join(";");
    const revisionServer = await startServer(
      queryName,
      process.execPath,
      ["-e", queryScript, queryObserved],
    );
    const opened = await openMachine(queryName);
    const baseline = decodeMachineResponse(opened.frames.frames[1]);
    if (baseline._tag !== "Ready") throw new Error("expected Ready baseline");
    await opened.frames.waitFor((received) => received.some((frame, index) => {
      if (index < 1) return false;
      const response = decodeMachineResponse(frame);
      return (response._tag === "Ready" && response.screen.includes(Buffer.from("RAW_READY"))) ||
        (response._tag === "Data" && response.bytes.includes(Buffer.from("RAW_READY")));
    }));
    const beforeQueryRevision = (revisionServer as unknown as { outputRevision: number }).outputRevision;
    opened.socket.write(encodeMachineRequest({ _tag: "Input", bytes: Buffer.from("q") }));
    await expect.poll(() => fs.existsSync(queryObserved)).toBe(true);
    expect((revisionServer as unknown as { outputRevision: number }).outputRevision)
      .toBe(beforeQueryRevision);
    opened.socket.write(encodeMachineRequest({ _tag: "Input", bytes: Buffer.from("v") }));
    await opened.frames.waitFor((received) => received.some((frame, index) => {
      if (index < 2) return false;
      const response = decodeMachineResponse(frame);
      return response._tag === "Data" && response.bytes.includes(Buffer.from("VISIBLE"));
    }));
    const visible = opened.frames.frames.slice(2).map(decodeMachineResponse).find((response) =>
      response._tag === "Data" && response.bytes.includes(Buffer.from("VISIBLE"))
    );
    expect(visible).toMatchObject({
      _tag: "Data",
      outputRevision: beforeQueryRevision + 1,
    });
    opened.socket.destroy();
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

  it("fails only a slow machine consumer without pausing or reordering its peer", async () => {
    const name = uniqueName();
    const normalOutputLimit = 1024;
    const dataEnvelopeBytes = encodeMachineResponse({
      _tag: "Data",
      bytes: Buffer.from([0x61]),
      outputRevision: 1,
      inputModeRevision: 0,
    }).length - 1;
    const firstResponse = {
      _tag: "Data" as const,
      bytes: Buffer.alloc(normalOutputLimit - dataEnvelopeBytes, 0x61),
      outputRevision: 1,
      inputModeRevision: 0,
    };
    expect(encodeMachineResponse(firstResponse)).toHaveLength(normalOutputLimit);
    const server = await startServer(
      name,
      "cat",
      [],
      "generation-a",
      normalOutputLimit,
    );
    const slow = await openMachine(name);
    const healthy = await openMachine(name);
    const clients = (server as unknown as {
      clients: Map<net.Socket, { role: { _tag: string }; socket: net.Socket }>;
      broadcastMachine: (response: { _tag: "Data"; bytes: Buffer }) => void;
    }).clients;
    const slowServerClient = [...clients.values()].find(
      (client) => client.role._tag === "Machine" && client.socket.remotePort === slow.socket.localPort,
    );
    expect(slowServerClient).toBeDefined();

    const socket = slowServerClient!.socket;
    const captured: Buffer[] = [];
    let simulatedWritableLength = 0;
    let acceptWrites = false;
    let ended = false;
    const originalWrite = socket.write;
    const originalEnd = socket.end;
    Object.defineProperty(socket, "writableLength", {
      configurable: true,
      get: () => simulatedWritableLength,
    });
    socket.write = ((packet: string | Uint8Array) => {
      const copy = Buffer.from(packet);
      captured.push(copy);
      simulatedWritableLength = acceptWrites ? 0 : simulatedWritableLength + copy.length;
      return acceptWrites;
    }) as typeof socket.write;
    socket.end = (() => {
      ended = true;
      return socket;
    }) as typeof socket.end;

    try {
      const broadcast = (server as unknown as {
        broadcastMachine: (response: MachineResponse) => void;
      }).broadcastMachine.bind(server);
      broadcast(firstResponse);
      broadcast({ _tag: "Data", bytes: Buffer.from("overflow"), outputRevision: 2, inputModeRevision: 0 });

      await healthy.frames.waitFor((frames) =>
        frames.slice(2).filter((frame) => decodeMachineResponse(frame)._tag === "Data").length >= 2
      );
      expect(
        healthy.frames.frames.slice(2).map(decodeMachineResponse)
          .filter((response) => response._tag === "Data")
          .map((response) => response.bytes[0]),
      ).toEqual([0x61, 0x6f]);
      expect(captured).toHaveLength(1);

      simulatedWritableLength = 0;
      acceptWrites = true;
      socket.emit("drain");
      expect(captured.map((frame) => decodeMachineResponse({
        type: frame.readUInt8(0),
        payload: frame.subarray(5),
      }))).toEqual([
        firstResponse,
        { _tag: "StreamFailure", phase: "stream", reason: "slow-consumer" },
      ]);
      expect(ended).toBe(true);
    } finally {
      socket.write = originalWrite;
      socket.end = originalEnd;
      delete (socket as unknown as { writableLength?: number }).writableLength;
      slow.socket.destroy();
      healthy.socket.destroy();
    }
  });
});
