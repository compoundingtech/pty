import * as net from "node:net";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { spawn } from "node:child_process";
import { PassThrough, Writable } from "node:stream";
import { afterEach, describe, expect, it } from "vitest";
import { machineAttachV2 } from "../src/machine-attach.ts";
import {
  MACHINE_PROTOCOL_VERSION,
  DaemonExtensionType,
  MachineFrameReader,
  decodeDaemonOpenV2,
  decodeMachineRequest,
  decodeMachineResponse,
  encodeDaemonAdmissionV2,
  encodeMachineRequest,
  encodeMachineResponse,
  type MachineInputModeSnapshotV1,
  type MachineOpenV2,
  type MachineResponse,
} from "../src/machine-protocol.ts";
import {
  MessageType,
  encodeStatusResponse,
} from "../src/protocol.ts";

const modes: MachineInputModeSnapshotV1 = {
  revision: 1,
  applicationCursorKeys: false,
  applicationKeypad: false,
  bracketedPaste: true,
  focusReporting: false,
  modifyOtherKeys: 0,
  mouseTracking: "none",
  mouseEncoding: "sgr",
  kittyKeyboardFlags: [],
};

const open: MachineOpenV2 = {
  _tag: "Open",
  protocol: MACHINE_PROTOCOL_VERSION,
  sessionId: "test-session",
  expectedGeneration: "generation-a",
  rows: 24,
  cols: 80,
  requiredCapabilities: ["framed-utf8-input", "typed-outcome", "input-mode-snapshot"],
};

const servers: net.Server[] = [];

afterEach(async () => {
  await Promise.all(servers.splice(0).map((server) => new Promise<void>((resolve) => server.close(() => resolve()))));
});

async function listen(handler: (socket: net.Socket) => void): Promise<(ignored: string) => net.Socket> {
  const server = net.createServer(handler);
  servers.push(server);
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("Expected TCP test address");
  return () => net.createConnection(address.port, "127.0.0.1");
}

function harness(connect: (socketPath: string) => net.Socket, signal?: AbortSignal) {
  const input = new PassThrough();
  const output = new PassThrough();
  const diagnostics = new PassThrough();
  const responses: MachineResponse[] = [];
  const reader = new MachineFrameReader();
  output.on("data", (chunk: Buffer) => {
    for (const frame of reader.feed(chunk)) responses.push(decodeMachineResponse(frame));
  });
  const completed = machineAttachV2({ input, output, diagnostics, connect, ...(signal ? { signal } : {}) });
  return { input, output, diagnostics, responses, completed };
}

async function waitForResponse(
  responses: MachineResponse[],
  tag: MachineResponse["_tag"],
): Promise<MachineResponse> {
  for (let attempt = 0; attempt < 100; attempt++) {
    const found = responses.find((response) => response._tag === tag);
    if (found) return found;
    await new Promise<void>((resolve) => setImmediate(resolve));
  }
  throw new Error(`Timed out waiting for ${tag}`);
}

describe("machine-attach-v2 adapter", () => {
  it("classifies an old daemon without sending ATTACH, RESIZE, or DATA", async () => {
    const received: number[] = [];
    const connect = await listen((socket) => {
      const reader = new MachineFrameReader();
      socket.on("data", (chunk) => {
        for (const packet of reader.feed(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk))) {
          received.push(packet.type);
          if (packet.type === MessageType.STATUS) socket.write(encodeStatusResponse("{}"));
        }
      });
    });
    const run = harness(connect);
    run.input.write(encodeMachineRequest(open));

    await expect(run.completed).resolves.toMatchObject({
      _tag: "AdmissionFailure",
      reason: "unsupported-daemon",
    });
    expect(received).toEqual([8, MessageType.STATUS]);
    expect(run.responses).toHaveLength(1);
    expect(run.responses[0]).toMatchObject({ _tag: "AdmissionFailure", reason: "unsupported-daemon" });
  });

  it("proxies valid UTF-8 and control bytes exactly and detaches only on the explicit frame", async () => {
    const receivedInput: Buffer[] = [];
    const receivedSizes: Array<{ rows: number; cols: number }> = [];
    let openSeen = false;
    const connect = await listen((socket) => {
      const reader = new MachineFrameReader();
      socket.on("data", (chunk) => {
        for (const packet of reader.feed(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk))) {
          if (packet.type === DaemonExtensionType.OPEN_V2) {
            expect(decodeDaemonOpenV2(packet)).toEqual(open);
            openSeen = true;
          } else if (packet.type === MessageType.STATUS) {
            expect(openSeen).toBe(true);
            socket.write(Buffer.concat([
              encodeDaemonAdmissionV2({
                _tag: "Accepted",
                protocol: MACHINE_PROTOCOL_VERSION,
                generation: open.expectedGeneration,
                capabilities: open.requiredCapabilities,
                build: { version: "test", dirty: false },
              }),
              encodeMachineResponse({
                _tag: "Ready",
                rows: open.rows,
                cols: open.cols,
                inputModes: modes,
                screen: Buffer.from("$ "),
              }),
            ]));
          } else {
            const request = decodeMachineRequest(packet);
            if (request._tag === "Input") receivedInput.push(Buffer.from(request.bytes));
            if (request._tag === "Resize") receivedSizes.push({ rows: request.rows, cols: request.cols });
            if (request._tag === "Detach") socket.write(encodeMachineResponse({ _tag: "Detached" }));
          }
        }
      });
    });
    const run = harness(connect);
    run.input.write(encodeMachineRequest(open));
    await waitForResponse(run.responses, "Ready");

    const bytes = Buffer.from([0x31, 0x32, 0x33, 0x1b, 0x62, 0x1c]);
    run.input.write(encodeMachineRequest({ _tag: "Input", bytes }));
    run.input.write(encodeMachineRequest({ _tag: "Resize", rows: 30, cols: 100 }));
    run.input.write(encodeMachineRequest({ _tag: "Detach" }));

    await expect(run.completed).resolves.toEqual({ _tag: "Detached" });
    expect(openSeen).toBe(true);
    expect(Buffer.concat(receivedInput)).toEqual(bytes);
    expect(receivedSizes).toEqual([{ rows: 30, cols: 100 }]);
    expect(run.responses.map((response) => response._tag)).toEqual(["Hello", "Ready", "Detached"]);
  });

  it("turns a same-connection generation rejection into one typed outcome", async () => {
    const connect = await listen((socket) => {
      const reader = new MachineFrameReader();
      socket.on("data", (chunk) => {
        for (const packet of reader.feed(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk))) {
          if (packet.type === MessageType.STATUS) {
            socket.write(encodeDaemonAdmissionV2({
              _tag: "Rejected",
              reason: "generation-mismatch",
              detail: "the session was replaced",
            }));
          }
        }
      });
    });
    const run = harness(connect);
    run.input.write(encodeMachineRequest(open));

    await expect(run.completed).resolves.toMatchObject({
      _tag: "AdmissionFailure",
      reason: "generation-mismatch",
    });
    expect(run.responses).toHaveLength(1);
  });

  it.each([
    {
      name: "a falsely accepted generation",
      generation: "generation-b",
      capabilities: open.requiredCapabilities,
      reason: "generation-mismatch",
    },
    {
      name: "an acceptance missing a required capability",
      generation: open.expectedGeneration,
      capabilities: ["framed-utf8-input", "typed-outcome"] as const,
      reason: "unsupported-capability",
    },
  ])("rejects $name before HELLO", async ({ generation, capabilities, reason }) => {
    const connect = await listen((socket) => {
      const reader = new MachineFrameReader();
      socket.on("data", (chunk) => {
        for (const packet of reader.feed(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk))) {
          if (packet.type === MessageType.STATUS) {
            socket.write(encodeDaemonAdmissionV2({
              _tag: "Accepted",
              protocol: MACHINE_PROTOCOL_VERSION,
              generation,
              capabilities,
              build: { version: "test", dirty: false },
            }));
          }
        }
      });
    });
    const run = harness(connect);
    run.input.write(encodeMachineRequest(open));

    await expect(run.completed).resolves.toMatchObject({ _tag: "AdmissionFailure", reason });
    expect(run.responses).toHaveLength(1);
    expect(run.responses[0]._tag).toBe("AdmissionFailure");
  });

  it("accepts an explicit detach after HELLO while the baseline is pending", async () => {
    const connect = await listen((socket) => {
      const reader = new MachineFrameReader();
      socket.on("data", (chunk) => {
        for (const packet of reader.feed(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk))) {
          if (packet.type === DaemonExtensionType.OPEN_V2) {
            expect(decodeDaemonOpenV2(packet)).toEqual(open);
          } else if (packet.type === MessageType.STATUS) {
            socket.write(encodeDaemonAdmissionV2({
              _tag: "Accepted",
              protocol: MACHINE_PROTOCOL_VERSION,
              generation: open.expectedGeneration,
              capabilities: open.requiredCapabilities,
              build: { version: "test", dirty: false },
            }));
          } else {
            const request = decodeMachineRequest(packet);
            if (request._tag === "Detach") socket.write(encodeMachineResponse({ _tag: "Detached" }));
          }
        }
      });
    });
    const run = harness(connect);
    run.input.write(encodeMachineRequest(open));
    await waitForResponse(run.responses, "Hello");
    run.input.write(encodeMachineRequest({ _tag: "Detach" }));

    await expect(run.completed).resolves.toEqual({ _tag: "Detached" });
    expect(run.responses.map((response) => response._tag)).toEqual(["Hello", "Detached"]);
  });

  it("rejects a nonempty DETACHED frame after accepted admission", async () => {
    const connect = await listen((socket) => {
      const reader = new MachineFrameReader();
      socket.on("data", (chunk) => {
        for (const packet of reader.feed(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk))) {
          if (packet.type === MessageType.STATUS) {
            const malformedDetached = Buffer.alloc(6);
            malformedDetached.writeUInt8(7, 0);
            malformedDetached.writeUInt32BE(1, 1);
            malformedDetached.writeUInt8(1, 5);
            socket.write(Buffer.concat([
              encodeDaemonAdmissionV2({
                _tag: "Accepted",
                protocol: MACHINE_PROTOCOL_VERSION,
                generation: open.expectedGeneration,
                capabilities: open.requiredCapabilities,
                build: { version: "test", dirty: false },
              }),
              malformedDetached,
            ]));
          }
        }
      });
    });
    const run = harness(connect);
    run.input.write(encodeMachineRequest(open));

    await expect(run.completed).resolves.toMatchObject({
      _tag: "StreamFailure",
      phase: "baseline",
      reason: "malformed-daemon-frame",
    });
    expect(run.responses.map((response) => response._tag)).toEqual(["Hello", "StreamFailure"]);
  });

  it("treats stdin EOF after explicit DETACH as a half-close", async () => {
    const connect = await listen((socket) => {
      const reader = new MachineFrameReader();
      socket.on("data", (chunk) => {
        for (const packet of reader.feed(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk))) {
          if (packet.type === DaemonExtensionType.OPEN_V2) {
            expect(decodeDaemonOpenV2(packet)).toEqual(open);
          } else if (packet.type === MessageType.STATUS) {
            socket.write(Buffer.concat([
              encodeDaemonAdmissionV2({
                _tag: "Accepted",
                protocol: MACHINE_PROTOCOL_VERSION,
                generation: open.expectedGeneration,
                capabilities: open.requiredCapabilities,
                build: { version: "test", dirty: false },
              }),
              encodeMachineResponse({
                _tag: "Ready",
                rows: open.rows,
                cols: open.cols,
                inputModes: modes,
                screen: Buffer.alloc(0),
              }),
            ]));
          } else {
            const request = decodeMachineRequest(packet);
            if (request._tag === "Detach") {
              setImmediate(() => socket.write(encodeMachineResponse({ _tag: "Detached" })));
            }
          }
        }
      });
    });
    const run = harness(connect);
    run.input.write(encodeMachineRequest(open));
    await waitForResponse(run.responses, "Ready");
    run.input.end(encodeMachineRequest({ _tag: "Detach" }));

    await expect(run.completed).resolves.toEqual({ _tag: "Detached" });
    expect(run.responses.at(-1)).toEqual({ _tag: "Detached" });
  });

  it("rejects coalesced OPEN plus INPUT before daemon admission", async () => {
    const receivedInput: Buffer[] = [];
    const connect = await listen((socket) => {
      const reader = new MachineFrameReader();
      socket.on("data", (chunk) => {
        for (const packet of reader.feed(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk))) {
          if (packet.type !== DaemonExtensionType.OPEN_V2 && packet.type !== MessageType.STATUS) {
            const request = decodeMachineRequest(packet);
            if (request._tag === "Input") receivedInput.push(request.bytes);
          }
        }
      });
    });
    const run = harness(connect);
    run.input.write(Buffer.concat([
      encodeMachineRequest(open),
      encodeMachineRequest({ _tag: "Input", bytes: Buffer.from("early") }),
    ]));

    await expect(run.completed).resolves.toMatchObject({
      _tag: "AdmissionFailure",
      reason: "malformed-request",
    });
    expect(receivedInput).toEqual([]);
    expect(run.responses).toHaveLength(1);
  });

  it("reports connection refusal as an admission transport failure", async () => {
    const probe = net.createServer();
    await new Promise<void>((resolve) => probe.listen(0, "127.0.0.1", resolve));
    const address = probe.address();
    if (!address || typeof address === "string") throw new Error("Expected TCP test address");
    const port = address.port;
    await new Promise<void>((resolve) => probe.close(() => resolve()));

    const run = harness(() => net.createConnection(port, "127.0.0.1"));
    run.input.write(encodeMachineRequest(open));
    await expect(run.completed).resolves.toMatchObject({
      _tag: "AdmissionFailure",
      reason: "not-found",
    });
    expect(run.responses).toHaveLength(1);
  });

  it("turns explicit cancellation into a terminal framed result", async () => {
    const controller = new AbortController();
    const run = harness(() => { throw new Error("must not connect"); }, controller.signal);
    controller.abort();

    await expect(run.completed).resolves.toMatchObject({
      _tag: "AdmissionFailure",
      reason: "transport-failure",
    });
    expect(run.responses).toHaveLength(1);
  });

  it("rejects promptly when stdout cannot carry the canonical result", async () => {
    const input = new PassThrough();
    const diagnostics = new PassThrough();
    let diagnosticText = "";
    diagnostics.on("data", (chunk) => { diagnosticText += chunk.toString(); });
    const output = new Writable({
      write(_chunk, _encoding, callback) {
        callback(new Error("closed consumer"));
      },
    });
    const completed = machineAttachV2({
      input,
      output,
      diagnostics,
      connect: () => { throw new Error("must not connect"); },
    });
    input.write(encodeMachineRequest({ _tag: "Detach" }));

    await expect(completed).rejects.toThrow("closed consumer");
    expect(diagnosticText).toContain("stdout failed: closed consumer");
  });

  it("does not wait forever when stdout closes before a pending write completes", async () => {
    const input = new PassThrough();
    const diagnostics = new PassThrough();
    const output = new Writable({ write() {} });
    const completed = machineAttachV2({
      input,
      output,
      diagnostics,
      connect: () => { throw new Error("must not connect"); },
    });
    input.write(encodeMachineRequest({ _tag: "Detach" }));
    setImmediate(() => output.destroy());

    await expect(completed).rejects.toThrow("stream closed before write completed");
  });

  it("uses exit zero when the subprocess delivered a typed admission outcome", async () => {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), "pty-machine-adapter-"));
    const socketPath = path.join(root, `${open.sessionId}.sock`);
    const server = net.createServer((socket) => {
      const reader = new MachineFrameReader();
      socket.on("data", (chunk) => {
        for (const packet of reader.feed(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk))) {
          if (packet.type === MessageType.STATUS) socket.write(encodeStatusResponse("{}"));
        }
      });
    });
    servers.push(server);
    await new Promise<void>((resolve, reject) => {
      server.once("error", reject);
      server.listen(socketPath, resolve);
    });

    const child = spawn(process.execPath, [path.join(process.cwd(), "dist", "cli.js"), "machine-attach-v2"], {
      env: { ...process.env, PTY_ROOT: root },
      stdio: ["pipe", "pipe", "pipe"],
    });
    const stdout: Buffer[] = [];
    const stderr: Buffer[] = [];
    child.stdout.on("data", (chunk) => stdout.push(Buffer.from(chunk)));
    child.stderr.on("data", (chunk) => stderr.push(Buffer.from(chunk)));
    child.stdin.write(encodeMachineRequest(open));
    const [code] = await new Promise<[number | null]>((resolve) => child.once("exit", (value) => resolve([value])));

    const reader = new MachineFrameReader();
    const responses = reader.feed(Buffer.concat(stdout)).map(decodeMachineResponse);
    expect(code).toBe(0);
    expect(responses).toHaveLength(1);
    expect(responses[0]).toMatchObject({ _tag: "AdmissionFailure", reason: "unsupported-daemon" });
    expect(Buffer.concat(stderr).toString()).toBe("");
    fs.rmSync(root, { recursive: true, force: true });
  });
});
