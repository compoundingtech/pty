import * as net from "node:net";
import { EventEmitter } from "node:events";
import {
  MessageType,
  PacketReader,
  encodeAttach,
  encodeData,
  encodeDetach,
  encodePeek,
  encodeResize,
  decodeExit,
} from "./protocol.ts";
import { getSocketPath } from "./sessions.ts";
import { resolveKey } from "./keys.ts";

export interface SessionConnectionOptions {
  name: string;
  rows: number;
  cols: number;
}

export interface SendDataOptions {
  name: string;
  data: string[];
  delayMs?: number;
}

export interface PeekScreenOptions {
  name: string;
  plain?: boolean;
  full?: boolean;
}

/**
 * Programmatic bidirectional connection to a pty session.
 * Unlike the CLI `attach()`, this does not take over stdin/stdout
 * or call process.exit().
 *
 * Events:
 * - 'data' (data: string) — terminal output from the session
 * - 'screen' (screen: string) — initial screen replay on connect
 * - 'exit' (code: number) — session process exited
 * - 'close' () — connection closed
 * - 'error' (err: Error) — connection error
 */
export class SessionConnection extends EventEmitter {
  private socket: net.Socket | null = null;
  private reader = new PacketReader();
  private _connected = false;
  private options: SessionConnectionOptions;

  constructor(options: SessionConnectionOptions) {
    super();
    this.options = options;
  }

  get connected(): boolean {
    return this._connected;
  }

  connect(): Promise<string> {
    return new Promise((resolve, reject) => {
      const socketPath = getSocketPath(this.options.name);
      const socket = net.createConnection(socketPath);
      this.socket = socket;

      socket.on("connect", () => {
        this._connected = true;
        socket.write(encodeAttach(this.options.rows, this.options.cols));
      });

      let initialScreenResolved = false;

      socket.on("data", (raw: Buffer) => {
        let packets;
        try { packets = this.reader.feed(raw); } catch {
          try { socket.destroy(); } catch {}
          return;
        }
        for (const packet of packets) {
          switch (packet.type) {
            case MessageType.SCREEN: {
              const screen = packet.payload.toString();
              if (!initialScreenResolved) {
                initialScreenResolved = true;
                resolve(screen);
              }
              this.emit("screen", screen);
              break;
            }
            case MessageType.DATA:
              this.emit("data", packet.payload.toString());
              break;
            case MessageType.EXIT: {
              const code = decodeExit(packet.payload);
              this.emit("exit", code);
              break;
            }
          }
        }
      });

      socket.on("error", (err: NodeJS.ErrnoException) => {
        this._connected = false;
        let error: Error;
        if (err.code === "ENOENT" || err.code === "ECONNREFUSED") {
          error = new Error(`Session "${this.options.name}" not found or not running.`);
        } else {
          error = new Error(`Connection error: ${err.message}`);
        }
        if (!initialScreenResolved) {
          initialScreenResolved = true;
          reject(error);
        }
        this.emit("error", error);
      });

      socket.on("close", () => {
        this._connected = false;
        this.socket = null;
        if (!initialScreenResolved) {
          initialScreenResolved = true;
          reject(new Error(`Connection to "${this.options.name}" closed before screen received.`));
        }
        this.emit("close");
      });
    });
  }

  write(data: string): void {
    if (!this.socket || !this._connected) return;
    this.socket.write(encodeData(data));
  }

  press(key: string): void {
    this.write(resolveKey(key));
  }

  resize(rows: number, cols: number): void {
    if (!this.socket || !this._connected) return;
    this.socket.write(encodeResize(rows, cols));
  }

  disconnect(): void {
    if (!this.socket) return;
    this.socket.write(encodeDetach());
    this.socket.destroy();
    this._connected = false;
    this.socket = null;
  }
}

/** Send data to a session. Promise-based alternative to the CLI send(). */
export function sendData(options: SendDataOptions): Promise<void> {
  return new Promise((resolve, reject) => {
    const socketPath = getSocketPath(options.name);
    const socket = net.createConnection(socketPath);

    socket.on("connect", async () => {
      for (let i = 0; i < options.data.length; i++) {
        if (i > 0 && options.delayMs) {
          await new Promise((r) => setTimeout(r, options.delayMs));
        }
        socket.write(encodeData(options.data[i]));
      }
      socket.end();
    });

    socket.on("error", (err: NodeJS.ErrnoException) => {
      if (err.code === "ENOENT" || err.code === "ECONNREFUSED") {
        reject(new Error(`Session "${options.name}" not found or not running.`));
      } else {
        reject(new Error(`Connection error: ${err.message}`));
      }
    });

    // Resolve on 'finish' (writable side fully flushed to the kernel) rather
    // than 'close' (both halves closed). 'close' requires the server's FIN
    // to come back, which is unreliable in Linux namespace containers where
    // the auto-half-close behavior on the server's socket can stall. For a
    // fire-and-forget send over a Unix domain socket, 'finish' is exactly
    // the right guarantee: the bytes are in the server's recv buffer, which
    // the kernel will hold until the server reads them regardless of what
    // happens to our userspace socket. (closes #18)
    socket.on("finish", () => {
      resolve();
    });
  });
}

/** Get the current screen content. Promise-based alternative to the CLI peek(). */
export function peekScreen(options: PeekScreenOptions): Promise<string> {
  return new Promise((resolve, reject) => {
    const socketPath = getSocketPath(options.name);
    const reader = new PacketReader();
    const socket = net.createConnection(socketPath);

    socket.on("connect", () => {
      socket.write(encodePeek(options.plain, options.full));
    });

    socket.on("data", (raw: Buffer) => {
      let packets;
      try { packets = reader.feed(raw); } catch (err: any) {
        try { socket.destroy(); } catch {}
        reject(err);
        return;
      }
      for (const packet of packets) {
        if (packet.type === MessageType.SCREEN) {
          socket.destroy();
          resolve(packet.payload.toString());
          return;
        }
      }
    });

    socket.on("error", (err: NodeJS.ErrnoException) => {
      if (err.code === "ENOENT" || err.code === "ECONNREFUSED") {
        reject(new Error(`Session "${options.name}" not found or not running.`));
      } else {
        reject(new Error(`Connection error: ${err.message}`));
      }
    });

    socket.on("close", () => {
      reject(new Error(`Connection to "${options.name}" closed before screen received.`));
    });
  });
}
