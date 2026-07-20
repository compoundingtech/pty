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
} as const;

export type MessageType = (typeof MessageType)[keyof typeof MessageType];

export interface Packet {
  type: MessageType;
  payload: Buffer;
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

/** Optional ATTACH flag: nudge the child into a full redraw even when this
 * client attaches at the size the session already has. */
export const ATTACH_FLAG_FORCE_RESIZE = 0x02;

export function encodeAttach(rows: number, cols: number, flags = 0): Buffer {
  // Keep the legacy frame byte-for-byte identical. The optional flag byte is
  // appended only when a flag is set; older servers already ignore bytes after
  // the first four size bytes.
  const payload = Buffer.alloc(flags === 0 ? 4 : 5);
  payload.writeUInt16BE(rows, 0);
  payload.writeUInt16BE(cols, 2);
  if (flags !== 0) payload.writeUInt8(flags, 4);
  return encodePacket(MessageType.ATTACH, payload);
}

export function decodeAttachFlags(payload: Buffer): number {
  return payload.length >= 5 ? payload.readUInt8(4) : 0;
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
