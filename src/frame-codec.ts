import { Buffer } from "node:buffer";

const HEADER_SIZE = 5;

export interface LengthPrefixedFrame<Type extends number> {
  readonly type: Type;
  readonly payload: Buffer;
}

export interface FrameLengthLimit {
  readonly maximum: number;
  readonly error: (declaredLength: number) => Error;
}

/** Encodes `[type: uint8][length: uint32BE][payload]` with one allocation. */
export function encodeLengthPrefixedFrame(
  type: number,
  payload: Buffer,
  limit?: FrameLengthLimit,
): Buffer {
  if (limit !== undefined && payload.length > limit.maximum) {
    throw limit.error(payload.length);
  }
  const frame = Buffer.allocUnsafe(HEADER_SIZE + payload.length);
  frame.writeUInt8(type, 0);
  frame.writeUInt32BE(payload.length, 1);
  payload.copy(frame, HEADER_SIZE);
  return frame;
}

/**
 * Bounded incremental reader for the shared PTY frame envelope.
 *
 * Each inbound byte is copied at most once into its final header or payload
 * allocation. A declared oversize length permanently poisons the reader so a
 * caller cannot accidentally resume at an untrusted stream boundary.
 */
export class BoundedFrameReader<Type extends number> {
  private readonly header = Buffer.allocUnsafe(HEADER_SIZE);
  private headerLength = 0;
  private type: Type | undefined;
  private payload: Buffer | undefined;
  private payloadLength = 0;
  private poisoned: Error | undefined;

  constructor(private readonly limit: FrameLengthLimit) {}

  feed(chunk: Buffer): Array<LengthPrefixedFrame<Type>> {
    if (this.poisoned !== undefined) throw this.poisoned;

    const frames: Array<LengthPrefixedFrame<Type>> = [];
    let offset = 0;

    while (offset < chunk.length) {
      if (this.headerLength < HEADER_SIZE) {
        const copied = Math.min(HEADER_SIZE - this.headerLength, chunk.length - offset);
        chunk.copy(this.header, this.headerLength, offset, offset + copied);
        this.headerLength += copied;
        offset += copied;

        if (this.headerLength < HEADER_SIZE) break;

        const declaredLength = this.header.readUInt32BE(1);
        if (declaredLength > this.limit.maximum) {
          this.poisoned = this.limit.error(declaredLength);
          throw this.poisoned;
        }
        this.type = this.header.readUInt8(0) as Type;
        this.payload = Buffer.allocUnsafe(declaredLength);
        this.payloadLength = 0;

        if (declaredLength === 0) {
          frames.push({ type: this.type, payload: this.payload });
          this.resetFrame();
        }
      }

      if (this.payload !== undefined) {
        const copied = Math.min(this.payload.length - this.payloadLength, chunk.length - offset);
        chunk.copy(this.payload, this.payloadLength, offset, offset + copied);
        this.payloadLength += copied;
        offset += copied;

        if (this.payloadLength === this.payload.length) {
          frames.push({ type: this.type as Type, payload: this.payload });
          this.resetFrame();
        }
      }
    }

    return frames;
  }

  private resetFrame(): void {
    this.headerLength = 0;
    this.type = undefined;
    this.payload = undefined;
    this.payloadLength = 0;
  }
}
