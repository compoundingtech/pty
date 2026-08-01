import { Buffer } from "node:buffer";

export const MACHINE_PROTOCOL_VERSION = 2 as const;
export const MAX_MACHINE_FRAME_LENGTH = 32 * 1024 * 1024;

const HEADER_SIZE = 5;
const MAX_STRUCTURED_PAYLOAD_LENGTH = 64 * 1024;
const MAX_STRING_LENGTH = 4096;
const MAX_CAPABILITIES = 32;
const MAX_KITTY_STACK_DEPTH = 64;

export const MachineRequestType = {
  OPEN: 1,
  INPUT: 2,
  RESIZE: 3,
  DETACH: 4,
} as const;

export const MachineResponseType = {
  HELLO: 1,
  READY: 2,
  DATA: 3,
  GEOMETRY: 4,
  SNAPSHOT: 5,
  EXITED: 6,
  DETACHED: 7,
  ADMISSION_FAILURE: 8,
  STREAM_FAILURE: 9,
  INPUT_MODES: 10,
} as const;

/** Reserved packet types on the existing daemon socket. */
export const DaemonExtensionType = {
  OPEN_V2: 8,
  ADMISSION_V2: 9,
} as const;

export type MachineCapability = "framed-utf8-input" | "typed-outcome" | "input-mode-snapshot";

export interface MachineOpenV2 {
  readonly _tag: "Open";
  readonly protocol: typeof MACHINE_PROTOCOL_VERSION;
  readonly sessionId: string;
  readonly expectedGeneration: string;
  readonly rows: number;
  readonly cols: number;
  readonly requiredCapabilities: readonly MachineCapability[];
}

export type MachineRequest =
  | MachineOpenV2
  | { readonly _tag: "Input"; readonly bytes: Buffer }
  | { readonly _tag: "Resize"; readonly rows: number; readonly cols: number }
  | { readonly _tag: "Detach" };

export type MouseTrackingMode = "none" | "click" | "button" | "any";
export type MouseEncodingMode = "x10" | "utf8" | "sgr" | "urxvt";

/** Complete child-input state at one ordered terminal-stream revision. */
export interface MachineInputModeSnapshotV1 {
  readonly revision: number;
  readonly applicationCursorKeys: boolean;
  readonly applicationKeypad: boolean;
  readonly bracketedPaste: boolean;
  readonly focusReporting: boolean;
  readonly mouseTracking: MouseTrackingMode;
  readonly mouseEncoding: MouseEncodingMode;
  readonly kittyKeyboardFlags: readonly number[];
}

export interface MachineBuildInfo {
  readonly version: string;
  readonly revision?: string;
  readonly dirty: boolean;
}

export interface MachineHelloV2 {
  readonly _tag: "Hello";
  readonly protocol: typeof MACHINE_PROTOCOL_VERSION;
  readonly generation: string;
  readonly capabilities: readonly MachineCapability[];
  readonly build: MachineBuildInfo;
}

export interface MachineReadyV2 {
  readonly _tag: "Ready";
  readonly rows: number;
  readonly cols: number;
  readonly inputModes: MachineInputModeSnapshotV1;
  readonly screen: Buffer;
}

export type AdmissionFailureReason =
  | "unsupported-daemon"
  | "generation-mismatch"
  | "not-found"
  | "permission-denied"
  | "unsupported-capability"
  | "transport-failure"
  | "malformed-request";

export type StreamFailurePhase = "baseline" | "stream" | "shutdown";

export type MachineOutcome =
  | { readonly _tag: "Exited"; readonly code: number; readonly signal: string | null }
  | { readonly _tag: "Detached" }
  | {
      readonly _tag: "AdmissionFailure";
      readonly reason: AdmissionFailureReason;
      readonly detail?: string;
    }
  | {
      readonly _tag: "StreamFailure";
      readonly phase: StreamFailurePhase;
      readonly reason: string;
      readonly diagnostic?: string;
    };

export type MachineResponse =
  | MachineHelloV2
  | MachineReadyV2
  | { readonly _tag: "Data"; readonly bytes: Buffer }
  | { readonly _tag: "Geometry"; readonly rows: number; readonly cols: number }
  | {
      readonly _tag: "Snapshot";
      readonly rows: number;
      readonly cols: number;
      readonly inputModes: MachineInputModeSnapshotV1;
      readonly screen: Buffer;
    }
  | { readonly _tag: "InputModes"; readonly inputModes: MachineInputModeSnapshotV1 }
  | MachineOutcome;

export type DaemonAdmissionV2 =
  | {
      readonly _tag: "Accepted";
      readonly protocol: typeof MACHINE_PROTOCOL_VERSION;
      readonly generation: string;
      readonly capabilities: readonly MachineCapability[];
      readonly build: MachineBuildInfo;
    }
  | {
      readonly _tag: "Rejected";
      readonly reason: AdmissionFailureReason;
      readonly detail?: string;
    };

export interface MachineWireFrame {
  readonly type: number;
  readonly payload: Buffer;
}

export class MachineProtocolError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "MachineProtocolError";
  }
}

function encodeFrame(type: number, payload: Buffer): Buffer {
  if (payload.length > MAX_MACHINE_FRAME_LENGTH) {
    throw new MachineProtocolError(
      `Machine frame length ${payload.length} exceeds maximum ${MAX_MACHINE_FRAME_LENGTH}`,
    );
  }
  const header = Buffer.alloc(HEADER_SIZE);
  header.writeUInt8(type, 0);
  header.writeUInt32BE(payload.length, 1);
  return Buffer.concat([header, payload]);
}

export class MachineFrameReader {
  private buffer = Buffer.alloc(0);

  feed(chunk: Buffer): MachineWireFrame[] {
    this.buffer = Buffer.concat([this.buffer, chunk]);
    const frames: MachineWireFrame[] = [];
    while (this.buffer.length >= HEADER_SIZE) {
      const type = this.buffer.readUInt8(0);
      const length = this.buffer.readUInt32BE(1);
      if (length > MAX_MACHINE_FRAME_LENGTH) {
        this.buffer = Buffer.alloc(0);
        throw new MachineProtocolError(`Machine frame length ${length} exceeds maximum ${MAX_MACHINE_FRAME_LENGTH}`);
      }
      if (this.buffer.length < HEADER_SIZE + length) break;
      frames.push({
        type,
        payload: Buffer.from(this.buffer.subarray(HEADER_SIZE, HEADER_SIZE + length)),
      });
      this.buffer = this.buffer.subarray(HEADER_SIZE + length);
    }
    return frames;
  }
}

function structured(value: unknown): Buffer {
  const payload = Buffer.from(JSON.stringify(value), "utf8");
  if (payload.length > MAX_STRUCTURED_PAYLOAD_LENGTH) {
    throw new MachineProtocolError("Structured machine payload exceeds 64 KiB");
  }
  return payload;
}

function parseStructured(payload: Buffer): unknown {
  if (payload.length > MAX_STRUCTURED_PAYLOAD_LENGTH) {
    throw new MachineProtocolError("Structured machine payload exceeds 64 KiB");
  }
  try {
    return JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(payload));
  } catch {
    throw new MachineProtocolError("Invalid machine protocol JSON");
  }
}

function exactKeys(object: Record<string, unknown>, allowed: readonly string[], name: string): void {
  const allowedSet = new Set(allowed);
  const unknown = Object.keys(object).find((key) => !allowedSet.has(key));
  if (unknown !== undefined) throw new MachineProtocolError(`${name} has unknown field ${unknown}`);
}

function record(value: unknown, name: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new MachineProtocolError(`${name} must be an object`);
  }
  return value as Record<string, unknown>;
}

function string(value: unknown, name: string, allowEmpty = false): string {
  if (typeof value !== "string" || (!allowEmpty && value.length === 0) || value.length > MAX_STRING_LENGTH) {
    throw new MachineProtocolError(`${name} must be a bounded string`);
  }
  return value;
}

function integer(value: unknown, name: string, minimum: number, maximum: number): number {
  if (!Number.isSafeInteger(value) || (value as number) < minimum || (value as number) > maximum) {
    throw new MachineProtocolError(`${name} must be an integer from ${minimum} through ${maximum}`);
  }
  return value as number;
}

function boolean(value: unknown, name: string): boolean {
  if (typeof value !== "boolean") throw new MachineProtocolError(`${name} must be boolean`);
  return value;
}

const CAPABILITIES = new Set<MachineCapability>(["framed-utf8-input", "typed-outcome", "input-mode-snapshot"]);

function capabilities(value: unknown): MachineCapability[] {
  if (!Array.isArray(value) || value.length > MAX_CAPABILITIES) {
    throw new MachineProtocolError("capabilities must be a bounded array");
  }
  const decoded = value.map((item) => {
    if (typeof item !== "string" || !CAPABILITIES.has(item as MachineCapability)) {
      throw new MachineProtocolError("unknown machine capability");
    }
    return item as MachineCapability;
  });
  if (new Set(decoded).size !== decoded.length) throw new MachineProtocolError("duplicate machine capability");
  return decoded;
}

function size(value: unknown): { rows: number; cols: number } {
  const object = record(value, "size");
  return {
    rows: integer(object.rows, "rows", 1, 0xffff),
    cols: integer(object.cols, "cols", 1, 0xffff),
  };
}

function inputModes(value: unknown): MachineInputModeSnapshotV1 {
  const object = record(value, "inputModes");
  exactKeys(
    object,
    [
      "revision",
      "applicationCursorKeys",
      "applicationKeypad",
      "bracketedPaste",
      "focusReporting",
      "mouseTracking",
      "mouseEncoding",
      "kittyKeyboardFlags",
    ],
    "inputModes",
  );
  const tracking = string(object.mouseTracking, "mouseTracking") as MouseTrackingMode;
  const encoding = string(object.mouseEncoding, "mouseEncoding") as MouseEncodingMode;
  if (!["none", "click", "button", "any"].includes(tracking))
    throw new MachineProtocolError("unknown mouse tracking mode");
  if (!["x10", "utf8", "sgr", "urxvt"].includes(encoding))
    throw new MachineProtocolError("unknown mouse encoding mode");
  if (!Array.isArray(object.kittyKeyboardFlags) || object.kittyKeyboardFlags.length > MAX_KITTY_STACK_DEPTH) {
    throw new MachineProtocolError("kittyKeyboardFlags must be a bounded array");
  }
  return {
    revision: integer(object.revision, "input mode revision", 0, Number.MAX_SAFE_INTEGER),
    applicationCursorKeys: boolean(object.applicationCursorKeys, "applicationCursorKeys"),
    applicationKeypad: boolean(object.applicationKeypad, "applicationKeypad"),
    bracketedPaste: boolean(object.bracketedPaste, "bracketedPaste"),
    focusReporting: boolean(object.focusReporting, "focusReporting"),
    mouseTracking: tracking,
    mouseEncoding: encoding,
    kittyKeyboardFlags: object.kittyKeyboardFlags.map((flag) => integer(flag, "kitty keyboard flag", 0, 0xffffffff)),
  };
}

function buildInfo(value: unknown): MachineBuildInfo {
  const object = record(value, "build");
  exactKeys(object, ["version", "revision", "dirty"], "build");
  return {
    version: string(object.version, "build.version"),
    ...(object.revision === undefined ? {} : { revision: string(object.revision, "build.revision") }),
    dirty: boolean(object.dirty, "build.dirty"),
  };
}

function openV2(value: unknown): MachineOpenV2 {
  const object = record(value, "OPEN");
  exactKeys(
    object,
    ["_tag", "protocol", "sessionId", "expectedGeneration", "rows", "cols", "requiredCapabilities"],
    "OPEN",
  );
  if (object._tag !== "Open" || object.protocol !== MACHINE_PROTOCOL_VERSION) {
    throw new MachineProtocolError("OPEN must request machine protocol 2");
  }
  return {
    _tag: "Open",
    protocol: MACHINE_PROTOCOL_VERSION,
    sessionId: string(object.sessionId, "sessionId"),
    expectedGeneration: string(object.expectedGeneration, "expectedGeneration"),
    ...size(object),
    requiredCapabilities: capabilities(object.requiredCapabilities),
  };
}

function admissionReason(value: unknown): AdmissionFailureReason {
  const reason = string(value, "admission failure reason") as AdmissionFailureReason;
  if (
    ![
      "unsupported-daemon",
      "generation-mismatch",
      "not-found",
      "permission-denied",
      "unsupported-capability",
      "transport-failure",
      "malformed-request",
    ].includes(reason)
  ) {
    throw new MachineProtocolError("unknown admission failure reason");
  }
  return reason;
}

function optionalDetail(value: unknown, name: string): string | undefined {
  return value === undefined ? undefined : string(value, name, true);
}

function snapshotPayload(metadata: Omit<MachineReadyV2, "_tag" | "screen">, screen: Buffer): Buffer {
  const encodedMetadata = structured({
    rows: metadata.rows,
    cols: metadata.cols,
    inputModes: metadata.inputModes,
  });
  const header = Buffer.alloc(4);
  header.writeUInt32BE(encodedMetadata.length, 0);
  return Buffer.concat([header, encodedMetadata, screen]);
}

function decodeSnapshotPayload(payload: Buffer): Omit<MachineReadyV2, "_tag"> {
  if (payload.length < 4) throw new MachineProtocolError("Snapshot payload is truncated");
  const metadataLength = payload.readUInt32BE(0);
  if (metadataLength > MAX_STRUCTURED_PAYLOAD_LENGTH || payload.length < 4 + metadataLength) {
    throw new MachineProtocolError("Snapshot metadata is truncated or oversized");
  }
  const metadata = record(parseStructured(payload.subarray(4, 4 + metadataLength)), "snapshot metadata");
  exactKeys(metadata, ["rows", "cols", "inputModes"], "snapshot metadata");
  return {
    ...size(metadata),
    inputModes: inputModes(metadata.inputModes),
    screen: Buffer.from(payload.subarray(4 + metadataLength)),
  };
}

export function encodeMachineRequest(request: MachineRequest): Buffer {
  switch (request._tag) {
    case "Open":
      return encodeFrame(MachineRequestType.OPEN, structured(openV2(request)));
    case "Input":
      return encodeFrame(MachineRequestType.INPUT, request.bytes);
    case "Resize":
      return encodeFrame(MachineRequestType.RESIZE, structured(size(request)));
    case "Detach":
      return encodeFrame(MachineRequestType.DETACH, Buffer.alloc(0));
  }
}

export function decodeMachineRequest(frame: MachineWireFrame): MachineRequest {
  switch (frame.type) {
    case MachineRequestType.OPEN:
      return openV2(parseStructured(frame.payload));
    case MachineRequestType.INPUT:
      return { _tag: "Input", bytes: Buffer.from(frame.payload) };
    case MachineRequestType.RESIZE: {
      const value = record(parseStructured(frame.payload), "RESIZE");
      exactKeys(value, ["rows", "cols"], "RESIZE");
      return { _tag: "Resize", ...size(value) };
    }
    case MachineRequestType.DETACH:
      if (frame.payload.length !== 0) throw new MachineProtocolError("DETACH payload must be empty");
      return { _tag: "Detach" };
    default:
      throw new MachineProtocolError(`Unknown machine request type ${frame.type}`);
  }
}

export function encodeMachineResponse(response: MachineResponse): Buffer {
  switch (response._tag) {
    case "Hello":
      return encodeFrame(MachineResponseType.HELLO, structured(response));
    case "Ready":
      return encodeFrame(MachineResponseType.READY, snapshotPayload(response, response.screen));
    case "Data":
      return encodeFrame(MachineResponseType.DATA, response.bytes);
    case "Geometry":
      return encodeFrame(MachineResponseType.GEOMETRY, structured(size(response)));
    case "Snapshot":
      return encodeFrame(MachineResponseType.SNAPSHOT, snapshotPayload(response, response.screen));
    case "InputModes":
      return encodeFrame(MachineResponseType.INPUT_MODES, structured({ inputModes: response.inputModes }));
    case "Exited":
      return encodeFrame(MachineResponseType.EXITED, structured(response));
    case "Detached":
      return encodeFrame(MachineResponseType.DETACHED, Buffer.alloc(0));
    case "AdmissionFailure":
      return encodeFrame(MachineResponseType.ADMISSION_FAILURE, structured(response));
    case "StreamFailure":
      return encodeFrame(MachineResponseType.STREAM_FAILURE, structured(response));
  }
}

export function decodeMachineResponse(frame: MachineWireFrame): MachineResponse {
  const structuredPayload = () => record(parseStructured(frame.payload), "machine response");
  switch (frame.type) {
    case MachineResponseType.HELLO: {
      const value = structuredPayload();
      exactKeys(value, ["_tag", "protocol", "generation", "capabilities", "build"], "HELLO");
      if (value._tag !== "Hello" || value.protocol !== MACHINE_PROTOCOL_VERSION)
        throw new MachineProtocolError("Invalid HELLO");
      return {
        _tag: "Hello",
        protocol: MACHINE_PROTOCOL_VERSION,
        generation: string(value.generation, "generation"),
        capabilities: capabilities(value.capabilities),
        build: buildInfo(value.build),
      };
    }
    case MachineResponseType.READY:
      return { _tag: "Ready", ...decodeSnapshotPayload(frame.payload) };
    case MachineResponseType.DATA:
      return { _tag: "Data", bytes: Buffer.from(frame.payload) };
    case MachineResponseType.GEOMETRY: {
      const value = record(parseStructured(frame.payload), "GEOMETRY");
      exactKeys(value, ["rows", "cols"], "GEOMETRY");
      return { _tag: "Geometry", ...size(value) };
    }
    case MachineResponseType.SNAPSHOT:
      return { _tag: "Snapshot", ...decodeSnapshotPayload(frame.payload) };
    case MachineResponseType.INPUT_MODES: {
      const value = structuredPayload();
      exactKeys(value, ["inputModes"], "INPUT_MODES");
      return { _tag: "InputModes", inputModes: inputModes(value.inputModes) };
    }
    case MachineResponseType.EXITED: {
      const value = structuredPayload();
      exactKeys(value, ["_tag", "code", "signal"], "EXITED");
      if (value._tag !== "Exited") throw new MachineProtocolError("Invalid EXITED");
      return {
        _tag: "Exited",
        code: integer(value.code, "exit code", -0x80000000, 0x7fffffff),
        signal: value.signal === null ? null : string(value.signal, "exit signal"),
      };
    }
    case MachineResponseType.DETACHED:
      if (frame.payload.length !== 0) throw new MachineProtocolError("DETACHED payload must be empty");
      return { _tag: "Detached" };
    case MachineResponseType.ADMISSION_FAILURE: {
      const value = structuredPayload();
      exactKeys(value, ["_tag", "reason", "detail"], "ADMISSION_FAILURE");
      if (value._tag !== "AdmissionFailure") throw new MachineProtocolError("Invalid ADMISSION_FAILURE");
      const detail = optionalDetail(value.detail, "admission detail");
      return {
        _tag: "AdmissionFailure",
        reason: admissionReason(value.reason),
        ...(detail === undefined ? {} : { detail }),
      };
    }
    case MachineResponseType.STREAM_FAILURE: {
      const value = structuredPayload();
      exactKeys(value, ["_tag", "phase", "reason", "diagnostic"], "STREAM_FAILURE");
      if (value._tag !== "StreamFailure") throw new MachineProtocolError("Invalid STREAM_FAILURE");
      const phase = string(value.phase, "stream failure phase") as StreamFailurePhase;
      if (!["baseline", "stream", "shutdown"].includes(phase))
        throw new MachineProtocolError("Unknown stream failure phase");
      const diagnostic = optionalDetail(value.diagnostic, "stream diagnostic");
      return {
        _tag: "StreamFailure",
        phase,
        reason: string(value.reason, "stream failure reason"),
        ...(diagnostic === undefined ? {} : { diagnostic }),
      };
    }
    default:
      throw new MachineProtocolError(`Unknown machine response type ${frame.type}`);
  }
}

export function encodeDaemonOpenV2(open: MachineOpenV2): Buffer {
  return encodeFrame(DaemonExtensionType.OPEN_V2, structured(openV2(open)));
}

export function decodeDaemonOpenV2(frame: MachineWireFrame): MachineOpenV2 {
  if (frame.type !== DaemonExtensionType.OPEN_V2) throw new MachineProtocolError("Expected daemon OPEN_V2");
  return openV2(parseStructured(frame.payload));
}

export function encodeDaemonAdmissionV2(admission: DaemonAdmissionV2): Buffer {
  return encodeFrame(DaemonExtensionType.ADMISSION_V2, structured(admission));
}

export function decodeDaemonAdmissionV2(frame: MachineWireFrame): DaemonAdmissionV2 {
  if (frame.type !== DaemonExtensionType.ADMISSION_V2) throw new MachineProtocolError("Expected daemon ADMISSION_V2");
  const value = record(parseStructured(frame.payload), "daemon admission");
  if (value._tag === "Accepted") {
    exactKeys(value, ["_tag", "protocol", "generation", "capabilities", "build"], "accepted admission");
    if (value.protocol !== MACHINE_PROTOCOL_VERSION)
      throw new MachineProtocolError("Daemon accepted an unsupported protocol");
    return {
      _tag: "Accepted",
      protocol: MACHINE_PROTOCOL_VERSION,
      generation: string(value.generation, "generation"),
      capabilities: capabilities(value.capabilities),
      build: buildInfo(value.build),
    };
  }
  if (value._tag === "Rejected") {
    exactKeys(value, ["_tag", "reason", "detail"], "rejected admission");
    const detail = optionalDetail(value.detail, "admission detail");
    return { _tag: "Rejected", reason: admissionReason(value.reason), ...(detail === undefined ? {} : { detail }) };
  }
  throw new MachineProtocolError("Invalid daemon admission response");
}

export type MachineAttachState =
  | { readonly _tag: "AwaitAdmission" }
  | { readonly _tag: "AwaitReady"; readonly hello: MachineHelloV2 }
  | {
      readonly _tag: "Streaming";
      readonly hello: MachineHelloV2;
      readonly ready: MachineReadyV2;
      readonly inputModeRevision: number;
    }
  | { readonly _tag: "AwaitEof"; readonly outcome: MachineOutcome }
  | { readonly _tag: "Complete"; readonly outcome: MachineOutcome }
  | { readonly _tag: "ProtocolViolation"; readonly reason: string };

export type MachineAttachEvent = { readonly _tag: "Frame"; readonly frame: MachineResponse } | { readonly _tag: "Eof" };

function violation(reason: string): MachineAttachState {
  return { _tag: "ProtocolViolation", reason };
}

/** Validates the complete adapter-to-host lifecycle, including terminal EOF. */
export function reduceMachineAttach(state: MachineAttachState, event: MachineAttachEvent): MachineAttachState {
  if (state._tag === "ProtocolViolation" || state._tag === "Complete") {
    return violation(`Unexpected ${event._tag} after ${state._tag}`);
  }
  if (event._tag === "Eof") {
    return state._tag === "AwaitEof"
      ? { _tag: "Complete", outcome: state.outcome }
      : violation(`Truncated machine stream while ${state._tag}`);
  }
  const frame = event.frame;
  switch (state._tag) {
    case "AwaitAdmission":
      if (frame._tag === "Hello") return { _tag: "AwaitReady", hello: frame };
      if (frame._tag === "AdmissionFailure") return { _tag: "AwaitEof", outcome: frame };
      return violation(`Expected HELLO or ADMISSION_FAILURE, received ${frame._tag}`);
    case "AwaitReady":
      if (frame._tag === "Ready") {
        return { _tag: "Streaming", hello: state.hello, ready: frame, inputModeRevision: frame.inputModes.revision };
      }
      if (frame._tag === "Exited" || frame._tag === "Detached" || frame._tag === "StreamFailure") {
        return { _tag: "AwaitEof", outcome: frame };
      }
      return violation(`Expected READY or pre-ready outcome, received ${frame._tag}`);
    case "Streaming":
      if (frame._tag === "Data" || frame._tag === "Geometry") return state;
      if (frame._tag === "InputModes") {
        return frame.inputModes.revision > state.inputModeRevision
          ? { ...state, inputModeRevision: frame.inputModes.revision }
          : violation("INPUT_MODES revision must advance");
      }
      if (frame._tag === "Snapshot") {
        return frame.inputModes.revision >= state.inputModeRevision
          ? { ...state, inputModeRevision: frame.inputModes.revision }
          : violation("SNAPSHOT input-mode revision moved backward");
      }
      if (frame._tag === "Exited" || frame._tag === "Detached" || frame._tag === "StreamFailure") {
        return { _tag: "AwaitEof", outcome: frame };
      }
      return violation(`Unexpected ${frame._tag} after READY`);
    case "AwaitEof":
      return violation(`Expected EOF after ${state.outcome._tag}, received ${frame._tag}`);
  }
}
