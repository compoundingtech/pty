import { afterAll, describe, expect, it } from "vitest";
import { Buffer } from "node:buffer";
import { spawn, spawnSync } from "node:child_process";
import * as fs from "node:fs";
import * as net from "node:net";
import * as nodePty from "node-pty";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import {
  MessageType,
  PacketReader,
  decodeSize,
  encodeExit,
  encodeGeometry,
  encodePacket,
  encodeScreen,
} from "../src/protocol.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const clientUrl = pathToFileURL(path.join(__dirname, "..", "dist", "client.js")).href;
const nodeBin = process.execPath;
const testRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-attach-stream-"));

afterAll(() => {
  fs.rmSync(testRoot, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
});

function collect(stream: NodeJS.ReadableStream): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    stream.on("data", (chunk) => chunks.push(Buffer.from(chunk)));
    stream.on("end", () => resolve(Buffer.concat(chunks)));
    stream.on("error", reject);
  });
}

async function listen(): Promise<{ server: net.Server; port: number }> {
  const server = net.createServer();
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("expected TCP address");
  return { server, port: address.port };
}

function attachScript(port: number, checkOwnership = true): string {
  return `
    import fs from "node:fs";
    import net from "node:net";
    import { attach } from ${JSON.stringify(clientUrl)};
    const socket = net.createConnection({ host: "127.0.0.1", port: ${port} });
    socket.once("connect", () => attach({
      name: "fixture",
      socket,
      attachStreamFdV1: 3,
      onExit: (code) => ${checkOwnership ? `{
        try {
          fs.writeSync(3, Buffer.alloc(0));
          process.exit(code);
        } catch (error) {
          process.stderr.write(\`caller lost ownership of fd 3: \${error.message}\\n\`);
          process.exit(99);
        }
      }` : "process.exit(code)"},
      onDetach: () => process.exit(0),
    }));
  `;
}

async function runAgainstFakeDaemon(
  send: (socket: net.Socket) => void,
): Promise<{ status: number | null; stdout: Buffer; stderr: Buffer; stream: Buffer }> {
  const { server, port } = await listen();
  const connected = new Promise<void>((resolve) => {
    server.once("connection", (socket) => {
      socket.once("data", () => send(socket));
      resolve();
    });
  });
  const child = spawn(nodeBin, ["--input-type=module", "-e", attachScript(port)], {
    stdio: ["pipe", "pipe", "pipe", "pipe"],
  });
  const stdout = collect(child.stdout);
  const stderr = collect(child.stderr);
  const stream = collect(child.stdio[3] as NodeJS.ReadableStream);
  await connected;
  const status = await new Promise<number | null>((resolve) => child.once("exit", resolve));
  server.close();
  return { status, stdout: await stdout, stderr: await stderr, stream: await stream };
}

describe("pty attach --attach-stream-fd-v1", () => {
  it("documents the versioned inherited-FD contract", () => {
    const result = spawnSync(nodeBin, [cliPath, "attach", "--help"], { encoding: "utf8" });
    expect(result.status).toBe(0);
    expect(result.stdout).toContain("--attach-stream-fd-v1 <fd>");
    expect(result.stdout).toMatch(/GEOMETRY.*SCREEN.*DATA.*EXIT/s);
  });

  it("rejects an invalid descriptor before resolving the session", () => {
    const result = spawnSync(
      nodeBin,
      [cliPath, "attach", "--attach-stream-fd-v1", "999999", "missing"],
      { encoding: "utf8" },
    );
    expect(result.status).not.toBe(0);
    expect(result.stderr).toMatch(/attach-stream-fd-v1.*999999.*not writable/i);
    expect(result.stderr).not.toContain('Session "missing"');
    expect(result.stdout).toBe("");
  });

  it("rejects a missing descriptor value", () => {
    const result = spawnSync(nodeBin, [cliPath, "attach", "--attach-stream-fd-v1"], {
      encoding: "utf8",
    });
    expect(result.status).not.toBe(0);
    expect(result.stderr).toMatch(/attach-stream-fd-v1 requires a file descriptor/);
    expect(result.stdout).toBe("");
  });

  it("reframes fragmented and coalesced daemon packets in order without stdout output", async () => {
    const geometry = encodeGeometry(31, 97);
    const screen = encodeScreen("\x1b[31mred\x1b[0m");
    const data = encodePacket(MessageType.DATA, Buffer.from("live"));
    const exit = encodeExit(0);

    const result = await runAgainstFakeDaemon((socket) => {
      socket.write(geometry.subarray(0, 2));
      socket.write(Buffer.concat([geometry.subarray(2), screen, data, exit]));
    });

    expect(result.status).toBe(0);
    expect(result.stdout).toEqual(Buffer.alloc(0));
    expect(result.stderr).toEqual(Buffer.alloc(0));
    const packets = new PacketReader().feed(result.stream);
    expect(packets.map((packet) => packet.type)).toEqual([
      MessageType.GEOMETRY,
      MessageType.SCREEN,
      MessageType.DATA,
      MessageType.EXIT,
    ]);
    expect(decodeSize(packets[0].payload)).toEqual({ rows: 31, cols: 97 });
    expect(packets[1].payload.toString()).toBe("\x1b[31mred\x1b[0m");
    expect(packets[2].payload.toString()).toBe("live");
  });

  it("keeps stdout as the controlling TTY for the advertised attach geometry", async () => {
    const { server, port } = await listen();
    let attachedSize: { rows: number; cols: number } | undefined;
    server.once("connection", (socket) => {
      const reader = new PacketReader();
      socket.on("data", (chunk) => {
        for (const packet of reader.feed(Buffer.from(chunk))) {
          if (packet.type !== MessageType.ATTACH) continue;
          attachedSize = decodeSize(packet.payload);
          socket.end(Buffer.concat([
            encodeGeometry(attachedSize.rows, attachedSize.cols),
            encodeScreen("screen stays off stdout"),
            encodeExit(0),
          ]));
        }
      });
    });
    const scriptPath = path.join(testRoot, "controlling-tty.mjs");
    const streamPath = path.join(testRoot, "controlling-tty.stream");
    fs.writeFileSync(scriptPath, `
      import net from "node:net";
      import { attach } from ${JSON.stringify(clientUrl)};
      const socket = net.createConnection({ host: "127.0.0.1", port: ${port} });
      socket.once("connect", () => attach({
        name: "fixture",
        socket,
        attachStreamFdV1: 3,
        onExit: (code) => process.exit(code),
      }));
    `);
    const terminal = nodePty.spawn(
      "/bin/sh",
      ["-c", `exec ${nodeBin} ${scriptPath} 3>${streamPath}`],
      { name: "xterm-256color", cols: 91, rows: 27, env: process.env as Record<string, string> },
    );
    let terminalOutput = "";
    terminal.onData((data) => { terminalOutput += data; });
    const exit = await new Promise<{ exitCode: number }>((resolve) => terminal.onExit(resolve));
    server.close();

    expect(exit.exitCode).toBe(0);
    expect(terminalOutput).toBe("");
    expect(attachedSize).toEqual({ rows: 27, cols: 91 });
    const packets = new PacketReader().feed(fs.readFileSync(streamPath));
    expect(packets.map((packet) => packet.type)).toEqual([
      MessageType.GEOMETRY,
      MessageType.SCREEN,
      MessageType.EXIT,
    ]);
  });

  it("fails clearly when the daemon does not support the v1 geometry contract", async () => {
    const result = await runAgainstFakeDaemon((socket) => {
      socket.write(encodeScreen("legacy screen"));
    });

    expect(result.status).not.toBe(0);
    expect(result.stdout).toEqual(Buffer.alloc(0));
    expect(result.stream).toEqual(Buffer.alloc(0));
    expect(result.stderr.toString()).toMatch(/daemon does not support attach stream v1/i);
  });

  it("fails when the connection closes without a framed EXIT event", async () => {
    const result = await runAgainstFakeDaemon((socket) => {
      socket.end(Buffer.concat([encodeGeometry(24, 80), encodeScreen("truncated")]));
    });

    expect(result.status).toBe(1);
    expect(result.stdout).toEqual(Buffer.alloc(0));
    expect(result.stderr.toString()).toMatch(/machine stream ended before an EXIT event/i);
    expect(new PacketReader().feed(result.stream).map((packet) => packet.type)).toEqual([
      MessageType.GEOMETRY,
      MessageType.SCREEN,
    ]);
  });

  it("fails instead of hanging when the inherited stream breaks", async () => {
    const { server, port } = await listen();
    let streamReader: { destroy(): void } | undefined;
    server.once("connection", (socket) => {
      socket.on("error", () => {});
      socket.once("data", () => {
        streamReader?.destroy();
        setTimeout(() => {
          socket.write(encodeGeometry(24, 80));
          socket.write(encodePacket(MessageType.DATA, Buffer.alloc(1024 * 1024, 65)));
        }, 10);
      });
    });
    const child = spawn(nodeBin, ["--input-type=module", "-e", attachScript(port, false)], {
      stdio: ["pipe", "pipe", "pipe", "pipe"],
    });
    const stdout = collect(child.stdout);
    const stderr = collect(child.stderr);
    streamReader = child.stdio[3] as { destroy(): void };
    const status = await new Promise<number | null>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("attach hung after machine stream failure")), 5_000);
      child.once("exit", (code) => {
        clearTimeout(timer);
        resolve(code);
      });
    });
    server.close();

    expect(status).toBe(1);
    expect(await stdout).toEqual(Buffer.alloc(0));
    expect((await stderr).toString()).toMatch(/machine stream descriptor 3 failed.*EPIPE/i);
  });

  it("keeps one framed stream across reconnects and requires fresh geometry", async () => {
    const { server, port } = await listen();
    let connection = 0;
    server.on("connection", (socket) => {
      const current = ++connection;
      socket.once("data", () => {
        if (current === 1) {
          socket.end(Buffer.concat([
            encodeGeometry(20, 70),
            encodeScreen("first"),
            encodePacket(MessageType.DATA, Buffer.from("before reconnect")),
          ]));
        } else {
          socket.end(Buffer.concat([
            encodeGeometry(21, 71),
            encodeScreen("second"),
            encodePacket(MessageType.DATA, Buffer.from("after reconnect")),
            encodeExit(0),
          ]));
        }
      });
    });
    const script = `
      import fs from "node:fs";
      import net from "node:net";
      import { attach } from ${JSON.stringify(clientUrl)};
      const dial = () => new Promise((resolve, reject) => {
        const socket = net.createConnection({ host: "127.0.0.1", port: ${port} });
        socket.once("connect", () => resolve(socket));
        socket.once("error", reject);
      });
      const socket = await dial();
      attach({
        name: "fixture",
        socket,
        attachStreamFdV1: 3,
        reconnect: dial,
        onExit: (code) => {
          try { fs.writeSync(3, Buffer.alloc(0)); process.exit(code); }
          catch { process.exit(99); }
        },
      });
    `;
    const child = spawn(nodeBin, ["--input-type=module", "-e", script], {
      stdio: ["pipe", "pipe", "pipe", "pipe"],
    });
    const stdout = collect(child.stdout);
    const stderr = collect(child.stderr);
    const stream = collect(child.stdio[3] as NodeJS.ReadableStream);
    const status = await new Promise<number | null>((resolve) => child.once("exit", resolve));
    server.close();

    expect(status).toBe(0);
    expect(await stdout).toEqual(Buffer.alloc(0));
    expect((await stderr).toString()).toMatch(/reconnecting/);
    const packets = new PacketReader().feed(await stream);
    expect(packets.map((packet) => packet.type)).toEqual([
      MessageType.GEOMETRY,
      MessageType.SCREEN,
      MessageType.DATA,
      MessageType.GEOMETRY,
      MessageType.SCREEN,
      MessageType.DATA,
      MessageType.EXIT,
    ]);
    expect(decodeSize(packets[3].payload)).toEqual({ rows: 21, cols: 71 });
  });
});
