import { Buffer } from "node:buffer";
import { describe, expect, it } from "vitest";
import {
  MACHINE_PROTOCOL_VERSION,
  MachineFrameReader,
  MachineProtocolError,
  decodeDaemonAdmissionV2,
  decodeDaemonOpenV2,
  decodeMachineRequest,
  decodeMachineResponse,
  encodeDaemonAdmissionV2,
  encodeDaemonOpenV2,
  encodeMachineRequest,
  encodeMachineResponse,
  reduceMachineAttach,
  type DaemonAdmissionV2,
  type MachineHelloV2,
  type MachineInputModeSnapshotV1,
  type MachineOpenV2,
} from "../src/machine-protocol.ts";

const modes: MachineInputModeSnapshotV1 = {
  schema: "pty.input-mode.v1",
  wireEncoder: "xterm-input.v1",
  revision: 7,
  applicationCursorKeys: true,
  applicationKeypad: false,
  bracketedPaste: true,
  focusReporting: false,
  modifyOtherKeys: 2,
  mouseTracking: "AnyMotion",
  mouseEncoding: "Sgr",
  mouseCoordinates: "Cell",
  kittyKeyboardFlagsStack: [1, 15],
};

const open: MachineOpenV2 = {
  _tag: "Open",
  protocol: MACHINE_PROTOCOL_VERSION,
  sessionId: "session-1",
  expectedGeneration: "generation-a",
  rows: 24,
  cols: 80,
  requiredCapabilities: ["framed-utf8-input", "typed-outcome", "input-mode-snapshot"],
};

describe("machine attach v2 protocol", () => {
  it("round-trips fragmented and coalesced directional frames", () => {
    const input = Buffer.from([0x1b, 0x62, 0xff, 0x00, 0x1c]);
    const wire = Buffer.concat([
      encodeMachineRequest(open),
      encodeMachineRequest({ _tag: "Input", bytes: input }),
      encodeMachineRequest({ _tag: "Detach" }),
    ]);
    const reader = new MachineFrameReader();
    expect(reader.feed(wire.subarray(0, 3))).toEqual([]);
    const frames = [...reader.feed(wire.subarray(3, 17)), ...reader.feed(wire.subarray(17))].map(decodeMachineRequest);

    expect(frames).toEqual([open, { _tag: "Input", bytes: input }, { _tag: "Detach" }]);
  });

  it("round-trips many deterministic fragmentations of a coalesced stream", () => {
    const expected = [
      open,
      { _tag: "Input" as const, bytes: Buffer.from([0x00, 0xff, 0x1b, 0x62, 0x1c]) },
      { _tag: "Resize" as const, rows: 60, cols: 180 },
      { _tag: "Detach" as const },
    ];
    const wire = Buffer.concat(expected.map(encodeMachineRequest));

    for (let seed = 1; seed <= 64; seed += 1) {
      const reader = new MachineFrameReader();
      const frames = [];
      let offset = 0;
      let state = seed;
      while (offset < wire.length) {
        state = (state * 1664525 + 1013904223) >>> 0;
        const length = 1 + (state % 23);
        frames.push(...reader.feed(wire.subarray(offset, offset + length)));
        offset += length;
      }
      expect(frames.map(decodeMachineRequest)).toEqual(expected);
    }
  });

  it("preserves arbitrary bytes within the framed transport", () => {
    const bytes = Buffer.from([0x00, 0xff, 0x1b, 0x62, 0x1c]);
    for (const frame of [
      { _tag: "Data" as const, outputRevision: 8, inputModeRevision: 7, bytes },
      {
        _tag: "Ready" as const,
        outputRevision: 7,
        rows: 24,
        cols: 80,
        inputModes: modes,
        screen: bytes,
      },
    ]) {
      const reader = new MachineFrameReader();
      const [wireFrame] = reader.feed(encodeMachineResponse(frame));
      const decoded = decodeMachineResponse(wireFrame);
      expect(decoded._tag).toBe(frame._tag);
      if (decoded._tag === "Data") expect(decoded.bytes).toEqual(bytes);
      else if (decoded._tag === "Ready") expect(decoded.screen).toEqual(bytes);
      else throw new Error(`unexpected decoded frame ${decoded._tag}`);
    }
  });

  it("round-trips same-connection daemon admission without weakening generation", () => {
    const daemonReader = new MachineFrameReader();
    const [openPacket] = daemonReader.feed(encodeDaemonOpenV2(open));
    expect(decodeDaemonOpenV2(openPacket)).toEqual(open);

    const accepted: DaemonAdmissionV2 = {
      _tag: "Accepted" as const,
      protocol: MACHINE_PROTOCOL_VERSION,
      generation: "generation-a",
      capabilities: ["framed-utf8-input", "typed-outcome", "input-mode-snapshot"],
      build: { version: "0.12.0", revision: "abc123", dirty: false },
    };
    const [admissionPacket] = daemonReader.feed(encodeDaemonAdmissionV2(accepted));
    expect(decodeDaemonAdmissionV2(admissionPacket)).toEqual(accepted);
  });

  it("rejects malformed, unknown and oversized frames", () => {
    const unknown = Buffer.alloc(5);
    unknown.writeUInt8(255, 0);
    expect(() => decodeMachineRequest(new MachineFrameReader().feed(unknown)[0])).toThrow(MachineProtocolError);

    const oversized = Buffer.alloc(5);
    oversized.writeUInt8(1, 0);
    oversized.writeUInt32BE(32 * 1024 * 1024 + 1, 1);
    const poisonedReader = new MachineFrameReader();
    let initialError: unknown;
    try {
      poisonedReader.feed(oversized);
    } catch (error) {
      initialError = error;
    }
    expect(initialError).toBeInstanceOf(MachineProtocolError);
    let subsequentError: unknown;
    try {
      poisonedReader.feed(encodeMachineRequest({ _tag: "Detach" }));
    } catch (error) {
      subsequentError = error;
    }
    expect(subsequentError).toBe(initialError);

    expect(() => encodeMachineRequest({ ...open, rows: 0 })).toThrow(MachineProtocolError);

    const extraField = Buffer.from(JSON.stringify({ ...open, legacy: true }));
    const header = Buffer.alloc(5);
    header.writeUInt8(1, 0);
    header.writeUInt32BE(extraField.length, 1);
    const [frame] = new MachineFrameReader().feed(Buffer.concat([header, extraField]));
    expect(() => decodeMachineRequest(frame)).toThrow(/unknown field legacy/);

    const invalidUtf8 = Buffer.from([1, 0, 0, 0, 1, 0xff]);
    const [invalidFrame] = new MachineFrameReader().feed(invalidUtf8);
    expect(() => decodeMachineRequest(invalidFrame)).toThrow(/Invalid machine protocol JSON/);

    expect(() =>
      encodeMachineResponse({
        _tag: "Data",
        outputRevision: 8,
        inputModeRevision: 7,
        inputModes: { ...modes, modifyOtherKeys: 3 as 2 },
        bytes: Buffer.from("x"),
      }),
    ).toThrow(/modifyOtherKeys/);
  });

  it("round-trips the complete key and mouse mode vocabulary", () => {
    const inputModes: MachineInputModeSnapshotV1 = {
      ...modes,
      mouseTracking: "X10Press",
      mouseEncoding: "Sgr",
      mouseCoordinates: "Pixel",
      modifyOtherKeys: 1,
    };
    const [frame] = new MachineFrameReader().feed(
      encodeMachineResponse({
        _tag: "Data",
        outputRevision: 8,
        inputModeRevision: inputModes.revision,
        inputModes,
        bytes: Buffer.from("mode-change"),
      })
    );
    expect(decodeMachineResponse(frame)).toEqual({
      _tag: "Data",
      outputRevision: 8,
      inputModeRevision: inputModes.revision,
      inputModes,
      bytes: Buffer.from("mode-change"),
    });
  });

  it("accepts only HELLO then READY then updates then one outcome then EOF", () => {
    const hello: MachineHelloV2 = {
      _tag: "Hello" as const,
      protocol: MACHINE_PROTOCOL_VERSION,
      generation: "generation-a",
      capabilities: ["framed-utf8-input", "typed-outcome", "input-mode-snapshot"],
      build: { version: "0.12.0", dirty: false },
    };
    const ready = {
      _tag: "Ready" as const,
      outputRevision: 41,
      rows: 24,
      cols: 80,
      inputModes: modes,
      screen: Buffer.from("$ "),
    };
    const data = {
      _tag: "Data" as const,
      outputRevision: 42,
      inputModeRevision: 7,
      bytes: Buffer.from("ok"),
    };
    const exited = { _tag: "Exited" as const, code: 0, signal: null };

    let state = reduceMachineAttach({ _tag: "AwaitAdmission" }, { _tag: "Frame", frame: hello });
    state = reduceMachineAttach(state, { _tag: "Frame", frame: ready });
    state = reduceMachineAttach(state, { _tag: "Frame", frame: data });
    state = reduceMachineAttach(state, { _tag: "Frame", frame: exited });
    state = reduceMachineAttach(state, { _tag: "Eof" });
    expect(state).toEqual({ _tag: "Complete", outcome: exited });
  });

  it("admits a typed rejection only before HELLO and requires EOF afterward", () => {
    const rejected = { _tag: "AdmissionFailure" as const, reason: "generation-mismatch" as const, detail: "replaced" };
    let state = reduceMachineAttach({ _tag: "AwaitAdmission" }, { _tag: "Frame", frame: rejected });
    expect(state._tag).toBe("AwaitEof");
    state = reduceMachineAttach(state, { _tag: "Eof" });
    expect(state).toEqual({ _tag: "Complete", outcome: rejected });
  });

  it("allows every terminal stream outcome after HELLO and before READY", () => {
    const hello: MachineHelloV2 = {
      _tag: "Hello" as const,
      protocol: MACHINE_PROTOCOL_VERSION,
      generation: "generation-a",
      capabilities: ["typed-outcome" as const],
      build: { version: "0.12.0", dirty: false },
    };
    const awaiting = reduceMachineAttach({ _tag: "AwaitAdmission" }, { _tag: "Frame", frame: hello });
    for (const outcome of [
      { _tag: "Exited" as const, code: 1, signal: null },
      { _tag: "Detached" as const },
      { _tag: "StreamFailure" as const, phase: "baseline" as const, reason: "screen unavailable" },
    ]) {
      const ended = reduceMachineAttach(awaiting, { _tag: "Frame", frame: outcome });
      expect(reduceMachineAttach(ended, { _tag: "Eof" })).toEqual({ _tag: "Complete", outcome });
    }
  });

  it("applies output and changed input mode as one causal DATA transition", () => {
    const hello: MachineHelloV2 = {
      _tag: "Hello" as const,
      protocol: MACHINE_PROTOCOL_VERSION,
      generation: "generation-a",
      capabilities: ["input-mode-snapshot" as const],
      build: { version: "0.12.0", dirty: false },
    };
    const awaitingReady = reduceMachineAttach({ _tag: "AwaitAdmission" }, { _tag: "Frame", frame: hello });
    const streaming = reduceMachineAttach(awaitingReady, {
      _tag: "Frame",
      frame: {
        _tag: "Ready",
        outputRevision: 41,
        rows: 24,
        cols: 80,
        inputModes: modes,
        screen: Buffer.alloc(0),
      },
    });
    expect(streaming).toEqual({ _tag: "Streaming", inputModeRevision: 7, outputRevision: 41 });
    const advanced = reduceMachineAttach(streaming, {
      _tag: "Frame",
      frame: {
        _tag: "Data",
        outputRevision: 42,
        inputModeRevision: 8,
        inputModes: { ...modes, revision: 8 },
        bytes: Buffer.from("set mode"),
      },
    });
    expect(advanced).toMatchObject({ _tag: "Streaming", inputModeRevision: 8, outputRevision: 42 });
    expect(
      reduceMachineAttach(advanced, {
        _tag: "Frame",
        frame: {
          _tag: "Data",
          outputRevision: 43,
          inputModeRevision: 9,
          bytes: Buffer.from("missing mode"),
        },
      }),
    ).toMatchObject({ _tag: "ProtocolViolation" });
  });

  it("classifies bare EOF and illegal ordering as protocol violations", () => {
    expect(reduceMachineAttach({ _tag: "AwaitAdmission" }, { _tag: "Eof" })).toMatchObject({
      _tag: "ProtocolViolation",
    });
    expect(
      reduceMachineAttach(
        { _tag: "AwaitAdmission" },
        {
          _tag: "Frame",
          frame: { _tag: "Data", outputRevision: 1, inputModeRevision: 0, bytes: Buffer.from("early") },
        },
      ),
    ).toMatchObject({ _tag: "ProtocolViolation" });
  });
});
