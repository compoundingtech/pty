import { afterAll, describe, expect, it } from "vitest";
import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import {
  MessageType,
  PacketReader,
  encodeStatusResponse,
} from "../src/protocol.ts";

const dirname = path.dirname(fileURLToPath(import.meta.url));
const cliPath = path.join(dirname, "..", "dist", "cli.js");
const root = fs.mkdtempSync(path.join(os.tmpdir(), "pty-neutral-cli-"));

afterAll(() => fs.rmSync(root, { recursive: true, force: true }));

describe("pty attach --no-resize", () => {
  it("refuses an old daemon before sending ATTACH", async () => {
    const name = "old-daemon";
    const socketPath = path.join(root, `${name}.sock`);
    fs.writeFileSync(path.join(root, `${name}.pid`), `${process.pid}\n`);

    const received: number[] = [];
    const server = net.createServer((socket) => {
      const reader = new PacketReader();
      socket.on("data", (data) => {
        for (const packet of reader.feed(Buffer.from(data))) {
          received.push(packet.type);
          if (packet.type === MessageType.STATUS) {
            // Pre-capability daemon: valid stats, but no capabilities block.
            socket.write(encodeStatusResponse(JSON.stringify({ name })));
          }
        }
      });
    });
    await new Promise<void>((resolve, reject) => {
      server.once("error", reject);
      server.listen(socketPath, resolve);
    });

    const env: NodeJS.ProcessEnv = { ...process.env, PTY_ROOT: root };
    delete env.PTY_SESSION;
    const child = spawn(process.execPath, [cliPath, "attach", "--no-resize", name], {
      env,
      stdio: ["ignore", "pipe", "pipe"],
    });
    let stderr = "";
    child.stderr.on("data", (data: Buffer) => { stderr += data.toString(); });
    const code = await new Promise<number | null>((resolve, reject) => {
      const timer = setTimeout(() => {
        child.kill("SIGKILL");
        reject(new Error("CLI did not reject the old daemon"));
      }, 5000);
      child.once("exit", (value) => {
        clearTimeout(timer);
        resolve(value);
      });
    });
    await new Promise<void>((resolve) => server.close(() => resolve()));

    expect(code).not.toBe(0);
    expect(stderr).toContain("does not support --no-resize");
    expect(received).toContain(MessageType.STATUS);
    expect(received).not.toContain(MessageType.ATTACH);
  });
});
