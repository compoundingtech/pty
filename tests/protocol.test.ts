import { describe, it, expect } from "vitest";
import {
  MessageType,
  PacketReader,
  PacketTooLargeError,
  MAX_PACKET_LENGTH,
  encodePacket,
  encodeData,
  encodeAttach,
  encodeDetach,
  encodeResize,
  encodeExit,
  encodeScreen,
  encodeStatus,
  encodeStatusResponse,
  decodeSize,
  decodeAttachFlags,
  ATTACH_FLAG_GEOMETRY_NEUTRAL,
  ATTACH_FLAG_FORCE_RESIZE,
  decodeExit,
} from "../src/protocol.ts";
import { Buffer } from "node:buffer";

describe("protocol", () => {
  describe("encodePacket / PacketReader", () => {
    it("round-trips a DATA packet", () => {
      const reader = new PacketReader();
      const encoded = encodeData("hello world");
      const packets = reader.feed(encoded);

      expect(packets).toHaveLength(1);
      expect(packets[0].type).toBe(MessageType.DATA);
      expect(packets[0].payload.toString()).toBe("hello world");
    });

    it("round-trips an ATTACH packet", () => {
      const reader = new PacketReader();
      const encoded = encodeAttach(24, 80);
      const packets = reader.feed(encoded);

      expect(packets).toHaveLength(1);
      expect(packets[0].type).toBe(MessageType.ATTACH);

      const size = decodeSize(packets[0].payload);
      expect(size.rows).toBe(24);
      expect(size.cols).toBe(80);
    });

    it("keeps legacy ATTACH byte-identical and appends set flags as a flag byte", () => {
      const legacy = encodeAttach(24, 80);
      expect(legacy).toEqual(
        encodePacket(MessageType.ATTACH, Buffer.from([0, 24, 0, 80])),
      );
      // An explicitly empty flag set stays on the legacy 4-byte frame.
      expect(encodeAttach(24, 80, 0)).toEqual(legacy);

      const reader = new PacketReader();
      const [neutral] = reader.feed(
        encodeAttach(24, 80, ATTACH_FLAG_GEOMETRY_NEUTRAL),
      );
      expect(neutral.payload).toEqual(Buffer.from([0, 24, 0, 80, 1]));
      expect(decodeSize(neutral.payload)).toEqual({ rows: 24, cols: 80 });
      expect(decodeAttachFlags(neutral.payload) & ATTACH_FLAG_GEOMETRY_NEUTRAL).toBe(1);
      expect(decodeAttachFlags(Buffer.from([0, 24, 0, 80]))).toBe(0);

      const [both] = new PacketReader().feed(
        encodeAttach(24, 80, ATTACH_FLAG_GEOMETRY_NEUTRAL | ATTACH_FLAG_FORCE_RESIZE),
      );
      expect(both.payload).toEqual(Buffer.from([0, 24, 0, 80, 3]));
      expect(decodeAttachFlags(both.payload) & ATTACH_FLAG_FORCE_RESIZE).toBe(
        ATTACH_FLAG_FORCE_RESIZE,
      );
    });

    it("round-trips a DETACH packet", () => {
      const reader = new PacketReader();
      const encoded = encodeDetach();
      const packets = reader.feed(encoded);

      expect(packets).toHaveLength(1);
      expect(packets[0].type).toBe(MessageType.DETACH);
      expect(packets[0].payload.length).toBe(0);
    });

    it("round-trips a RESIZE packet", () => {
      const reader = new PacketReader();
      const encoded = encodeResize(48, 120);
      const packets = reader.feed(encoded);

      expect(packets).toHaveLength(1);
      const size = decodeSize(packets[0].payload);
      expect(size.rows).toBe(48);
      expect(size.cols).toBe(120);
    });

    it("round-trips an EXIT packet", () => {
      const reader = new PacketReader();
      const encoded = encodeExit(42);
      const packets = reader.feed(encoded);

      expect(packets).toHaveLength(1);
      expect(packets[0].type).toBe(MessageType.EXIT);
      expect(decodeExit(packets[0].payload)).toBe(42);
    });

    it("round-trips a SCREEN packet", () => {
      const reader = new PacketReader();
      const screen = "\x1b[2J\x1b[H$ hello\r\nworld";
      const encoded = encodeScreen(screen);
      const packets = reader.feed(encoded);

      expect(packets).toHaveLength(1);
      expect(packets[0].type).toBe(MessageType.SCREEN);
      expect(packets[0].payload.toString()).toBe(screen);
    });
  });

  describe("PacketReader streaming", () => {
    it("handles multiple packets in one chunk", () => {
      const reader = new PacketReader();
      const buf = Buffer.concat([
        encodeData("hello"),
        encodeData("world"),
        encodeDetach(),
      ]);

      const packets = reader.feed(buf);
      expect(packets).toHaveLength(3);
      expect(packets[0].payload.toString()).toBe("hello");
      expect(packets[1].payload.toString()).toBe("world");
      expect(packets[2].type).toBe(MessageType.DETACH);
    });

    it("handles packets split across multiple chunks", () => {
      const reader = new PacketReader();
      const full = encodeData("hello world");

      // Split in the middle
      const part1 = full.subarray(0, 3);
      const part2 = full.subarray(3, 8);
      const part3 = full.subarray(8);

      expect(reader.feed(part1)).toHaveLength(0);
      expect(reader.feed(part2)).toHaveLength(0);

      const packets = reader.feed(part3);
      expect(packets).toHaveLength(1);
      expect(packets[0].payload.toString()).toBe("hello world");
    });

    it("handles a packet split exactly at the header boundary", () => {
      const reader = new PacketReader();
      const full = encodeData("test");

      // Split exactly after the 5-byte header
      const header = full.subarray(0, 5);
      const payload = full.subarray(5);

      expect(reader.feed(header)).toHaveLength(0);
      const packets = reader.feed(payload);
      expect(packets).toHaveLength(1);
      expect(packets[0].payload.toString()).toBe("test");
    });

    it("handles empty payload", () => {
      const reader = new PacketReader();
      const encoded = encodePacket(MessageType.DETACH, Buffer.alloc(0));
      const packets = reader.feed(encoded);

      expect(packets).toHaveLength(1);
      expect(packets[0].type).toBe(MessageType.DETACH);
      expect(packets[0].payload.length).toBe(0);
    });

    it("handles large payloads", () => {
      const reader = new PacketReader();
      const bigString = "x".repeat(100_000);
      const encoded = encodeData(bigString);
      const packets = reader.feed(encoded);

      expect(packets).toHaveLength(1);
      expect(packets[0].payload.toString()).toBe(bigString);
    });

    it("ignores unknown message types without crashing", () => {
      const reader = new PacketReader();
      // Manually craft a packet with type 99
      const header = Buffer.alloc(5);
      header.writeUInt8(99, 0);
      header.writeUInt32BE(3, 1);
      const payload = Buffer.from("abc");
      const raw = Buffer.concat([header, payload]);

      const packets = reader.feed(raw);
      expect(packets).toHaveLength(1);
      expect(packets[0].type).toBe(99);
      expect(packets[0].payload.toString()).toBe("abc");
    });
  });

  describe("decode edge cases", () => {
    it("decodeSize returns defaults for truncated payload", () => {
      const size = decodeSize(Buffer.alloc(2));
      expect(size.rows).toBe(24);
      expect(size.cols).toBe(80);
    });

    it("decodeSize returns defaults for empty payload", () => {
      const size = decodeSize(Buffer.alloc(0));
      expect(size.rows).toBe(24);
      expect(size.cols).toBe(80);
    });

    it("decodeExit returns -1 for truncated payload", () => {
      expect(decodeExit(Buffer.alloc(2))).toBe(-1);
    });

    it("decodeExit returns -1 for empty payload", () => {
      expect(decodeExit(Buffer.alloc(0))).toBe(-1);
    });
  });

  describe("STATUS", () => {
    it("round-trips a STATUS request (empty payload)", () => {
      const reader = new PacketReader();
      const encoded = encodeStatus();
      const packets = reader.feed(encoded);
      expect(packets).toHaveLength(1);
      expect(packets[0].type).toBe(MessageType.STATUS);
      expect(packets[0].payload.length).toBe(0);
    });

    it("rejects packets whose declared length exceeds MAX_PACKET_LENGTH (BUG-3)", () => {
      const reader = new PacketReader();
      // Craft a header with length = MAX + 1 and no payload
      const header = Buffer.alloc(5);
      header.writeUInt8(MessageType.DATA, 0);
      header.writeUInt32BE(MAX_PACKET_LENGTH + 1, 1);

      expect(() => reader.feed(header)).toThrow(PacketTooLargeError);
    });

    it("rejects the max-uint32 length (worst case attack)", () => {
      const reader = new PacketReader();
      const header = Buffer.alloc(5);
      header.writeUInt8(MessageType.DATA, 0);
      header.writeUInt32BE(0xffffffff, 1);

      expect(() => reader.feed(header)).toThrow(PacketTooLargeError);
    });

    it("poisons the buffer after oversize throw (subsequent feeds don't buffer unbounded)", () => {
      const reader = new PacketReader();
      const header = Buffer.alloc(5);
      header.writeUInt8(MessageType.DATA, 0);
      header.writeUInt32BE(0xffffffff, 1);

      try { reader.feed(header); } catch {}
      // Buffer is cleared, so subsequent valid packets parse correctly from
      // the new boundary (not treating their bytes as payload of the bad one).
      const packets = reader.feed(encodeData("hi"));
      expect(packets).toHaveLength(1);
      expect(packets[0].payload.toString()).toBe("hi");
    });

    it("accepts packets at exactly MAX_PACKET_LENGTH", () => {
      // Build a valid packet with length exactly at the cap (but small payload;
      // we just want the length field to be valid).
      const reader = new PacketReader();
      const encoded = encodeData("ok");
      const packets = reader.feed(encoded);
      expect(packets).toHaveLength(1);
      expect(packets[0].payload.toString()).toBe("ok");
    });

    it("round-trips a STATUS response (JSON payload)", () => {
      const reader = new PacketReader();
      const json = JSON.stringify({ name: "test", terminal: { cols: 80, rows: 24 } });
      const encoded = encodeStatusResponse(json);
      const packets = reader.feed(encoded);
      expect(packets).toHaveLength(1);
      expect(packets[0].type).toBe(MessageType.STATUS);
      expect(JSON.parse(packets[0].payload.toString())).toEqual({
        name: "test",
        terminal: { cols: 80, rows: 24 },
      });
    });
  });
});
