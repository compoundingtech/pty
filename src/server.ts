import * as net from "node:net";
import * as fs from "node:fs";
import * as pty from "node-pty";
// @xterm packages are CJS-only. Named imports fail under Node's native ESM
// loader (Node v24+), so we use default imports + separate type imports.
import type { Terminal } from "@xterm/headless";
import type { SerializeAddon } from "@xterm/addon-serialize";
import xterm from "@xterm/headless";
import xtermSerialize from "@xterm/addon-serialize";
import {
  MessageType,
  PacketReader,
  encodeData,
  encodeExit,
  encodeScreen,
  encodeStatusResponse,
  decodeSize,
} from "./protocol.ts";
import {
  getSocketPath,
  getPidPath,
  ensureSessionDir,
  cleanup,
  writeMetadata,
  readMetadata,
  type SessionMetadata,
} from "./sessions.ts";

interface Client {
  socket: net.Socket;
  reader: PacketReader;
  rows: number;
  cols: number;
  readonly: boolean;
  attachSeq: number;
}

export interface ServerOptions {
  name: string;
  command: string;
  args: string[];
  displayCommand: string;
  cwd: string;
  rows: number;
  cols: number;
  onExit?: (code: number) => void;
}

const LAST_LINES_COUNT = 20;

export class PtyServer {
  private terminal: Terminal;
  private serialize: SerializeAddon;
  private ptyProcess: pty.IPty;
  private socketServer: net.Server;
  private clients = new Map<net.Socket, Client>();
  private exited = false;
  private exitCode = 0;
  private name: string;
  private options: ServerOptions;
  private attachCounter = 0;
  private sgrMouseMode = false;
  private cursorHidden = false;
  private kittyKeyboardStack: number[] = [];
  private lastResizeTime = 0;
  readonly ready: Promise<void>;

  constructor(options: ServerOptions) {
    this.name = options.name;
    this.options = options;

    // Set up xterm-headless for screen buffer tracking
    this.terminal = new xterm.Terminal({
      rows: options.rows,
      cols: options.cols,
      scrollback: 10000,
      allowProposedApi: true,
    });
    this.serialize = new xtermSerialize.SerializeAddon();
    this.terminal.loadAddon(this.serialize);

    // Track terminal modes not exposed by xterm's serialize addon
    this.terminal.parser.registerCsiHandler(
      { prefix: "?", final: "h" },
      (params) => {
        for (const p of params) {
          const v = typeof p === "number" ? p : p[0];
          if (v === 1006) this.sgrMouseMode = true;
          if (v === 25) this.cursorHidden = false;
        }
        return false;
      }
    );
    this.terminal.parser.registerCsiHandler(
      { prefix: "?", final: "l" },
      (params) => {
        for (const p of params) {
          const v = typeof p === "number" ? p : p[0];
          if (v === 1006) this.sgrMouseMode = false;
          if (v === 25) this.cursorHidden = true;
        }
        return false;
      }
    );
    this.terminal.parser.registerCsiHandler(
      { prefix: ">", final: "u" },
      (params) => {
        const flags = typeof params[0] === "number" ? params[0] : params[0][0];
        this.kittyKeyboardStack.push(flags);
        return false;
      }
    );
    this.terminal.parser.registerCsiHandler(
      { prefix: "<", final: "u" },
      () => {
        this.kittyKeyboardStack.pop();
        return false;
      }
    );

    // Spawn the child process in a PTY via a shell, so that shell scripts,
    // symlinks, and shebangs all work reliably (like tmux/screen do).
    // `exec "$@"` replaces the shell with the actual process.
    const childEnv = { ...process.env };
    delete childEnv.PTY_SERVER_CONFIG;
    try {
      this.ptyProcess = pty.spawn(
        "/bin/sh",
        ["-c", 'exec "$@"', "sh", options.command, ...options.args],
        {
          name: "xterm-256color",
          cols: options.cols,
          rows: options.rows,
          cwd: options.cwd,
          env: childEnv as Record<string, string>,
        }
      );
    } catch (err: any) {
      const msg = err?.message ?? String(err);
      if (msg.includes("posix_spawnp") || msg.includes("spawn")) {
        throw new Error(
          `Failed to spawn "${options.command}": ${msg}\nIs the command installed and executable?`
        );
      }
      throw err;
    }

    // Feed PTY output into xterm-headless and broadcast to clients
    this.ptyProcess.onData((data: string) => {
      this.terminal.write(data);
      this.broadcast(encodeData(data));
    });

    this.ptyProcess.onExit(({ exitCode }) => {
      this.exited = true;
      this.exitCode = exitCode;
      this.broadcast(encodeExit(exitCode));
      this.saveExitMetadata(exitCode);
      options.onExit?.(exitCode);
    });

    // Create Unix socket server
    ensureSessionDir();
    const socketPath = getSocketPath(this.name);

    // Remove stale socket if it exists
    try {
      fs.unlinkSync(socketPath);
    } catch {}

    this.socketServer = net.createServer((socket) =>
      this.handleClient(socket)
    );
    this.ready = new Promise((resolve) => {
      this.socketServer.listen(socketPath, () => {
        try { fs.chmodSync(socketPath, 0o600); } catch {}
        fs.writeFileSync(getPidPath(this.name), process.pid.toString());
        writeMetadata(this.name, {
          command: options.command,
          args: options.args,
          displayCommand: options.displayCommand,
          cwd: options.cwd,
          createdAt: new Date().toISOString(),
        });
        resolve();
      });
    });

    this.socketServer.on("error", (err) => {
      console.error(`Socket server error: ${err.message}`);
    });
  }

  private handleClient(socket: net.Socket): void {
    const client: Client = {
      socket,
      reader: new PacketReader(),
      rows: this.terminal.rows,
      cols: this.terminal.cols,
      readonly: false,
      attachSeq: 0,
    };
    this.clients.set(socket, client);

    socket.on("data", (data: Buffer) => {
      const packets = client.reader.feed(data);
      for (const packet of packets) {
        switch (packet.type) {
          case MessageType.ATTACH: {
            if (packet.payload.length < 4) break;
            const size = decodeSize(packet.payload);
            client.rows = size.rows;
            client.cols = size.cols;
            client.attachSeq = ++this.attachCounter;
            const resized = this.negotiateSize();

            const sendScreen = () => {
              if (socket.destroyed) return;
              const screen = this.getModePrefix() + this.serialize.serialize();
              socket.write(encodeScreen(screen));
              if (this.exited) {
                socket.write(encodeExit(this.exitCode));
              } else {
                // The serialize addon's output is an approximation — ECH/CUF
                // sequences may not perfectly reproduce what the app originally
                // drew (e.g., background fills in ratatui). Nudge the child
                // with a SIGWINCH so it does a fresh full redraw, whose DATA
                // overwrites any serialize artifacts on the client.
                this.nudgeRedraw();
              }
            };

            if (!this.exited) {
              // If the PTY was just resized (either by this attach or
              // recently by another client), wait for the process to
              // redraw before serializing. Without this delay, the client
              // sees a transient mid-redraw state.
              const sinceLast = Date.now() - this.lastResizeTime;
              const REDRAW_SETTLE_MS = 80;
              if (resized || sinceLast < REDRAW_SETTLE_MS) {
                const delay = resized ? REDRAW_SETTLE_MS : REDRAW_SETTLE_MS - sinceLast;
                setTimeout(sendScreen, delay);
              } else {
                sendScreen();
              }
            } else {
              sendScreen();
            }
            break;
          }

          case MessageType.PEEK: {
            client.readonly = true;
            const plain = packet.payload.length > 0 && packet.payload.readUInt8(0) === 1;

            if (plain) {
              socket.write(encodeScreen(this.getPlainScreen()));
            } else {
              // Send current screen state (same as ATTACH)
              const peekScreen = this.getModePrefix() + this.serialize.serialize();
              socket.write(encodeScreen(peekScreen));
            }

            if (this.exited) {
              socket.write(encodeExit(this.exitCode));
            }
            break;
          }

          case MessageType.DATA: {
            if (!this.exited && !client.readonly) {
              this.ptyProcess.write(packet.payload.toString());
            }
            break;
          }

          case MessageType.RESIZE: {
            if (!client.readonly && packet.payload.length >= 4) {
              const size = decodeSize(packet.payload);
              client.rows = size.rows;
              client.cols = size.cols;
              client.attachSeq = ++this.attachCounter;
              this.negotiateSize();
            }
            break;
          }

          case MessageType.DETACH: {
            socket.end();
            break;
          }

          case MessageType.STATUS: {
            const stats = this.collectStats();
            socket.write(encodeStatusResponse(JSON.stringify(stats)));
            break;
          }
        }
      }
    });

    socket.on("close", () => {
      this.clients.delete(socket);
      this.negotiateSize();
    });

    socket.on("error", () => {
      this.clients.delete(socket);
      this.negotiateSize();
    });
  }

  private getModePrefix(): string {
    let prefix = "";
    if (this.sgrMouseMode) prefix += "\x1b[?1006h";
    if (this.cursorHidden) prefix += "\x1b[?25l";
    for (const flags of this.kittyKeyboardStack) {
      prefix += `\x1b[>${flags}u`;
    }
    return prefix;
  }

  private collectStats(): object {
    const buf = this.terminal.buffer.active;
    const meta = readMetadata(this.name);

    let attached = 0;
    let readOnly = 0;
    for (const c of this.clients.values()) {
      if (c.readonly) readOnly++;
      else if (c.attachSeq > 0) attached++;
    }

    const createdAt = meta?.createdAt ?? null;
    const uptimeSeconds = createdAt
      ? Math.floor((Date.now() - new Date(createdAt).getTime()) / 1000)
      : null;

    return {
      name: this.name,
      terminal: {
        cols: this.terminal.cols,
        rows: this.terminal.rows,
        cursorX: buf.cursorX,
        cursorY: buf.cursorY,
        scrollbackUsed: buf.length,
        scrollbackCapacity: this.terminal.rows + (this.terminal.options.scrollback ?? 10000),
      },
      process: {
        alive: !this.exited,
        exitCode: this.exited ? this.exitCode : null,
      },
      clients: {
        total: this.clients.size,
        attached,
        readOnly,
      },
      modes: {
        sgrMouse: this.sgrMouseMode,
        cursorHidden: this.cursorHidden,
        kittyKeyboard: this.kittyKeyboardStack.length > 0,
        kittyKeyboardFlags: [...this.kittyKeyboardStack],
      },
      uptimeSeconds,
      createdAt,
    };
  }

  /** Resize the PTY to the smallest dimensions across all connected writable clients.
   *  Returns true if the size actually changed. */
  private negotiateSize(): boolean {
    let rows = 0;
    let cols = 0;

    for (const client of this.clients.values()) {
      if (!client.readonly && client.attachSeq > 0) {
        rows = rows === 0 ? client.rows : Math.min(rows, client.rows);
        cols = cols === 0 ? client.cols : Math.min(cols, client.cols);
      }
    }

    if (rows > 0 && cols > 0) {
      if (rows !== this.terminal.rows || cols !== this.terminal.cols) {
        this.ptyProcess.resize(cols, rows);
        this.terminal.resize(cols, rows);
        this.lastResizeTime = Date.now();
        return true;
      }
    }
    return false;
  }

  /** Briefly resize the PTY by 1 column and back to trigger SIGWINCH,
   *  forcing the child to do a complete redraw. The xterm-headless terminal
   *  is resized in sync so its buffer stays correct. */
  private nudgeRedraw(): void {
    const cols = this.terminal.cols;
    const rows = this.terminal.rows;
    this.ptyProcess.resize(cols - 1, rows);
    this.terminal.resize(cols - 1, rows);
    this.ptyProcess.resize(cols, rows);
    this.terminal.resize(cols, rows);
  }

  private broadcast(data: Buffer): void {
    for (const client of this.clients.values()) {
      client.socket.write(data);
    }
  }

  private getPlainScreen(): string {
    const buffer = this.terminal.buffer.active;
    const lines: string[] = [];
    for (let i = 0; i < buffer.length; i++) {
      const line = buffer.getLine(i);
      if (line) {
        lines.push(line.translateToString(true));
      }
    }
    // Trim trailing empty lines
    while (lines.length > 0 && lines[lines.length - 1] === "") {
      lines.pop();
    }
    return lines.join("\n");
  }

  private getLastLines(): string[] {
    const buffer = this.terminal.buffer.active;
    const lines: string[] = [];
    for (let i = 0; i < buffer.length; i++) {
      const line = buffer.getLine(i);
      if (line) {
        lines.push(line.translateToString(true));
      }
    }
    // Trim trailing empty lines, then take last N
    while (lines.length > 0 && lines[lines.length - 1] === "") {
      lines.pop();
    }
    return lines.slice(-LAST_LINES_COUNT);
  }

  private saveExitMetadata(exitCode: number): void {
    const existing = readMetadata(this.name);
    writeMetadata(this.name, {
      command: this.options.command,
      args: this.options.args,
      displayCommand: this.options.displayCommand,
      cwd: this.options.cwd,
      createdAt: existing?.createdAt ?? new Date().toISOString(),
      exitCode,
      exitedAt: new Date().toISOString(),
      lastLines: this.getLastLines(),
    });
  }

  /** Clean up resources. Does not call process.exit(). */
  close(): Promise<void> {
    return new Promise((resolve) => {
      for (const client of this.clients.values()) {
        client.socket.destroy();
      }
      this.socketServer.close(() => {
        cleanup(this.name);
        try {
          this.ptyProcess.kill();
        } catch {}
        resolve();
      });
    });
  }
}

/** Entry point when this file is run as the daemon process. */
if (process.argv[1]?.endsWith("/server.js")) {
  const config = JSON.parse(process.env.PTY_SERVER_CONFIG ?? "{}");
  if (!config.name || !config.command) {
    console.error("PTY_SERVER_CONFIG env var required");
    process.exit(1);
  }

  const server = new PtyServer({
    name: config.name,
    command: config.command,
    args: config.args ?? [],
    displayCommand: config.displayCommand,
    cwd: config.cwd ?? process.cwd(),
    rows: config.rows ?? 24,
    cols: config.cols ?? 80,
    onExit: (code) => {
      // Give clients a moment to receive the exit message, then shut down
      setTimeout(() => server.close().then(() => process.exit(code)), 500);
    },
  });

  process.on("SIGTERM", () => server.close().then(() => process.exit(0)));
  process.on("SIGINT", () => server.close().then(() => process.exit(0)));
}
