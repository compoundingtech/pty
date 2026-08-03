import * as fs from "node:fs";
import * as path from "node:path";
import { describe, expect, it } from "vitest";
import type { ActivityFixtureState } from "../src/activity.ts";
import {
  MAX_GUARDED_DATA_BYTES,
  parseGuardedSendCommand,
} from "../src/guarded-send.ts";

describe("combined activity and guarded-send fixtures", () => {
  it("separates explicit idle authority from exact-token acceptance", () => {
    const fixturePath = path.join(
      import.meta.dirname,
      "fixtures",
      "guarded-send",
      "cases.json",
    );
    const fixture = JSON.parse(fs.readFileSync(fixturePath, "utf8")) as {
      cases: Array<{
        name: string;
        screen: string;
        activity: ActivityFixtureState;
        alternateScreen: boolean;
        observed: { generation: string; ioRevision: number };
        current: { generation: string; ioRevision: number };
        change: string;
        expected: {
          adapterEligible: boolean;
          guardAccepted: boolean;
          deliveryAllowed: boolean;
        };
      }>;
    };

    expect(fixture.cases.map((entry) => entry.name)).toEqual([
      "idle",
      "active",
      "alternate-screen-vim",
      "user-input",
      "child-output",
      "resize",
      "stale-token",
      "daemon-replacement",
    ]);
    for (const entry of fixture.cases) {
      const adapterEligible = entry.activity === "idle";
      const guardAccepted =
        entry.observed.generation === entry.current.generation &&
        entry.observed.ioRevision === entry.current.ioRevision;
      expect(entry.expected.adapterEligible, entry.name).toBe(adapterEligible);
      expect(entry.expected.guardAccepted, entry.name).toBe(guardAccepted);
      expect(entry.expected.deliveryAllowed, entry.name).toBe(
        adapterEligible && guardAccepted,
      );
    }
    expect(fixture.cases.find((entry) => entry.name === "alternate-screen-vim"))
      .toMatchObject({
        alternateScreen: true,
        activity: "unknown",
        expected: {
          guardAccepted: true,
          deliveryAllowed: false,
        },
      });
  });
});

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
