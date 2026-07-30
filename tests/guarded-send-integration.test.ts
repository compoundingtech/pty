import { afterAll, afterEach, describe, expect, it } from "vitest";
import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { PtyServer, type ServerOptions } from "../src/server.ts";
import {
  MessageType,
  PacketReader,
  encodeAttach,
  encodeData,
  encodeGuardedData,
  encodePeek,
} from "../src/protocol.ts";
import { compareAndSend } from "../src/guarded-send-client.ts";
import { connectActivityPublisher } from "../src/activity-client.ts";
import { queryStats, type StatsResult } from "../src/client.ts";
import { cleanupAll, getSocketPath } from "../src/sessions.ts";

const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-guarded-"));
const sessionDir = fs.mkdtempSync(path.join(testRoot, "sessions-"));
process.env.PTY_SESSION_DIR = sessionDir;

let servers: PtyServer[] = [];
let names: string[] = [];

afterAll(() => {
  fs.rmSync(testRoot, { recursive: true, force: true });
});

afterEach(async () => {
  for (const server of servers) await server.close();
  for (const name of names) cleanupAll(name);
  servers = [];
  names = [];
});

function uniqueName(): string {
  const name = `guarded-${process.pid}-${Date.now()}-${Math.random().toString(36).slice(2, 7)}`;
  names.push(name);
  return name;
}

async function startServer(
  name: string,
  command = "sleep",
  args = ["30"],
  options: Partial<ServerOptions> = {},
): Promise<PtyServer> {
  const server = new PtyServer({
    name,
    command,
    args,
    displayCommand: command,
    cwd: testRoot,
    rows: 24,
    cols: 80,
    ...options,
  });
  servers.push(server);
  await server.ready;
  return server;
}

function connect(name: string): Promise<net.Socket> {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection(getSocketPath(name));
    socket.once("connect", () => resolve(socket));
    socket.once("error", reject);
  });
}

async function waitForRevisionChange(
  name: string,
  revision: number,
): Promise<StatsResult> {
  const deadline = Date.now() + 3000;
  while (Date.now() < deadline) {
    const stats = await queryStats(name);
    if (stats.ioRevision !== revision) return stats;
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  throw new Error(`timed out waiting for revision after ${revision}`);
}

async function waitForFile(pathname: string, pattern: string): Promise<string> {
  const deadline = Date.now() + 3000;
  while (Date.now() < deadline) {
    const text = fs.existsSync(pathname) ? fs.readFileSync(pathname, "utf8") : "";
    if (text.includes(pattern)) return text;
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  return fs.existsSync(pathname) ? fs.readFileSync(pathname, "utf8") : "";
}

describe("guarded compare-and-send", () => {
  it("succeeds once for an unchanged token and rejects replay with zero bytes", async () => {
    const name = uniqueName();
    const captured = path.join(testRoot, `${name}.txt`);
    await startServer(name, "sh", ["-c", 'cat > "$1"', "sh", captured]);
    const before = await queryStats(name);

    const sent = await compareAndSend(name, {
      generation: before.generation,
      ioRevision: before.ioRevision,
      data: "ONCE\n",
    });
    expect(sent.ok).toBe(true);
    expect(sent.ioRevision).toBeGreaterThan(before.ioRevision);
    expect(await waitForFile(captured, "ONCE")).toContain("ONCE");

    const replay = await compareAndSend(name, {
      generation: before.generation,
      ioRevision: before.ioRevision,
      data: "TWICE\n",
    });
    expect(replay.ok).toBe(false);
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(fs.readFileSync(captured, "utf8")).not.toContain("TWICE");
  });

  for (const [label, bytes] of [
    ["key-shaped", "x"],
    ["newline-shaped", "\n"],
    ["paste-shaped", "\x1b[200~draft\x1b[201~"],
    ["escape-shaped", "\x1b"],
  ] as const) {
    it(`rejects after ${label} ordinary input without interpreting it`, async () => {
      const name = uniqueName();
      const captured = path.join(testRoot, `${name}.txt`);
      await startServer(name, "sh", ["-c", 'cat > "$1"', "sh", captured]);
      const before = await queryStats(name);
      const ordinary = await connect(name);
      ordinary.write(encodeData(bytes));
      await waitForRevisionChange(name, before.ioRevision);

      const result = await compareAndSend(name, {
        generation: before.generation,
        ioRevision: before.ioRevision,
        data: "GUARDED\n",
      });
      expect(result.ok).toBe(false);
      await new Promise((resolve) => setTimeout(resolve, 50));
      const text = fs.existsSync(captured) ? fs.readFileSync(captured, "utf8") : "";
      expect(text).not.toContain("GUARDED");
      ordinary.destroy();
    });
  }

  it("rejects after child output and writes zero guarded bytes", async () => {
    const name = uniqueName();
    await startServer(name, process.execPath, [
      "-e",
      "process.on('SIGUSR1',()=>process.stdout.write('RACE'));setInterval(()=>{},1000)",
    ]);
    const before = await queryStats(name);
    process.kill(before.process.pid!, "SIGUSR1");
    await waitForRevisionChange(name, before.ioRevision);

    const result = await compareAndSend(name, {
      generation: before.generation,
      ioRevision: before.ioRevision,
      data: "GUARDED",
    });
    expect(result.ok).toBe(false);
  });

  it("rejects after an actual resize but allows an attached idle viewer", async () => {
    const unchangedName = uniqueName();
    await startServer(unchangedName);
    const unchanged = await queryStats(unchangedName);
    const viewer = await connect(unchangedName);
    viewer.write(encodeAttach(24, 80));
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect((await queryStats(unchangedName)).ioRevision).toBe(unchanged.ioRevision);
    expect((await compareAndSend(unchangedName, {
      generation: unchanged.generation,
      ioRevision: unchanged.ioRevision,
      data: "x",
    })).ok).toBe(true);
    viewer.destroy();

    const resizedName = uniqueName();
    await startServer(resizedName);
    const beforeResize = await queryStats(resizedName);
    const resizer = await connect(resizedName);
    resizer.write(encodeAttach(23, 79));
    await waitForRevisionChange(resizedName, beforeResize.ioRevision);
    expect((await compareAndSend(resizedName, {
      generation: beforeResize.generation,
      ioRevision: beforeResize.ioRevision,
      data: "x",
    })).ok).toBe(false);
    resizer.destroy();
  });

  it("rejects a token from a replaced daemon generation", async () => {
    const name = uniqueName();
    const first = await startServer(name, "sleep", ["30"], {
      generation: "generation-a",
    });
    const before = await queryStats(name);
    await first.close();
    servers = servers.filter((server) => server !== first);
    cleanupAll(name);
    await startServer(name, "sleep", ["30"], {
      generation: "generation-b",
    });

    const result = await compareAndSend(name, {
      generation: before.generation,
      ioRevision: before.ioRevision,
      data: "x",
    });
    expect(result).toMatchObject({
      ok: false,
      generation: "generation-b",
    });
  });

  it("keeps the activity lease and publisher usable after a failed guard", async () => {
    const name = uniqueName();
    await startServer(name);
    const activity = await connectActivityPublisher(name, {
      producerEpoch: "activity-a",
      source: "adapter",
    });
    await activity.publish("idle");
    const before = await queryStats(name);
    const ordinary = await connect(name);
    ordinary.write(encodeData("race"));
    await waitForRevisionChange(name, before.ioRevision);

    expect((await compareAndSend(name, {
      generation: before.generation,
      ioRevision: before.ioRevision,
      data: "x",
    })).ok).toBe(false);
    expect((await queryStats(name)).activity.state).toBe("idle");
    await expect(activity.publish("active")).resolves.toMatchObject({
      state: "active",
    });
    ordinary.destroy();
    activity.close();
  });

  it("rejects when activity changes after the guarded snapshot", async () => {
    const name = uniqueName();
    await startServer(name);
    const activity = await connectActivityPublisher(name, {
      producerEpoch: "activity-race",
      source: "adapter",
    });
    await activity.publish("idle");
    const before = await queryStats(name);
    await activity.publish("active");

    expect((await compareAndSend(name, {
      generation: before.generation,
      ioRevision: before.ioRevision,
      data: "x",
    })).ok).toBe(false);
    activity.close();
  });

  it("rejects a valid guard on a read-only socket without closing it", async () => {
    const name = uniqueName();
    await startServer(name);
    const before = await queryStats(name);
    const socket = await connect(name);
    const reader = new PacketReader();
    socket.write(encodePeek());
    socket.write(encodeGuardedData({
      generation: before.generation,
      ioRevision: before.ioRevision,
      data: "x",
    }));

    const response = await new Promise<Record<string, unknown>>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("guard response timeout")), 3000);
      socket.on("data", (data) => {
        for (const packet of reader.feed(
          Buffer.isBuffer(data) ? data : Buffer.from(data),
        )) {
          if (packet.type === MessageType.GUARDED_DATA) {
            clearTimeout(timer);
            resolve(JSON.parse(packet.payload.toString()));
          }
        }
      });
    });
    expect(response.ok).toBe(false);
    expect(socket.destroyed).toBe(false);
    socket.destroy();
  });

  it("rejects live malformed and oversized commands without writing data", async () => {
    const name = uniqueName();
    const captured = path.join(testRoot, `${name}.txt`);
    await startServer(name, "sh", ["-c", 'cat > "$1"', "sh", captured]);
    const before = await queryStats(name);

    for (const command of [
      {
        generation: before.generation,
        ioRevision: before.ioRevision,
        data: "MALFORMED\n",
        semanticKey: "enter",
      },
      {
        generation: before.generation,
        ioRevision: before.ioRevision,
        data: "OVERSIZED".repeat(8193),
      },
    ]) {
      const socket = await connect(name);
      const reader = new PacketReader();
      socket.write(encodeGuardedData(command));
      const response = await new Promise<Record<string, unknown>>((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error("guard response timeout")), 3000);
        socket.on("data", (data) => {
          for (const packet of reader.feed(
            Buffer.isBuffer(data) ? data : Buffer.from(data),
          )) {
            if (packet.type === MessageType.GUARDED_DATA) {
              clearTimeout(timer);
              resolve(JSON.parse(packet.payload.toString()));
            }
          }
        });
      });
      expect(response.ok).toBe(false);
      socket.destroy();
    }

    await new Promise((resolve) => setTimeout(resolve, 50));
    const text = fs.existsSync(captured) ? fs.readFileSync(captured, "utf8") : "";
    expect(text).not.toContain("MALFORMED");
    expect(text).not.toContain("OVERSIZED");
  });
});
