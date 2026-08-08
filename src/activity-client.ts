import * as net from "node:net";
import { randomUUID } from "node:crypto";
import {
  type ActivityClaim,
  type ActivityResponse,
  type ActivityState,
  type ActivityStatus,
  type ActivityUpdate,
} from "./activity.ts";
import { MessageType, PacketReader, encodeActivity } from "./protocol.ts";
import { getSocketPath } from "./sessions.ts";

export interface ActivityPublisherOptions {
  producerEpoch?: string;
  source?: string;
  timeoutMs?: number;
}

export interface ActivityPublishOptions {
  turnId?: string;
}

export class ActivityPublisher {
  private readonly reader = new PacketReader();
  private pending: {
    resolve: (value: ActivityStatus) => void;
    reject: (error: Error) => void;
    timer: NodeJS.Timeout;
  } | null = null;
  private sequence = 0;
  private closed = false;

  private constructor(
    private readonly socket: net.Socket,
    readonly producerEpoch: string,
    private readonly timeoutMs: number,
  ) {
    socket.on("data", (data) => {
      let packets;
      try {
        packets = this.reader.feed(Buffer.isBuffer(data) ? data : Buffer.from(data));
      } catch (error) {
        this.failPending(error instanceof Error ? error : new Error(String(error)));
        socket.destroy();
        return;
      }
      for (const packet of packets) {
        if (packet.type !== MessageType.ACTIVITY || this.pending === null) continue;
        let response: ActivityResponse;
        try {
          response = JSON.parse(packet.payload.toString("utf8")) as ActivityResponse;
        } catch {
          this.failPending(new Error("invalid activity response"));
          socket.destroy();
          return;
        }
        const pending = this.pending;
        this.pending = null;
        clearTimeout(pending.timer);
        if (!response.ok) {
          pending.reject(new Error(response.error ?? "activity update rejected"));
        } else {
          this.sequence = response.activity.sequence;
          pending.resolve(response.activity);
        }
      }
    });
    socket.on("error", (error) => this.failPending(error));
    socket.on("close", () => {
      this.closed = true;
      this.failPending(new Error("activity publisher connection closed"));
    });
  }

  static async connect(
    name: string,
    options: ActivityPublisherOptions = {},
  ): Promise<ActivityPublisher> {
    const socket = net.createConnection(getSocketPath(name));
    await new Promise<void>((resolve, reject) => {
      socket.once("connect", resolve);
      socket.once("error", reject);
    });
    const publisher = new ActivityPublisher(
      socket,
      options.producerEpoch ?? randomUUID(),
      options.timeoutMs ?? 2000,
    );
    const claim: ActivityClaim = {
      op: "claim",
      producerEpoch: publisher.producerEpoch,
      ...(options.source === undefined ? {} : { source: options.source }),
    };
    try {
      await publisher.request(claim);
      return publisher;
    } catch (error) {
      publisher.close();
      throw error;
    }
  }

  publish(
    state: ActivityState,
    options: ActivityPublishOptions = {},
  ): Promise<ActivityStatus> {
    const update: ActivityUpdate = {
      op: "set",
      producerEpoch: this.producerEpoch,
      sequence: this.sequence + 1,
      state,
      ...(options.turnId === undefined ? {} : { turnId: options.turnId }),
    };
    return this.request(update);
  }

  close(): void {
    this.closed = true;
    this.socket.destroy();
  }

  private request(command: ActivityClaim | ActivityUpdate): Promise<ActivityStatus> {
    if (this.closed) return Promise.reject(new Error("activity publisher is closed"));
    if (this.pending !== null) {
      return Promise.reject(new Error("activity update already in flight"));
    }
    return new Promise<ActivityStatus>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending = null;
        reject(new Error("timed out waiting for activity response"));
        this.socket.destroy();
      }, this.timeoutMs);
      this.pending = { resolve, reject, timer };
      this.socket.write(encodeActivity(command));
    });
  }

  private failPending(error: Error): void {
    if (this.pending === null) return;
    const pending = this.pending;
    this.pending = null;
    clearTimeout(pending.timer);
    pending.reject(error);
  }
}

export function connectActivityPublisher(
  name: string,
  options: ActivityPublisherOptions = {},
): Promise<ActivityPublisher> {
  return ActivityPublisher.connect(name, options);
}
