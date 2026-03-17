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
  readonly ready: Promise<void>;

  constructor(options: ServerOptions) {
    this.name = options.name;
    this.options = options;

    // Set up xterm-headless for screen buffer tracking
    this.terminal = new xterm.Terminal({
      rows: options.rows,
      cols: options.cols,
      scrollback: 1000,
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
              }
            };

            if (resized && !this.exited) {
              // The PTY was resized, which sends SIGWINCH to the process.
              // Wait briefly so the process can redraw before we serialize,
              // otherwise the client sees a transient state (e.g., cursor
              // clamped to the new width instead of where the TUI places it).
              setTimeout(sendScreen, 50);
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

  /** Resize the PTY to match the most recently attached client.
   *  Returns true if the size actually changed. */
  private negotiateSize(): boolean {
    // Use the most recently attached/resized non-readonly client's size
    let lastClient: Client | null = null;
    for (const client of this.clients.values()) {
      if (!client.readonly && client.attachSeq > 0) {
        if (!lastClient || client.attachSeq > lastClient.attachSeq) {
          lastClient = client;
        }
      }
    }

    if (lastClient) {
      const { rows, cols } = lastClient;
      if (rows !== this.terminal.rows || cols !== this.terminal.cols) {
        this.ptyProcess.resize(cols, rows);
        this.terminal.resize(cols, rows);
        return true;
      }
    }
    return false;
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
if (process.argv[1]?.endsWith("/server.ts")) {
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
