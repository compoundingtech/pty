import { describe, expect, it } from "vitest";
import {
  MAX_GUARDED_DATA_BYTES,
  parseGuardedSendCommand,
} from "../src/guarded-send.ts";

describe("parseGuardedSendCommand", () => {
  it("accepts an exact generation, revision, and bounded non-empty payload", () => {
    expect(parseGuardedSendCommand(Buffer.from(JSON.stringify({
      generation: "generation-a",
      ioRevision: 42,
      data: "submit\n",
    })))).toEqual({
      generation: "generation-a",
      ioRevision: 42,
      data: "submit\n",
    });
    const escapedAtLimit = "\0".repeat(MAX_GUARDED_DATA_BYTES);
    expect(parseGuardedSendCommand(Buffer.from(JSON.stringify({
      generation: "generation-a",
      ioRevision: 43,
      data: escapedAtLimit,
    })))?.data).toBe(escapedAtLimit);
  });

  it("rejects malformed, ambiguous, empty, and oversized guards", () => {
    expect(parseGuardedSendCommand(Buffer.from("{bad"))).toBeNull();
    expect(parseGuardedSendCommand(Buffer.from(JSON.stringify({
      generation: "generation-a",
      ioRevision: -1,
      data: "x",
    })))).toBeNull();
    expect(parseGuardedSendCommand(Buffer.from(JSON.stringify({
      generation: "generation-a",
      ioRevision: 0,
      data: "",
    })))).toBeNull();
    expect(parseGuardedSendCommand(Buffer.from(JSON.stringify({
      generation: "generation-a",
      ioRevision: 0,
      data: "x",
      semanticKey: "enter",
    })))).toBeNull();
    expect(parseGuardedSendCommand(Buffer.from(JSON.stringify({
      generation: "generation-a",
      ioRevision: 0,
      data: "x".repeat(MAX_GUARDED_DATA_BYTES + 1),
    })))).toBeNull();
  });
});
