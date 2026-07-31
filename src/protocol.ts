import { Buffer } from "node:buffer";

export const MessageType = {
  DATA: 0, // Terminal data (bidirectional)
  ATTACH: 1, // Client → Server: attaching with terminal size
  DETACH: 2, // Client → Server: detaching
  RESIZE: 3, // Client → Server: terminal resized
  EXIT: 4, // Server → Client: process exited
  SCREEN: 5, // Server → Client: screen buffer replay on attach
  PEEK: 6, // Client → Server: read-only attach (no input, no resize)
  STATUS: 7, // Client → Server: request stats; Server → Client: JSON stats response
  TERMINAL_REGION_REQUEST: 8, // Client → Server: read-only structured cell query
  TERMINAL_REGION_RESPONSE: 9, // Server → Client: structured cells at one revision
} as const;

export type MessageType = (typeof MessageType)[keyof typeof MessageType];

export interface Packet {
  type: MessageType;
  payload: Buffer;
}

export interface TerminalRegionRequest {
  /** Absolute row in the active buffer, where 0 is the oldest retained row. */
  row: number;
  /** Zero-based column in the active buffer. */
  col: number;
  rows: number;
  cols: number;
}

export type TerminalCellColor =
  | { _tag: "default" }
  | { _tag: "palette"; index: number }
  | { _tag: "rgb"; value: number };

export interface TerminalCell {
  chars: string;
  width: number;
  fg: TerminalCellColor;
  bg: TerminalCellColor;
  bold: boolean;
  italic: boolean;
  dim: boolean;
  underline: boolean;
  blink: boolean;
  inverse: boolean;
  invisible: boolean;
  strikethrough: boolean;
  overline: boolean;
}

export interface TerminalRegionLine {
  wrapped: boolean;
  cells: TerminalCell[];
}

export interface TerminalModes {
  applicationCursorKeys: boolean;
  applicationKeypad: boolean;
  bracketedPaste: boolean;
  insert: boolean;
  mouseTracking: "none" | "x10" | "vt200" | "drag" | "any";
  origin: boolean;
  reverseWraparound: boolean;
  sendFocus: boolean;
  synchronizedOutput: boolean;
  wraparound: boolean;
  sgrMouse: boolean;
  cursorHidden: boolean;
  kittyKeyboardFlags: number[];
}

export interface TerminalRegionResponse {
  /** Identifies the daemon lifetime in which `revision` is monotonic. */
  generation: string;
  /** Monotonic daemon-local revision of the terminal model. */
  revision: number;
  terminal: {
    rows: number;
    cols: number;
    buffer: "normal" | "alternate";
    bufferRows: number;
    viewportRow: number;
    cursor: { row: number; col: number };
    modes: TerminalModes;
  };
  region: {
    /** Actual, clamped origin and dimensions returned by the daemon. */
    row: number;
    col: number;
    rows: number;
    cols: number;
    lines: TerminalRegionLine[];
  };
}

// Packet wire format: [type: uint8][length: uint32BE][payload: N bytes]
const HEADER_SIZE = 5;

// BUG-3: cap legitimate packet size. SCREEN replays carry the serialized
// xterm buffer (rows × cols × attrs × scrollback). With the 10k-line default
// scrollback plus mode prefixes, 32 MiB is generously above any real payload
// while still small enough to bound a single malformed-length attack.
export const MAX_PACKET_LENGTH = 32 * 1024 * 1024;

/** Thrown when an inbound packet declares a length larger than
 *  `MAX_PACKET_LENGTH`. Socket handlers should destroy the connection. */
export class PacketTooLargeError extends Error {
  readonly declaredLength: number;
  constructor(declaredLength: number) {
    super(
      `Packet length ${declaredLength} exceeds maximum ${MAX_PACKET_LENGTH}`
    );
    this.name = "PacketTooLargeError";
    this.declaredLength = declaredLength;
  }
}

export function encodePacket(type: MessageType, payload: Buffer): Buffer {
  const header = Buffer.alloc(HEADER_SIZE);
  header.writeUInt8(type, 0);
  header.writeUInt32BE(payload.length, 1);
  return Buffer.concat([header, payload]);
}

export function encodeData(data: string): Buffer {
  return encodePacket(MessageType.DATA, Buffer.from(data));
}

export function encodeAttach(rows: number, cols: number): Buffer {
  const payload = Buffer.alloc(4);
  payload.writeUInt16BE(rows, 0);
  payload.writeUInt16BE(cols, 2);
  return encodePacket(MessageType.ATTACH, payload);
}

export function encodeDetach(): Buffer {
  return encodePacket(MessageType.DETACH, Buffer.alloc(0));
}

export function encodeResize(rows: number, cols: number): Buffer {
  const payload = Buffer.alloc(4);
  payload.writeUInt16BE(rows, 0);
  payload.writeUInt16BE(cols, 2);
  return encodePacket(MessageType.RESIZE, payload);
}

export function encodeExit(code: number): Buffer {
  const payload = Buffer.alloc(4);
  payload.writeInt32BE(code, 0);
  return encodePacket(MessageType.EXIT, payload);
}

export function encodePeek(plain = false, full = false): Buffer {
  const payload = Buffer.alloc(1);
  // Bit 0: plain, Bit 1: full scrollback
  payload.writeUInt8((plain ? 1 : 0) | (full ? 2 : 0), 0);
  return encodePacket(MessageType.PEEK, payload);
}

export function encodeScreen(data: string): Buffer {
  return encodePacket(MessageType.SCREEN, Buffer.from(data));
}

export function encodeStatus(): Buffer {
  return encodePacket(MessageType.STATUS, Buffer.alloc(0));
}

export function encodeStatusResponse(json: string): Buffer {
  return encodePacket(MessageType.STATUS, Buffer.from(json));
}

/** Maximum requested cell count. Region queries are intended for bounded
 * viewports and scans, not dumping the daemon's entire scrollback in one frame. */
export const MAX_TERMINAL_REGION_CELLS = 100_000;

function validateTerminalRegionRequest(value: unknown): TerminalRegionRequest {
  if (typeof value !== "object" || value === null) {
    throw new Error("Terminal region request must be an object");
  }
  const request = value as Record<string, unknown>;
  const row = request.row;
  const col = request.col;
  const rows = request.rows;
  const cols = request.cols;
  if (
    !Number.isInteger(row) || (row as number) < 0
    || !Number.isInteger(col) || (col as number) < 0
    || !Number.isInteger(rows) || (rows as number) <= 0
    || !Number.isInteger(cols) || (cols as number) <= 0
  ) {
    throw new Error("Terminal region coordinates must be non-negative and dimensions positive");
  }
  if ((rows as number) * (cols as number) > MAX_TERMINAL_REGION_CELLS) {
    throw new Error(
      `Terminal region exceeds ${MAX_TERMINAL_REGION_CELLS} cells`,
    );
  }
  return {
    row: row as number,
    col: col as number,
    rows: rows as number,
    cols: cols as number,
  };
}

export function encodeTerminalRegionRequest(request: TerminalRegionRequest): Buffer {
  return encodePacket(
    MessageType.TERMINAL_REGION_REQUEST,
    Buffer.from(JSON.stringify(validateTerminalRegionRequest(request))),
  );
}

export function decodeTerminalRegionRequest(payload: Buffer): TerminalRegionRequest {
  return validateTerminalRegionRequest(JSON.parse(payload.toString()));
}

export function encodeTerminalRegionResponse(response: TerminalRegionResponse): Buffer {
  const payload = Buffer.from(JSON.stringify(response));
  if (payload.length > MAX_PACKET_LENGTH) {
    throw new PacketTooLargeError(payload.length);
  }
  return encodePacket(
    MessageType.TERMINAL_REGION_RESPONSE,
    payload,
  );
}

export function decodeTerminalRegionResponse(payload: Buffer): TerminalRegionResponse {
  return JSON.parse(payload.toString()) as TerminalRegionResponse;
}

export function decodeSize(payload: Buffer): { rows: number; cols: number } {
  if (payload.length < 4) {
    return { rows: 24, cols: 80 };
  }
  return {
    rows: payload.readUInt16BE(0),
    cols: payload.readUInt16BE(2),
  };
}

export function decodeExit(payload: Buffer): number {
  if (payload.length < 4) {
    return -1;
  }
  return payload.readInt32BE(0);
}

/** Streaming packet parser that handles partial reads on a stream socket.
 *  Throws `PacketTooLargeError` if a peer declares a length exceeding
 *  `MAX_PACKET_LENGTH` — handlers should destroy the socket. */
export class PacketReader {
  private buffer = Buffer.alloc(0);

  feed(data: Buffer): Packet[] {
    this.buffer = Buffer.concat([this.buffer, data]);
    const packets: Packet[] = [];

    while (this.buffer.length >= HEADER_SIZE) {
      const type = this.buffer.readUInt8(0) as MessageType;
      const length = this.buffer.readUInt32BE(1);

      if (length > MAX_PACKET_LENGTH) {
        // Poison the buffer so subsequent feed() calls can't continue past
        // the bad header (even though the caller should drop the connection).
        this.buffer = Buffer.alloc(0);
        throw new PacketTooLargeError(length);
      }

      if (this.buffer.length < HEADER_SIZE + length) break;

      const payload = Buffer.from(
        this.buffer.subarray(HEADER_SIZE, HEADER_SIZE + length)
      );
      packets.push({ type, payload });
      this.buffer = this.buffer.subarray(HEADER_SIZE + length);
    }

    return packets;
  }
}
