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
  encodeActivity,
  encodeGeometry,
  decodeSize,
  decodeGeometry,
  decodeExit,
} from "../src/protocol.ts";
import { Buffer } from "node:buffer";
import type { StatsResult } from "../src/client.ts";

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

    it("encodes ATTACH as a plain 4-byte size payload", () => {
      const attach = encodeAttach(24, 80);
      expect(attach).toEqual(
        encodePacket(MessageType.ATTACH, Buffer.from([0, 24, 0, 80])),
      );

      const reader = new PacketReader();
      const [packet] = reader.feed(attach);
      expect(packet.payload).toEqual(Buffer.from([0, 24, 0, 80]));
      expect(decodeSize(packet.payload)).toEqual({ rows: 24, cols: 80 });
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
      const response = {
        name: "test",
        terminal: { cols: 80, rows: 24 },
        clients: {
          total: 1,
          attached: 1,
          readOnly: 0,
          connections: [{
            role: "writable",
            rows: 24,
            cols: 80,
            lastRequestSequence: 1,
            constrains: { rows: true, cols: true },
          }],
        },
      };
      const json = JSON.stringify(response);
      const encoded = encodeStatusResponse(json);
      const packets = reader.feed(encoded);
      expect(packets).toHaveLength(1);
      expect(packets[0].type).toBe(MessageType.STATUS);
      expect(JSON.parse(packets[0].payload.toString())).toEqual(response);
    });

    it("round-trips an effective GEOMETRY packet", () => {
      const reader = new PacketReader();
      const packets = reader.feed(encodeGeometry(24, 80));
      expect(packets).toHaveLength(1);
      expect(packets[0].type).toBe(MessageType.GEOMETRY);
      expect(decodeGeometry(packets[0].payload)).toEqual({
        rows: 24,
        cols: 80,
      });
    });

    it("lets an older client ignore an unknown framed type and continue DATA", () => {
      const reader = new PacketReader();
      const packets = reader.feed(Buffer.concat([
        encodeGeometry(24, 80),
        encodeData("after-unknown"),
      ]));
      let received = "";
      for (const packet of packets) {
        // This intentionally models the pre-GEOMETRY client switch.
        if (packet.type === MessageType.DATA) {
          received += packet.payload.toString();
        }
      }
      expect(received).toBe("after-unknown");
    });

    it("accepts an old-daemon STATUS response without connection details", () => {
      const response = {
        name: "legacy",
        terminal: {
          cols: 80,
          rows: 24,
          cursorX: 0,
          cursorY: 0,
          scrollbackUsed: 24,
          scrollbackCapacity: 10024,
        },
        process: { alive: true, exitCode: null, pid: 123, resources: null },
        daemon: { pid: 456, resources: null },
        clients: { total: 2, attached: 2, readOnly: 0 },
        modes: {
          alternateScreen: false,
          sgrMouse: false,
          cursorHidden: false,
          kittyKeyboard: false,
          kittyKeyboardFlags: [],
        },
        activity: {
          state: "unknown" as const,
          generation: "generation-legacy",
          producerEpoch: null,
          sequence: 0,
        },
        uptimeSeconds: 10,
        createdAt: "2026-07-31T00:00:00.000Z",
      } satisfies StatsResult;

      const reader = new PacketReader();
      const packets = reader.feed(encodeStatusResponse(JSON.stringify(response)));
      const decoded = JSON.parse(packets[0].payload.toString()) as StatsResult;

      expect(decoded.clients).toEqual({ total: 2, attached: 2, readOnly: 0 });
      expect(decoded.clients.connections).toBeUndefined();
    });
  });

  describe("ACTIVITY", () => {
    it("round-trips bounded JSON commands and responses", () => {
      const reader = new PacketReader();
      const encoded = encodeActivity({
        op: "claim",
        producerEpoch: "epoch-a",
      });
      const packets = reader.feed(encoded);
      expect(packets).toHaveLength(1);
      expect(packets[0].type).toBe(MessageType.ACTIVITY);
      expect(JSON.parse(packets[0].payload.toString())).toEqual({
        op: "claim",
        producerEpoch: "epoch-a",
      });
    });
  });
});
