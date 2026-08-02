import * as net from "node:net";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { spawn } from "node:child_process";
import { PassThrough, Writable } from "node:stream";
import { afterEach, describe, expect, it } from "vitest";
import { machineAttachV2 } from "../src/machine-attach.ts";
import { attach, InteractiveInputPolicy } from "../src/client.ts";
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
  schema: "pty.input-mode.v1",
  wireEncoder: "xterm-input.v1",
  revision: 1,
  applicationCursorKeys: false,
  applicationKeypad: false,
  bracketedPaste: true,
  focusReporting: false,
  modifyOtherKeys: 0,
  mouseTracking: "Off",
  mouseEncoding: "Sgr",
  mouseCoordinates: "Cell",
  kittyKeyboardFlagsStack: [],
};

const advancedModes: MachineInputModeSnapshotV1 = {
  ...modes,
  revision: 2,
  bracketedPaste: false,
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

async function waitFor(predicate: () => boolean, description: string): Promise<void> {
  for (let attempt = 0; attempt < 200; attempt++) {
    if (predicate()) return;
    await new Promise<void>((resolve) => setTimeout(resolve, 5));
  }
  throw new Error(`Timed out waiting for ${description}`);
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
                outputRevision: 0,
                rows: open.rows,
                cols: open.cols,
                inputModes: modes,
                screen: Buffer.from("$ "),
              }),
            ]));
          } else {
            const request = decodeMachineRequest(packet);
            if (request._tag === "Input") {
              receivedInput.push(Buffer.from(request.bytes));
              socket.write(encodeMachineResponse({
                _tag: "Data",
                outputRevision: 1,
                inputModeRevision: modes.revision,
                bytes: Buffer.from("echo"),
              }));
            }
            if (request._tag === "Resize") {
              receivedSizes.push({ rows: request.rows, cols: request.cols });
              socket.write(encodeMachineResponse({
                _tag: "Data",
                outputRevision: 2,
                inputModeRevision: advancedModes.revision,
                inputModes: advancedModes,
                bytes: Buffer.from("mode"),
              }));
            }
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
    expect(run.responses.map((response) => response._tag)).toEqual(["Hello", "Ready", "Data", "Data", "Detached"]);
    expect(run.responses.slice(2, 4)).toEqual([
      {
        _tag: "Data",
        outputRevision: 1,
        inputModeRevision: modes.revision,
        bytes: Buffer.from("echo"),
      },
      {
        _tag: "Data",
        outputRevision: 2,
        inputModeRevision: advancedModes.revision,
        inputModes: advancedModes,
        bytes: Buffer.from("mode"),
      },
    ]);
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
                outputRevision: 0,
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

describe("interactive attach input policy", () => {
  it("preserves UTF-8 and control bytes across arbitrary stdin fragmentation", () => {
    const expected = Buffer.from("A😀\x1b[1;3D終");
    const emitted: Buffer[] = [];
    const policy = new InteractiveInputPolicy({
      onInput: (bytes) => emitted.push(bytes),
      onDetach: () => { throw new Error("unexpected detach"); },
    });
    for (const byte of expected) policy.feed(Buffer.from([byte]));
    policy.end();
    expect(Buffer.concat(emitted)).toEqual(expected);
  });

  it("recognizes Kitty Ctrl-\\ across every chunk boundary and forwards a double tap exactly", () => {
    const kitty = Buffer.from("\x1b[92;5u");
    for (let split = 1; split < kitty.length; split++) {
      const emitted: Buffer[] = [];
      const policy = new InteractiveInputPolicy({
        onInput: (bytes) => emitted.push(bytes),
        onDetach: () => { throw new Error("unexpected detach"); },
        ambiguityMs: 10_000,
      });
      policy.setInputModes({ ...modes, kittyKeyboardFlagsStack: [1] });
      policy.feed(kitty.subarray(0, split));
      policy.feed(kitty.subarray(split));
      policy.feed(kitty);
      expect(Buffer.concat(emitted)).toEqual(Buffer.from([0x1c]));
      policy.dispose();
    }
  });

  it("preserves causal byte order when ordinary input and a double detach chord are coalesced", () => {
    const emitted: Buffer[] = [];
    const policy = new InteractiveInputPolicy({
      onInput: (bytes) => emitted.push(bytes),
      onDetach: () => { throw new Error("unexpected detach"); },
    });
    policy.setInputModes({ ...modes, kittyKeyboardFlagsStack: [1] });
    policy.feed(Buffer.from("😀\x1bb\x1b[92;5u\x1b[92;5u"));
    expect(Buffer.concat(emitted)).toEqual(Buffer.from("😀\x1bb\x1c"));
    policy.dispose();
  });

  it("does not reserve Kitty-looking bytes when Kitty keyboard mode is inactive", () => {
    const bytes = Buffer.from("before\x1b[92;5uafter");
    const emitted: Buffer[] = [];
    const policy = new InteractiveInputPolicy({
      onInput: (value) => emitted.push(value),
      onDetach: () => { throw new Error("unexpected detach"); },
    });
    policy.feed(bytes);
    policy.end();
    expect(Buffer.concat(emitted)).toEqual(bytes);
  });

  it("releases an ambiguous Kitty prefix byte-for-byte when its deadline expires", async () => {
    const emitted: Buffer[] = [];
    const policy = new InteractiveInputPolicy({
      onInput: (value) => emitted.push(value),
      onDetach: () => { throw new Error("unexpected detach"); },
      ambiguityMs: 1,
    });
    policy.setInputModes({ ...modes, kittyKeyboardFlagsStack: [1] });
    policy.feed(Buffer.from("\x1b["));
    await new Promise((resolve) => setTimeout(resolve, 10));
    policy.feed(Buffer.from("1;3D"));
    policy.end();
    expect(Buffer.concat(emitted)).toEqual(Buffer.from("\x1b[1;3D"));
  });

  it("discards only incomplete ordinary input at an uncertain transport boundary", () => {
    const emitted: Buffer[] = [];
    const policy = new InteractiveInputPolicy({
      onInput: (value) => emitted.push(value),
      onDetach: () => { throw new Error("unexpected detach"); },
    });
    policy.feed(Buffer.from("😀").subarray(0, 2));
    policy.discardPendingInput();
    policy.feed(Buffer.from("A"));
    policy.end();
    expect(Buffer.concat(emitted)).toEqual(Buffer.from("A"));
  });
});

describe("interactive attach lifecycle", () => {
  it("cancels the admitted socket when local detach wins during Opening", async () => {
    let serverSocketClosed = false;
    const connect = await listen((socket) => {
      socket.once("close", () => { serverSocketClosed = true; });
      const reader = new MachineFrameReader();
      let opened: MachineOpenV2 | null = null;
      socket.on("data", (chunk) => {
        for (const packet of reader.feed(Buffer.from(chunk))) {
          if (packet.type === DaemonExtensionType.OPEN_V2) {
            opened = decodeDaemonOpenV2(packet);
          } else if (packet.type === MessageType.STATUS) {
            if (!opened) throw new Error("STATUS arrived before OPEN_V2");
            socket.write(encodeDaemonAdmissionV2({
              _tag: "Accepted",
              protocol: MACHINE_PROTOCOL_VERSION,
              generation: opened.expectedGeneration,
              capabilities: opened.requiredCapabilities,
              build: { version: "test", dirty: false },
            }));
          }
        }
      });
    });
    const input = new PassThrough();
    const output = new PassThrough();
    let detached = false;
    const exitCodes: number[] = [];
    attach({
      name: "test-session",
      expectedGeneration: "generation-a",
      socket: connect("ignored"),
      input,
      output,
      onDetach: () => { detached = true; },
      onExit: (code) => { exitCodes.push(code); },
    });
    input.write(Buffer.from([0x1c]));
    await waitFor(() => detached, "Opening detach callback");
    await waitFor(() => serverSocketClosed, "admitted socket close");
    expect(exitCodes).toEqual([]);
    input.destroy();
    output.destroy();
  });

  it("completes detach locally when transport fails after the detach request", async () => {
    const connect = await listen((socket) => {
      const reader = new MachineFrameReader();
      let opened: MachineOpenV2 | null = null;
      socket.on("data", (chunk) => {
        for (const packet of reader.feed(Buffer.from(chunk))) {
          if (packet.type === DaemonExtensionType.OPEN_V2) {
            opened = decodeDaemonOpenV2(packet);
            continue;
          }
          if (packet.type === MessageType.STATUS) {
            if (!opened) throw new Error("STATUS arrived before OPEN_V2");
            socket.write(Buffer.concat([
              encodeDaemonAdmissionV2({
                _tag: "Accepted",
                protocol: MACHINE_PROTOCOL_VERSION,
                generation: opened.expectedGeneration,
                capabilities: opened.requiredCapabilities,
                build: { version: "test", dirty: false },
              }),
              encodeMachineResponse({
                _tag: "Ready",
                outputRevision: 0,
                rows: opened.rows,
                cols: opened.cols,
                inputModes: modes,
                screen: Buffer.from("READY"),
              }),
            ]));
            continue;
          }
          if (decodeMachineRequest(packet)._tag === "Detach") socket.destroy();
        }
      });
    });
    const input = new PassThrough();
    const output = new PassThrough();
    let rendered = "";
    output.on("data", (chunk) => { rendered += chunk.toString(); });
    let detached = false;
    const exitCodes: number[] = [];
    attach({
      name: "test-session",
      expectedGeneration: "generation-a",
      socket: connect("ignored"),
      input,
      output,
      onDetach: () => { detached = true; },
      onExit: (code) => { exitCodes.push(code); },
    });
    await waitFor(() => rendered.includes("READY"), "interactive READY");
    input.write(Buffer.from([0x1c]));
    await waitFor(() => detached, "local detach after transport failure");
    expect(rendered).toContain("[detached]");
    expect(exitCodes).toEqual([]);
    input.destroy();
    output.destroy();
  });

  it("aborts an in-flight reconnect before invoking a library detach callback", async () => {
    let drop: (() => void) | null = null;
    const connect = await listen((socket) => {
      drop = () => socket.destroy();
      const reader = new MachineFrameReader();
      let opened: MachineOpenV2 | null = null;
      socket.on("data", (chunk) => {
        for (const packet of reader.feed(Buffer.from(chunk))) {
          if (packet.type === DaemonExtensionType.OPEN_V2) {
            opened = decodeDaemonOpenV2(packet);
          } else if (packet.type === MessageType.STATUS) {
            if (!opened) throw new Error("STATUS arrived before OPEN_V2");
            socket.write(Buffer.concat([
              encodeDaemonAdmissionV2({
                _tag: "Accepted",
                protocol: MACHINE_PROTOCOL_VERSION,
                generation: opened.expectedGeneration,
                capabilities: opened.requiredCapabilities,
                build: { version: "test", dirty: false },
              }),
              encodeMachineResponse({
                _tag: "Ready",
                outputRevision: 0,
                rows: opened.rows,
                cols: opened.cols,
                inputModes: modes,
                screen: Buffer.from("READY"),
              }),
            ]));
          }
        }
      });
    });
    const input = new PassThrough();
    const output = new PassThrough();
    let rendered = "";
    output.on("data", (chunk) => { rendered += chunk.toString(); });
    let reconnectSignal: AbortSignal | null = null;
    let reconnectAborted = false;
    let detached = false;
    attach({
      name: "test-session",
      expectedGeneration: "generation-a",
      socket: connect("ignored"),
      input,
      output,
      reconnect: (signal) => new Promise((resolve) => {
        reconnectSignal = signal;
        signal.addEventListener("abort", () => {
          reconnectAborted = true;
          resolve(null);
        }, { once: true });
      }),
      onDetach: () => { detached = true; },
      onExit: (code) => { throw new Error(`unexpected exit ${code}`); },
    });
    await waitFor(() => rendered.includes("READY"), "interactive READY");
    drop!();
    await waitFor(() => reconnectSignal !== null, "in-flight reconnect");
    input.write(Buffer.from([0x1c]));
    await waitFor(() => detached, "library detach callback");
    expect(reconnectSignal!.aborted).toBe(true);
    expect(reconnectAborted).toBe(true);
    expect(rendered).toContain("[detached]");
    input.destroy();
    output.destroy();
  });
});
