import * as net from "node:net";
import {
  type GuardedSendCommand,
  type GuardedSendResponse,
} from "./guarded-send.ts";
import {
  MessageType,
  PacketReader,
  encodeGuardedData,
} from "./protocol.ts";
import { getSocketPath } from "./sessions.ts";

export interface CompareAndSendOptions extends GuardedSendCommand {
  timeoutMs?: number;
}

export function compareAndSend(
  name: string,
  options: CompareAndSendOptions,
): Promise<GuardedSendResponse> {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection(getSocketPath(name));
    const reader = new PacketReader();
    const timeoutMs = options.timeoutMs ?? 2000;
    const timer = setTimeout(() => {
      socket.destroy();
      reject(new Error(`Timeout sending guarded data to "${name}"`));
    }, timeoutMs);

    const finish = (
      callback: () => void,
    ): void => {
      clearTimeout(timer);
      socket.destroy();
      callback();
    };

    socket.once("connect", () => {
      socket.write(encodeGuardedData({
        generation: options.generation,
        ioRevision: options.ioRevision,
        data: options.data,
      }));
    });
    socket.on("data", (data) => {
      let packets;
      try {
        packets = reader.feed(Buffer.isBuffer(data) ? data : Buffer.from(data));
      } catch (error) {
        finish(() => reject(
          error instanceof Error ? error : new Error(String(error)),
        ));
        return;
      }
      for (const packet of packets) {
        if (packet.type !== MessageType.GUARDED_DATA) continue;
        try {
          const response = JSON.parse(
            packet.payload.toString("utf8"),
          ) as GuardedSendResponse;
          finish(() => resolve(response));
        } catch {
          finish(() => reject(new Error("invalid guarded send response")));
        }
        return;
      }
    });
    socket.once("error", (error) => {
      finish(() => reject(error));
    });
  });
}
