import * as net from "node:net";
import type { Terminal } from "@xterm/headless";
import type { SerializeAddon } from "@xterm/addon-serialize";
import xterm from "@xterm/headless";
import xtermSerialize from "@xterm/addon-serialize";
import * as pty from "node-pty";
import { PtyServer, type ServerOptions as PtyServerOptions } from "../server.ts";
import {
  MessageType,
  PacketReader,
  encodeAttach,
  encodeData,
  encodeResize,
} from "../protocol.ts";
import { getSocketPath } from "../sessions.ts";
import { resolveKey } from "../keys.ts";
import { captureScreenshot } from "./screenshot.ts";
import type { Screenshot, SpawnOptions, ServerOptions } from "./types.ts";

type Backend =
  | { kind: "spawn"; ptyProcess: pty.IPty }
  | { kind: "server"; server: PtyServer; ownsServer: boolean; socket: net.Socket; reader: PacketReader; screenCallbacks: Array<() => void>; exitCode: number | null; name: string };

let nameCounter = 0;
function autoName(): string {
  return `test-${process.pid}-${Date.now()}-${++nameCounter}`;
}

export class Session {
  private terminal: Terminal;
  private serialize: SerializeAddon;
  private backend: Backend;
  private _rows: number;
  private _cols: number;

  private constructor(
    terminal: Terminal,
    serialize: SerializeAddon,
    backend: Backend,
    rows: number,
    cols: number
  ) {
    this.terminal = terminal;
    this.serialize = serialize;
    this.backend = backend;
    this._rows = rows;
    this._cols = cols;
  }

  // ── Factories ──

  static spawn(
    command: string,
    args: string[] = [],
    opts: SpawnOptions = {}
  ): Session {
    const rows = opts.rows ?? 24;
    const cols = opts.cols ?? 80;

    const terminal = new xterm.Terminal({
      rows,
      cols,
      scrollback: 1000,
      allowProposedApi: true,
    });
    const serialize = new xtermSerialize.SerializeAddon();
    terminal.loadAddon(serialize);

    const env: Record<string, string> = {
      ...(process.env as Record<string, string>),
      ...opts.env,
    };
    delete env.PTY_SERVER_CONFIG;

    const proc = pty.spawn(command, args, {
      name: "xterm-256color",
      cols,
      rows,
      cwd: opts.cwd ?? process.cwd(),
      env,
    });

    proc.onData((data: string) => {
      terminal.write(data);
    });

    const backend: Backend = { kind: "spawn", ptyProcess: proc };
    return new Session(terminal, serialize, backend, rows, cols);
  }

  static async server(
    command: string,
    args: string[] = [],
    opts: ServerOptions = {}
  ): Promise<Session> {
    const rows = opts.rows ?? 24;
    const cols = opts.cols ?? 80;
    const name = opts.name ?? autoName();

    const server = new PtyServer({
      name,
      command,
      args,
      displayCommand: command,
      cwd: opts.cwd ?? process.cwd(),
      rows,
      cols,
    });
    await server.ready;

    const terminal = new xterm.Terminal({
      rows,
      cols,
      scrollback: 1000,
      allowProposedApi: true,
    });
    const serialize = new xtermSerialize.SerializeAddon();
    terminal.loadAddon(serialize);

    const backend: Backend = {
      kind: "server",
      server,
      ownsServer: true,
      socket: null!,
      reader: null!,
      screenCallbacks: [],
      exitCode: null,
      name,
    };

    const session = new Session(terminal, serialize, backend, rows, cols);
    await session.connectSocket();
    return session;
  }

  static async connectToExisting(
    existing: Session,
    opts: { rows?: number; cols?: number } = {}
  ): Promise<Session> {
    if (existing.backend.kind !== "server") {
      throw new Error("connectToExisting() requires a server-mode session");
    }

    const rows = opts.rows ?? existing._rows;
    const cols = opts.cols ?? existing._cols;

    const terminal = new xterm.Terminal({
      rows,
      cols,
      scrollback: 1000,
      allowProposedApi: true,
    });
    const serialize = new xtermSerialize.SerializeAddon();
    terminal.loadAddon(serialize);

    const backend: Backend = {
      kind: "server",
      server: existing.backend.server,
      ownsServer: false,
      socket: null!,
      reader: null!,
      screenCallbacks: [],
      exitCode: null,
      name: existing.backend.name,
    };

    const session = new Session(terminal, serialize, backend, rows, cols);
    await session.connectSocket();
    return session;
  }

  // ── Properties ──

  get rows(): number {
    return this._rows;
  }

  get cols(): number {
    return this._cols;
  }

  get hasExited(): boolean {
    if (this.backend.kind === "server") {
      return this.backend.exitCode !== null;
    }
    return false;
  }

  /** The PtyServer instance (server-mode only). */
  get server(): PtyServer {
    if (this.backend.kind !== "server") {
      throw new Error("server is only available in server mode");
    }
    return this.backend.server;
  }

  /** The session name (server-mode only). */
  get name(): string {
    if (this.backend.kind !== "server") {
      throw new Error("name is only available in server mode");
    }
    return this.backend.name;
  }

  // ── Input ──

  sendKeys(keys: string): void {
    if (this.backend.kind === "spawn") {
      this.backend.ptyProcess.write(keys);
    } else {
      this.backend.socket.write(encodeData(keys));
    }
  }

  press(keyName: string): void {
    this.sendKeys(resolveKey(keyName));
  }

  type(text: string): void {
    this.sendKeys(text);
  }

  // ── Screen ──

  screenshot(): Screenshot {
    return captureScreenshot(this.terminal, this.serialize);
  }

  // ── Waiting ──

  async waitForText(text: string, timeoutMs = 5000): Promise<Screenshot> {
    const start = Date.now();
    while (Date.now() - start < timeoutMs) {
      await new Promise((r) => setTimeout(r, 50));
      const ss = this.screenshot();
      if (ss.text.includes(text)) return ss;
    }
    const ss = this.screenshot();
    throw new Error(
      `Timed out after ${timeoutMs}ms waiting for "${text}".\nScreen:\n${ss.text}`
    );
  }

  async waitForAbsent(text: string, timeoutMs = 5000): Promise<Screenshot> {
    const start = Date.now();
    while (Date.now() - start < timeoutMs) {
      await new Promise((r) => setTimeout(r, 50));
      const ss = this.screenshot();
      if (!ss.text.includes(text)) return ss;
    }
    const ss = this.screenshot();
    throw new Error(
      `Timed out after ${timeoutMs}ms waiting for "${text}" to disappear.\nScreen:\n${ss.text}`
    );
  }

  async waitFor(
    predicate: (ss: Screenshot) => boolean,
    timeoutMs = 5000,
    description = "predicate"
  ): Promise<Screenshot> {
    const start = Date.now();
    while (Date.now() - start < timeoutMs) {
      await new Promise((r) => setTimeout(r, 50));
      const ss = this.screenshot();
      if (predicate(ss)) return ss;
    }
    const ss = this.screenshot();
    throw new Error(
      `Timed out after ${timeoutMs}ms waiting for ${description}.\nScreen:\n${ss.text}`
    );
  }

  // ── Server-mode only ──

  async attach(): Promise<void> {
    if (this.backend.kind !== "server") {
      throw new Error("attach() is only available in server mode");
    }
    const backend = this.backend;
    const screenPromise = new Promise<void>((resolve) => {
      const timer = setTimeout(resolve, 5000);
      backend.screenCallbacks.push(() => {
        clearTimeout(timer);
        resolve();
      });
    });
    backend.socket.write(encodeAttach(this._rows, this._cols));
    await screenPromise;
  }

  async reconnect(): Promise<void> {
    if (this.backend.kind !== "server") {
      throw new Error("reconnect() is only available in server mode");
    }
    this.backend.socket.destroy();
    await new Promise((r) => setTimeout(r, 100));
    this.terminal.reset();
    await this.connectSocket();
    await this.attach();
  }

  resize(rows: number, cols: number): void {
    if (this.backend.kind !== "server") {
      throw new Error("resize() is only available in server mode");
    }
    this._rows = rows;
    this._cols = cols;
    this.backend.socket.write(encodeResize(rows, cols));
    this.terminal.resize(cols, rows);
  }

  // ── Lifecycle ──

  async close(): Promise<void> {
    if (this.backend.kind === "spawn") {
      try {
        this.backend.ptyProcess.kill();
      } catch {}
      this.terminal.dispose();
    } else {
      this.backend.socket.destroy();
      this.terminal.dispose();
      if (this.backend.ownsServer) {
        await this.backend.server.close();
      }
    }
  }

  // ── Private ──

  private async connectSocket(): Promise<void> {
    if (this.backend.kind !== "server") return;
    const backend = this.backend;

    backend.reader = new PacketReader();
    backend.screenCallbacks = [];
    backend.exitCode = null;

    backend.socket = await new Promise<net.Socket>((resolve, reject) => {
      const s = net.createConnection(getSocketPath(backend.name));
      s.on("connect", () => resolve(s));
      s.on("error", reject);
    });

    backend.socket.on("data", (data: Buffer) => {
      const packets = backend.reader.feed(data);
      for (const packet of packets) {
        switch (packet.type) {
          case MessageType.SCREEN:
            this.terminal.reset();
            this.terminal.write(packet.payload.toString(), () => {
              const cbs = backend.screenCallbacks;
              backend.screenCallbacks = [];
              for (const cb of cbs) cb();
            });
            break;
          case MessageType.DATA:
            this.terminal.write(packet.payload.toString());
            break;
          case MessageType.EXIT:
            backend.exitCode = packet.payload.readInt32BE(0);
            break;
        }
      }
    });
  }
}
