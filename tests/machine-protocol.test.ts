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
  revision: 7,
  applicationCursorKeys: true,
  applicationKeypad: false,
  bracketedPaste: true,
  focusReporting: false,
  mouseTracking: "any",
  mouseEncoding: "sgr",
  kittyKeyboardFlags: [1, 15],
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

  it("preserves arbitrary bytes within the framed transport", () => {
    const bytes = Buffer.from([0x00, 0xff, 0x1b, 0x62, 0x1c]);
    for (const frame of [
      { _tag: "Data" as const, bytes },
      {
        _tag: "Ready" as const,
        rows: 24,
        cols: 80,
        inputModes: modes,
        screen: bytes,
      },
      {
        _tag: "Snapshot" as const,
        rows: 30,
        cols: 100,
        inputModes: { ...modes, revision: 8 },
        screen: bytes,
      },
    ]) {
      const reader = new MachineFrameReader();
      const [wireFrame] = reader.feed(encodeMachineResponse(frame));
      const decoded = decodeMachineResponse(wireFrame);
      expect(decoded._tag).toBe(frame._tag);
      if (decoded._tag === "Data") expect(decoded.bytes).toEqual(bytes);
      else if (decoded._tag === "Ready" || decoded._tag === "Snapshot") expect(decoded.screen).toEqual(bytes);
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
    expect(() => new MachineFrameReader().feed(oversized)).toThrow(MachineProtocolError);

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
  });

  it("accepts only HELLO then READY then updates then one outcome then EOF", () => {
    const hello: MachineHelloV2 = {
      _tag: "Hello" as const,
      protocol: MACHINE_PROTOCOL_VERSION,
      generation: "generation-a",
      capabilities: ["framed-utf8-input", "typed-outcome", "input-mode-snapshot"],
      build: { version: "0.12.0", dirty: false },
    };
    const ready = { _tag: "Ready" as const, rows: 24, cols: 80, inputModes: modes, screen: Buffer.from("$ ") };
    const data = { _tag: "Data" as const, bytes: Buffer.from("ok") };
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

  it("orders input mode updates by their explicit revision", () => {
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
      frame: { _tag: "Ready", rows: 24, cols: 80, inputModes: modes, screen: Buffer.alloc(0) },
    });
    const advanced = reduceMachineAttach(streaming, {
      _tag: "Frame",
      frame: { _tag: "InputModes", inputModes: { ...modes, revision: 8 } },
    });
    expect(advanced).toMatchObject({ _tag: "Streaming", inputModeRevision: 8 });
    expect(
      reduceMachineAttach(advanced, {
        _tag: "Frame",
        frame: { _tag: "InputModes", inputModes: { ...modes, revision: 8 } },
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
        { _tag: "Frame", frame: { _tag: "Data", bytes: Buffer.from("early") } },
      ),
    ).toMatchObject({ _tag: "ProtocolViolation" });
  });
});
