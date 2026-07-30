import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import {
  cleanupRawCandidateGuarded,
  gc,
  inventoryRawCleanupCandidates,
  listSessions,
} from "../src/sessions.ts";

const roots: string[] = [];

const withRoot = () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "pty-list-purity-"));
  roots.push(root);
  process.env.PTY_ROOT = root;
  return root;
};

const writeStaleSession = (root: string, name: string) => {
  fs.writeFileSync(path.join(root, `${name}.sock`), "");
  fs.writeFileSync(path.join(root, `${name}.pid`), "2147483647");
  fs.writeFileSync(path.join(root, `${name}.json`), JSON.stringify({
    command: "true",
    args: [],
    displayCommand: "true",
    cwd: root,
    createdAt: new Date().toISOString(),
  }));
};

const writeRawDebris = (
  root: string,
  name: string,
  { corruptMetadata }: { corruptMetadata: boolean },
) => {
  fs.writeFileSync(path.join(root, `${name}.sock`), "");
  fs.writeFileSync(path.join(root, `${name}.pid`), "2147483647");
  if (corruptMetadata) fs.writeFileSync(path.join(root, `${name}.json`), "{");
};

afterEach(() => {
  delete process.env.PTY_ROOT;
  for (const root of roots.splice(0)) fs.rmSync(root, { recursive: true, force: true });
});

describe("listSessions is observational", () => {
  it("does not create an absent registry", async () => {
    const parent = fs.mkdtempSync(path.join(os.tmpdir(), "pty-list-purity-parent-"));
    roots.push(parent);
    const root = path.join(parent, "absent");
    process.env.PTY_ROOT = root;

    expect(await listSessions()).toEqual([]);
    expect(fs.existsSync(root)).toBe(false);
  });

  it("reports a stale-socket session in one pass without deleting artifacts", async () => {
    const root = withRoot();
    writeStaleSession(root, "stale");

    const sessions = await listSessions({ socketProbeBudgetMs: 5 });

    expect(sessions.map(({ name, status }) => ({ name, status }))).toEqual([
      { name: "stale", status: "vanished" },
    ]);
    expect(fs.existsSync(path.join(root, "stale.sock"))).toBe(true);
    expect(fs.existsSync(path.join(root, "stale.pid"))).toBe(true);
  });

  it("does not delete corrupt metadata", async () => {
    const root = withRoot();
    fs.writeFileSync(path.join(root, "corrupt.json"), "{");

    expect(await listSessions()).toEqual([]);
    expect(fs.existsSync(path.join(root, "corrupt.json"))).toBe(true);
  });

  it("leaves cleanup to gc while keeping gc dry-run non-mutating", async () => {
    const root = withRoot();
    writeStaleSession(root, "dry");

    const preview = await gc({ dryRun: true });

    expect(preview.removed).toEqual(["dry"]);
    expect(fs.existsSync(path.join(root, "dry.sock"))).toBe(true);
    expect(fs.existsSync(path.join(root, "dry.pid"))).toBe(true);
    expect(fs.existsSync(path.join(root, "dry.json"))).toBe(true);

    const applied = await gc();

    expect(applied.removed).toEqual(["dry"]);
    expect(fs.existsSync(path.join(root, "dry.sock"))).toBe(false);
    expect(fs.existsSync(path.join(root, "dry.pid"))).toBe(false);
    expect(fs.existsSync(path.join(root, "dry.json"))).toBe(false);
  });

  it.each([
    ["corrupt metadata", true],
    ["missing metadata", false],
  ])("previews and applies guarded cleanup for raw debris with %s", async (
    _label,
    corruptMetadata,
  ) => {
    const root = withRoot();
    writeRawDebris(root, "debris", { corruptMetadata });

    expect(await listSessions()).toEqual([]);
    const preview = await gc({ dryRun: true });

    expect(preview.removed).toEqual(["debris"]);
    expect(fs.existsSync(path.join(root, "debris.sock"))).toBe(true);
    expect(fs.existsSync(path.join(root, "debris.pid"))).toBe(true);
    expect(fs.existsSync(path.join(root, "debris.json"))).toBe(corruptMetadata);

    const applied = await gc();

    expect(applied.removed).toEqual(["debris"]);
    expect(fs.existsSync(path.join(root, "debris.sock"))).toBe(false);
    expect(fs.existsSync(path.join(root, "debris.pid"))).toBe(false);
    expect(fs.existsSync(path.join(root, "debris.json"))).toBe(false);
  });

  it("does not clean raw debris while another creator owns the name", async () => {
    const root = withRoot();
    writeRawDebris(root, "locked", { corruptMetadata: true });
    fs.writeFileSync(path.join(root, "locked.lock"), String(process.pid));

    const result = await gc();

    expect(result.removed).toEqual([]);
    expect(fs.existsSync(path.join(root, "locked.sock"))).toBe(true);
    expect(fs.existsSync(path.join(root, "locked.pid"))).toBe(true);
    expect(fs.existsSync(path.join(root, "locked.json"))).toBe(true);
  });

  it.each([
    ["live pid", (root: string) => {
      fs.writeFileSync(path.join(root, "changed.pid"), String(process.pid));
    }],
    ["ambiguous missing pid", (root: string) => {
      fs.unlinkSync(path.join(root, "changed.pid"));
    }],
  ])("revalidates an initially reclaimable candidate that becomes %s", async (
    _label,
    makeNonReclaimable,
  ) => {
    const root = withRoot();
    writeRawDebris(root, "changed", { corruptMetadata: false });
    const [candidate] = await inventoryRawCleanupCandidates();
    expect(candidate?.name).toBe("changed");
    if (!candidate) throw new Error("Expected raw cleanup candidate.");

    makeNonReclaimable(root);
    const cleaned = await cleanupRawCandidateGuarded(candidate);

    expect(cleaned).toBe(false);
    expect(fs.existsSync(path.join(root, "changed.sock"))).toBe(true);
  });

  it("preserves dead-pid debris when its socket is reachable", async () => {
    const root = withRoot();
    fs.writeFileSync(path.join(root, "reachable.pid"), "2147483647");
    const socketPath = path.join(root, "reachable.sock");
    const server = net.createServer();
    await new Promise<void>((resolve, reject) => {
      server.once("error", reject);
      server.listen(socketPath, resolve);
    });

    try {
      const preview = await gc({ dryRun: true });
      const applied = await gc();

      expect(preview.removed).toEqual([]);
      expect(applied.removed).toEqual([]);
      expect(fs.existsSync(socketPath)).toBe(true);
      expect(fs.existsSync(path.join(root, "reachable.pid"))).toBe(true);
    } finally {
      await new Promise<void>((resolve) => server.close(() => resolve()));
    }
  });
});
