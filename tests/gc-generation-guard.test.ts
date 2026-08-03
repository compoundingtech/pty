import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import {
  cleanupObservedSession,
  commitObservedFlapping,
  respawnPermanent,
  type SessionInfo,
  type SessionMetadata,
} from "../src/sessions.ts";
import { terminateAndWait } from "./setup/processes.ts";

const roots: string[] = [];
const daemonPids: number[] = [];

const makeRoot = () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "pty-gc-generation-"));
  roots.push(root);
  process.env.PTY_ROOT = root;
  return root;
};

const metadata = (
  root: string,
  generation?: string,
  tags?: Record<string, string>,
): SessionMetadata => ({
  ...(generation !== undefined ? { generation } : {}),
  daemonPid: 2147483647,
  command: "true",
  args: [],
  displayCommand: "true",
  cwd: root,
  createdAt: "2026-01-01T00:00:00.000Z",
  exitedAt: "2026-01-01T00:00:01.000Z",
  exitCode: 0,
  tags,
});

afterEach(async () => {
  await terminateAndWait(daemonPids.splice(0));
  delete process.env.PTY_ROOT;
  for (const root of roots.splice(0)) {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

describe("gc generation compare-and-swap", () => {
  it("preserves a concurrent presentation update while committing flapping", () => {
    const root = makeRoot();
    const name = "flap-presentation";
    const observed = metadata(root, "same-generation", { strategy: "permanent" });
    fs.writeFileSync(path.join(root, `${name}.json`), JSON.stringify({
      ...observed,
      displayName: "new presentation",
      tags: { ...observed.tags, presentation: "current" },
    }));

    expect(commitObservedFlapping(
      name,
      observed,
      { "strategy.status": "flapping" },
      { counter: 3, limit: 3, window: 60 },
    )).toBe(true);
    const current = JSON.parse(fs.readFileSync(path.join(root, `${name}.json`), "utf-8"));
    expect(current.displayName).toBe("new presentation");
    expect(current.tags).toMatchObject({
      presentation: "current",
      "strategy.status": "flapping",
    });
    const events = fs.readFileSync(path.join(root, `${name}.events.jsonl`), "utf-8")
      .trimEnd().split("\n").map((line) => JSON.parse(line));
    expect(events.filter((event) => event.type === "session_flapping")).toHaveLength(1);
  });

  it("does not mark or emit against a replacement generation", () => {
    const root = makeRoot();
    const name = "flap-replacement";
    const observed = metadata(root, "old", { strategy: "permanent" });
    const replacement = metadata(root, "replacement", {
      strategy: "permanent",
      presentation: "replacement",
    });
    fs.writeFileSync(path.join(root, `${name}.json`), JSON.stringify(replacement));

    expect(commitObservedFlapping(
      name,
      observed,
      { "strategy.status": "flapping" },
      { counter: 3, limit: 3, window: 60 },
    )).toBe(false);
    expect(JSON.parse(fs.readFileSync(path.join(root, `${name}.json`), "utf-8"))).toEqual(replacement);
    expect(fs.existsSync(path.join(root, `${name}.events.jsonl`))).toBe(false);
  });

  it("does not residual-sweep a replacement generation", async () => {
    const root = makeRoot();
    const name = "residual";
    const observed = metadata(root, "old");
    fs.writeFileSync(
      path.join(root, `${name}.json`),
      JSON.stringify(metadata(root, "replacement")),
    );
    const session: SessionInfo = {
      name,
      socketPath: path.join(root, `${name}.sock`),
      pid: null,
      status: "exited",
      metadata: observed,
    };

    const removed = await cleanupObservedSession(session);

    expect(removed).toBe(false);
    expect(JSON.parse(
      fs.readFileSync(path.join(root, `${name}.json`), "utf-8"),
    ).generation).toBe("replacement");
  });

  it("cleans an unchanged legacy observation without a generation", async () => {
    const root = makeRoot();
    const name = "legacy-exact";
    const observed = metadata(root);
    fs.writeFileSync(
      path.join(root, `${name}.json`),
      JSON.stringify(observed),
    );
    const session: SessionInfo = {
      name,
      socketPath: path.join(root, `${name}.sock`),
      pid: null,
      status: "exited",
      metadata: observed,
    };

    expect(await cleanupObservedSession(session)).toBe(true);
    expect(fs.existsSync(path.join(root, `${name}.json`))).toBe(false);
  });

  it("preserves changed legacy metadata without a generation", async () => {
    const root = makeRoot();
    const name = "legacy-stale";
    const observed = metadata(root, undefined, { revision: "old" });
    fs.writeFileSync(
      path.join(root, `${name}.json`),
      JSON.stringify(metadata(root, undefined, { revision: "replacement" })),
    );
    const session: SessionInfo = {
      name,
      socketPath: path.join(root, `${name}.sock`),
      pid: null,
      status: "exited",
      metadata: observed,
    };

    expect(await cleanupObservedSession(session)).toBe(false);
    expect(JSON.parse(
      fs.readFileSync(path.join(root, `${name}.json`), "utf-8"),
    ).tags).toEqual({ revision: "replacement" });
  });

  it("does not respawn over a replacement generation", async () => {
    const root = makeRoot();
    const name = "permanent";
    const tags = { strategy: "permanent" };
    const observed = metadata(root, "old", tags);
    fs.writeFileSync(
      path.join(root, `${name}.json`),
      JSON.stringify(metadata(root, "replacement", tags)),
    );

    const respawned = await respawnPermanent(name, observed);

    expect(respawned).toBe(false);
    expect(JSON.parse(
      fs.readFileSync(path.join(root, `${name}.json`), "utf-8"),
    ).generation).toBe("replacement");
  });

  it("keeps the CAS lock across bundled CLI fallback respawn", async () => {
    const root = makeRoot();
    const name = "fallback";
    const observed = metadata(root, "same", { strategy: "permanent" });
    observed.command = "/bin/sh";
    observed.args = ["-c", "sleep 30"];
    observed.displayCommand = "sleep 30";
    fs.writeFileSync(
      path.join(root, `${name}.json`),
      JSON.stringify(observed),
    );

    const oldPath = process.env.PATH ?? "";
    process.env.PATH = `${path.resolve(import.meta.dirname, "../bin")}:${oldPath}`;
    try {
      expect(await respawnPermanent(name, observed)).toBe(true);
    } finally {
      process.env.PATH = oldPath;
    }

    const pid = parseInt(
      fs.readFileSync(path.join(root, `${name}.pid`), "utf-8").trim(),
      10,
    );
    daemonPids.push(pid);
    expect(pid).toBeGreaterThan(0);
  }, 15_000);
});
