import * as fs from "node:fs";
import * as path from "node:path";
import { describe, expect, it } from "vitest";
import {
  ActivityLease,
  parseActivityCommand,
  type ActivityFixtureState,
} from "../src/activity.ts";

describe("activity fixtures", () => {
  it("make explicit activity the only configured-adapter eligibility fact", () => {
    const fixturePath = path.join(
      import.meta.dirname,
      "fixtures",
      "activity",
      "cases.json",
    );
    const fixture = JSON.parse(fs.readFileSync(fixturePath, "utf8")) as {
      cases: Array<{
        name: string;
        screen: string;
        activity: ActivityFixtureState;
        alternateScreen: boolean;
        inputMode: string;
        eligible: boolean;
      }>;
    };

    expect(fixture.cases.map((entry) => entry.name)).toEqual([
      "active-turn",
      "idle-prompt",
      "long-child-command",
      "alternate-screen",
      "terminal-restored",
      "compaction",
      "clear",
      "adapter-crash",
      "stale-log-idle",
      "daemon-restart",
    ]);
    for (const entry of fixture.cases) {
      expect(entry.eligible, entry.name).toBe(entry.activity === "idle");
    }
    expect(fixture.cases.find((entry) => entry.name === "idle-prompt"))
      .toMatchObject({ alternateScreen: true, inputMode: "raw", eligible: true });
    expect(fixture.cases.find((entry) => entry.name === "stale-log-idle"))
      .toMatchObject({ screen: "> ", activity: "unknown", eligible: false });
  });
});

describe("ActivityLease", () => {
  it("starts unknown and generation-bound", () => {
    const lease = new ActivityLease("generation-a");
    expect(lease.snapshot()).toEqual({
      state: "unknown",
      generation: "generation-a",
      producerEpoch: null,
      sequence: 0,
    });
  });

  it("orders active, child-command, and idle transitions from one live owner", () => {
    const lease = new ActivityLease("generation-a");
    const owner = {};

    expect(lease.apply(owner, {
      op: "claim",
      producerEpoch: "epoch-a",
      source: "codex",
    })).toMatchObject({ ok: true, activity: { state: "unknown", sequence: 0 } });
    expect(lease.apply(owner, {
      op: "set",
      producerEpoch: "epoch-a",
      sequence: 1,
      state: "active",
      turnId: "turn-1",
    })).toMatchObject({ ok: true, activity: { state: "active", sequence: 1 } });
    expect(lease.apply(owner, {
      op: "set",
      producerEpoch: "epoch-a",
      sequence: 2,
      state: "child_command",
      turnId: "turn-1",
    })).toMatchObject({
      ok: true,
      activity: { state: "child_command", sequence: 2 },
    });
    expect(lease.apply(owner, {
      op: "set",
      producerEpoch: "epoch-a",
      sequence: 3,
      state: "idle",
      turnId: "turn-1",
    })).toMatchObject({ ok: true, activity: { state: "idle", sequence: 3 } });
  });

  it("rejects a competing owner and stale, skipped, or wrong-epoch updates", () => {
    const lease = new ActivityLease("generation-a");
    const owner = {};
    const other = {};
    expect(lease.apply(owner, {
      op: "claim",
      producerEpoch: "epoch-a",
    }).ok).toBe(true);
    expect(lease.apply(other, {
      op: "claim",
      producerEpoch: "epoch-b",
    })).toMatchObject({ ok: false, error: "activity lease already held" });
    expect(lease.apply(owner, {
      op: "set",
      producerEpoch: "epoch-a",
      sequence: 1,
      state: "idle",
    }).ok).toBe(true);
    expect(lease.apply(owner, {
      op: "set",
      producerEpoch: "epoch-a",
      sequence: 3,
      state: "active",
    })).toMatchObject({
      ok: false,
      error: "activity sequence must be 2",
      activity: { state: "unknown", producerEpoch: null, sequence: 0 },
    });
    expect(lease.apply(owner, {
      op: "claim",
      producerEpoch: "epoch-c",
    }).ok).toBe(true);
    expect(lease.apply(owner, {
      op: "set",
      producerEpoch: "epoch-b",
      sequence: 1,
      state: "idle",
    })).toMatchObject({
      ok: false,
      error: "activity lease identity mismatch",
      activity: { state: "unknown", producerEpoch: null, sequence: 0 },
    });
  });

  it("resets to unknown on adapter crash and daemon restart", () => {
    const owner = {};
    const lease = new ActivityLease("generation-a");
    lease.apply(owner, { op: "claim", producerEpoch: "epoch-a" });
    lease.apply(owner, {
      op: "set",
      producerEpoch: "epoch-a",
      sequence: 1,
      state: "idle",
    });
    lease.release(owner);
    expect(lease.snapshot()).toEqual({
      state: "unknown",
      generation: "generation-a",
      producerEpoch: null,
      sequence: 0,
    });

    const restarted = new ActivityLease("generation-b");
    expect(restarted.snapshot()).toEqual({
      state: "unknown",
      generation: "generation-b",
      producerEpoch: null,
      sequence: 0,
    });
  });

  it("rejects malformed and oversized adapter commands", () => {
    expect(parseActivityCommand(Buffer.from("{bad"))).toBeNull();
    expect(parseActivityCommand(Buffer.from(JSON.stringify({
      op: "set",
      producerEpoch: "epoch-a",
      sequence: 1,
      state: "idle",
      turnId: "x".repeat(257),
    })))).toBeNull();
    expect(parseActivityCommand(Buffer.from(JSON.stringify({
      op: "set",
      producerEpoch: "epoch-a",
      sequence: 1,
      state: "screen-looked-idle",
    })))).toBeNull();
  });
});
