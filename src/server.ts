import * as net from "node:net";
import * as fs from "node:fs";
import { execFileSync } from "node:child_process";
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
  cleanupAll,
  writeMetadata,
  readMetadata,
  type SessionMetadata,
} from "./sessions.ts";
import { EventWriter, clearEvents, EventType, type EventRecord } from "./events.ts";

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
  tags?: Record<string, string>;
  onExit?: (code: number) => void;
  /** When true, spawn the child with a scrubbed environment containing only
   *  a small allow-list of variables (plus any entries in `extraEnv`).
   *  Intended for contexts where the daemon may have inherited secrets that
   *  shouldn't leak into the session (e.g., a daemon launched by pty-relay
   *  for a remote client). See BUG-4. */
  isolateEnv?: boolean;
  /** Additional `KEY=VALUE` env entries to add on top of the isolation
   *  allow-list. Only consulted when `isolateEnv` is true. */
  extraEnv?: Record<string, string>;
}

/** Env variables that are safe to pass through to a session child when
 *  `isolateEnv` is on. Keeps terminal/locale/path functionality working
 *  without propagating the operator's shell secrets. */
const ISOLATED_ENV_ALLOWLIST = new Set([
  "PATH", "HOME", "USER", "LOGNAME", "SHELL",
  "TERM", "COLORTERM", "LANG", "TZ", "PWD", "TMPDIR",
  // pty-internal
  "PTY_SESSION_DIR",
]);

function buildChildEnv(options: ServerOptions): Record<string, string> {
  const source = process.env as Record<string, string>;

  if (!options.isolateEnv) {
    // Legacy behaviour: full inheritance, minus the server-config handoff.
    const env = { ...source };
    delete env.PTY_SERVER_CONFIG;
    env.PTY_SESSION = options.name;
    return env;
  }

  const env: Record<string, string> = {};
  for (const [k, v] of Object.entries(source)) {
    if (v === undefined) continue;
    if (ISOLATED_ENV_ALLOWLIST.has(k) || k.startsWith("LC_")) env[k] = v;
  }
  if (options.extraEnv) {
    for (const [k, v] of Object.entries(options.extraEnv)) env[k] = v;
  }
  env.PTY_SESSION = options.name;
  return env;
}

const LAST_LINES_COUNT = 200;

export interface ProcessResources {
  rssKb: number;       // Resident set size in KB
  cpuPercent: number;  // CPU usage percentage
}

/** Query CPU and memory usage for a process via ps. Returns null on failure. */
function queryProcessResources(pid: number): ProcessResources | null {
  try {
    const output = execFileSync("ps", ["-o", "rss=,pcpu=", "-p", String(pid)], {
      encoding: "utf-8",
      timeout: 1000,
    }).trim();
    const parts = output.split(/\s+/);
    if (parts.length < 2) return null;
    return {
      rssKb: parseInt(parts[0], 10),
      cpuPercent: parseFloat(parts[1]),
    };
  } catch {
    return null;
  }
}

/** Validate that cwd is usable for spawning a process. Returns undefined if
 *  valid, or a descriptive error string explaining what's wrong. */
function describeInvalidCwd(cwd: string): string | undefined {
  if (cwd.length === 0) return "Working directory is empty.";

  let stats: fs.Stats;
  try {
    stats = fs.statSync(cwd);
  } catch (err: any) {
    if (err?.code === "ENOENT") {
      return `Working directory does not exist: ${cwd}`;
    }
    return `Working directory is not accessible: ${cwd} (${err?.message ?? String(err)})`;
  }

  if (!stats.isDirectory()) {
    return `Working directory is not a directory: ${cwd}`;
  }

  try {
    fs.accessSync(cwd, fs.constants.X_OK);
  } catch {
    return `Working directory is not searchable: ${cwd}`;
  }

  return undefined;
}

/** Strip terminal query sequences that should not be forwarded to clients.
 *  Exported for unit testing. */
export function stripTerminalQueries(data: string): string {
  return data
    .replace(/\x1b\]1[01];\?\x07/g, "")           // OSC 10/11 with BEL
    .replace(/\x1b\]1[01];\?\x1b\\/g, "")         // OSC 10/11 with ST
    .replace(/\x1b\]4;\d+;\?\x07/g, "")           // OSC 4 with BEL
    .replace(/\x1b\]4;\d+;\?\x1b\\/g, "")         // OSC 4 with ST
    .replace(/\x1b\[c/g, "")                       // DA1
    .replace(/\x1b\[>c/g, "")                      // DA2
    .replace(/\x1b\[6n/g, "")                      // DSR cursor position
    .replace(/\x1b\[>0q/g, "");                    // XTVERSION
}

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
  // Mouse tracking modes — these are separate DEC private modes (set/cleared
  // independently by the child process) that control WHICH events the
  // terminal should report. SGR mode (1006) only controls the ENCODING of
  // reports, not whether tracking is active. Clients attaching to a session
  // that's already mid-stream need all active modes replayed so their own
  // mouse forwarding logic sees the correct state.
  private mouseTracking1000 = false; // button press/release tracking
  private mouseTracking1002 = false; // button-motion tracking
  private mouseTracking1003 = false; // any-motion tracking
  private lastResizeTime = 0;
  private eventWriter: EventWriter;
  private lastTitle = "";
  readonly ready: Promise<void>;

  constructor(options: ServerOptions) {
    this.name = options.name;
    this.options = options;
    this.eventWriter = new EventWriter(options.name);

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
          if (v === 1000) this.mouseTracking1000 = true;
          if (v === 1002) this.mouseTracking1002 = true;
          if (v === 1003) this.mouseTracking1003 = true;
          if (v === 25) {
            if (this.cursorHidden) this.emitEvent(EventType.CURSOR_VISIBLE);
            this.cursorHidden = false;
          }
          if (v === 1004) this.emitEvent(EventType.FOCUS_REQUEST);
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
          if (v === 1000) this.mouseTracking1000 = false;
          if (v === 1002) this.mouseTracking1002 = false;
          if (v === 1003) this.mouseTracking1003 = false;
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

    // Respond to DA1 (Primary Device Attribute) queries from the child process.
    // Shells like fish 4.x send ESC[c at startup and block for up to 10s waiting
    // for a response. Since xterm-headless doesn't reply, we intercept the query
    // in the output stream and write a VT220 response back to the pty process.
    this.terminal.parser.registerCsiHandler(
      { final: "c" },
      (params) => {
        if (params.length === 0 || params[0] === 0) {
          this.ptyProcess.write("\x1b[?62;22c");
        }
        return false;
      }
    );

    // ── Event detection ──

    this.terminal.onBell(() => {
      this.emitEvent(EventType.BELL);
    });

    this.terminal.onTitleChange((title: string) => {
      if (title !== this.lastTitle) {
        this.lastTitle = title;
        this.emitEvent(EventType.TITLE_CHANGE, { value: title });
      }
    });

    // iTerm2 desktop notification (OSC 9)
    this.terminal.parser.registerOscHandler(9, (data: string) => {
      this.emitEvent(EventType.NOTIFICATION, { body: data, source: "osc9" });
      return false;
    });

    // Kitty notification (OSC 99) — key=value;key=value payload
    this.terminal.parser.registerOscHandler(99, (data: string) => {
      const fields: Record<string, string> = {};
      for (const part of data.split(";")) {
        const eq = part.indexOf("=");
        if (eq !== -1) {
          fields[part.slice(0, eq)] = part.slice(eq + 1);
        }
      }
      this.emitEvent(EventType.NOTIFICATION, {
        title: fields["title"] ?? fields["t"],
        body: fields["body"] ?? fields["b"],
        source: "osc99",
      });
      return false;
    });

    // rxvt notification (OSC 777) — notify;title;body
    this.terminal.parser.registerOscHandler(777, (data: string) => {
      const parts = data.split(";");
      if (parts[0] === "notify" && parts.length >= 2) {
        this.emitEvent(EventType.NOTIFICATION, {
          title: parts[1],
          body: parts.slice(2).join(";"),
          source: "osc777",
        });
      }
      return false;
    });

    // ── Terminal query responses ──
    // Programs send queries expecting the terminal to respond on stdin.
    // xterm-headless doesn't answer, so the query leaks to the client's
    // real terminal, whose response comes back as garbage input. We
    // intercept common queries and respond directly to the PTY process.

    // OSC 10: foreground color query (less, vim)
    // Return true to consume the sequence so it doesn't leak to clients.
    this.terminal.parser.registerOscHandler(10, (data: string) => {
      if (data === "?") {
        this.ptyProcess.write("\x1b]10;rgb:c0c0/c0c0/c0c0\x1b\\");
        return true; // consume — don't pass to client
      }
      return false;
    });
    // OSC 11: background color query (less, vim)
    this.terminal.parser.registerOscHandler(11, (data: string) => {
      if (data === "?") {
        this.ptyProcess.write("\x1b]11;rgb:0000/0000/0000\x1b\\");
        return true;
      }
      return false;
    });
    // OSC 4: palette color query (vim, emacs)
    this.terminal.parser.registerOscHandler(4, (data: string) => {
      if (data.includes("?")) {
        const idx = parseInt(data, 10);
        if (!isNaN(idx)) {
          this.ptyProcess.write(`\x1b]4;${idx};rgb:0000/0000/0000\x1b\\`);
        }
        return true;
      }
      return false;
    });
    // DA2: secondary device attributes (vim, tmux)
    this.terminal.parser.registerCsiHandler(
      { prefix: ">", final: "c" },
      (_params) => {
        // Respond as xterm version 382
        this.ptyProcess.write("\x1b[>0;382;0c");
        return false;
      }
    );
    // DSR: cursor position query (CSI 6 n, vim, readline)
    this.terminal.parser.registerCsiHandler(
      { final: "n" },
      (params) => {
        if (params.length === 1 && params[0] === 6) {
          const buf = this.terminal.buffer.active;
          this.ptyProcess.write(`\x1b[${buf.cursorY + 1};${buf.cursorX + 1}R`);
        }
        return false;
      }
    );
    // XTVERSION: terminal version query (CSI > 0 q, vim)
    this.terminal.parser.registerCsiHandler(
      { prefix: ">", final: "q" },
      (_params) => {
        this.ptyProcess.write("\x1bP>|pty(0.8)\x1b\\");
        return false;
      }
    );

    // Spawn the child process in a PTY via a shell, so that shell scripts,
    // symlinks, and shebangs all work reliably (like tmux/screen do).
    // `exec "$@"` replaces the shell with the actual process.
    const childEnv = buildChildEnv(options);

    const invalidCwd = describeInvalidCwd(options.cwd);
    if (invalidCwd !== undefined) {
      throw new Error(
        `${invalidCwd}\nCannot start session "${options.name}" for command "${options.command}".`
      );
    }

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
          `Failed to spawn PTY shell "/bin/sh" for command "${options.command}" in cwd "${options.cwd}": ${msg}`
        );
      }
      throw err;
    }

    // Feed PTY output into xterm-headless and broadcast to clients.
    // Query sequences (OSC 10/11, DA1, etc.) are intercepted by parser
    // handlers above and must NOT be forwarded to clients — otherwise the
    // client's terminal responds and its response appears as garbage input.
    this.ptyProcess.onData((data: string) => {
      this.terminal.write(data);
      const cleaned = stripTerminalQueries(data);
      if (cleaned.length > 0) {
        this.broadcast(encodeData(cleaned));
      }
    });

    this.ptyProcess.onExit(({ exitCode }) => {
      this.exited = true;
      this.exitCode = exitCode;
      this.broadcast(encodeExit(exitCode));
      this.emitEvent(EventType.SESSION_EXIT, { exitCode });
      // Save exit status immediately so the session shows as "exited"
      // in pty list during the cleanup window. lastLines may be incomplete
      // here since PTY data could still be in-flight — close() will
      // update with the final output.
      this.saveExitMetadata(exitCode);
      options.onExit?.(exitCode);
    });

    // Create Unix socket server
    ensureSessionDir();
    clearEvents(this.name);
    const socketPath = getSocketPath(this.name);

    // Remove stale socket if it exists
    try {
      fs.unlinkSync(socketPath);
    } catch {}

    this.socketServer = net.createServer((socket) =>
      this.handleClient(socket)
    );
    // Tighten umask around listen() so the socket inode is never transiently
    // group/world-readable (BUG-5). The chmodSync below is kept as
    // belt-and-suspenders for good measure.
    const prevUmask = process.umask(0o077);
    this.ready = new Promise((resolve, reject) => {
      let settled = false;
      this.socketServer.once("error", (err) => {
        if (settled) return;
        settled = true;
        reject(err);
      });
      this.socketServer.listen(socketPath, () => {
        try { fs.chmodSync(socketPath, 0o600); } catch {}
        fs.writeFileSync(getPidPath(this.name), process.pid.toString());
        writeMetadata(this.name, {
          command: options.command,
          args: options.args,
          displayCommand: options.displayCommand,
          cwd: options.cwd,
          createdAt: new Date().toISOString(),
          ...(options.tags && Object.keys(options.tags).length > 0 ? { tags: options.tags } : {}),
        });
        this.emitEvent(EventType.SESSION_START, {
          ...(options.tags && Object.keys(options.tags).length > 0 ? { tags: options.tags } : {}),
        });
        if (settled) return;
        settled = true;
        resolve();
      });
    });
    process.umask(prevUmask);

    // Post-listen errors (e.g., socket file unlinked out from under us) must
    // not crash the process, but they also mustn't interfere with the
    // initial ready resolution above.
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
      let packets;
      try {
        packets = client.reader.feed(data);
      } catch (err: any) {
        // BUG-3: peer sent an oversize length header (or some other malformed
        // frame) — drop them rather than buffer unbounded.
        console.error(`Rejected client packet: ${err.message}`);
        try { socket.destroy(); } catch {}
        return;
      }
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
            const flags = packet.payload.length > 0 ? packet.payload.readUInt8(0) : 0;
            const plain = (flags & 1) !== 0;
            const full = (flags & 2) !== 0;

            if (plain) {
              socket.write(encodeScreen(full ? this.getFullPlainScreen() : this.getPlainScreen()));
            } else {
              // scrollback: 0 for viewport only, omit for full scrollback
              const serializeOpts = full ? undefined : { scrollback: 0 };
              const peekScreen = this.getModePrefix() + this.serialize.serialize(serializeOpts);
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
    if (this.mouseTracking1000) prefix += "\x1b[?1000h";
    if (this.mouseTracking1002) prefix += "\x1b[?1002h";
    if (this.mouseTracking1003) prefix += "\x1b[?1003h";
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

    const childPid = this.exited ? null : this.ptyProcess.pid;
    const daemonPid = process.pid;

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
        pid: childPid,
        resources: childPid ? queryProcessResources(childPid) : null,
      },
      daemon: {
        pid: daemonPid,
        resources: queryProcessResources(daemonPid),
      },
      clients: {
        total: attached + readOnly,
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

  private emitEvent(type: EventType, fields?: Record<string, unknown>): void {
    this.eventWriter.append({
      session: this.name,
      type,
      ts: new Date().toISOString(),
      ...fields,
    } as EventRecord);
  }

  private broadcast(data: Buffer): void {
    for (const client of this.clients.values()) {
      client.socket.write(data);
    }
  }

  private getPlainScreen(): string {
    // Viewport only: last `rows` lines (where the cursor is)
    const buffer = this.terminal.buffer.active;
    const lines: string[] = [];
    const start = Math.max(0, buffer.baseY);
    const end = buffer.length;
    for (let i = start; i < end; i++) {
      const line = buffer.getLine(i);
      if (line) lines.push(line.translateToString(true));
    }
    while (lines.length > 0 && lines[lines.length - 1] === "") lines.pop();
    return lines.join("\n");
  }

  private getFullPlainScreen(): string {
    // Full scrollback + viewport
    const buffer = this.terminal.buffer.active;
    const lines: string[] = [];
    for (let i = 0; i < buffer.length; i++) {
      const line = buffer.getLine(i);
      if (line) lines.push(line.translateToString(true));
    }
    while (lines.length > 0 && lines[lines.length - 1] === "") lines.pop();
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
      ...(existing?.tags ? { tags: existing.tags } : {}),
    });
  }

  /** Clean up resources. Does not call process.exit(). */
  close(): Promise<void> {
    // Update exit metadata with final output — by the time close() runs,
    // all PTY data has been delivered to the terminal buffer. This overwrites
    // the initial save from onExit which may have had incomplete lastLines.
    if (this.exited) {
      this.saveExitMetadata(this.exitCode);
    }

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

  const isEphemeral = config.ephemeral === true;

  function cleanShutdown(code: number): Promise<never> {
    return server.close().then(() => {
      if (isEphemeral) cleanupAll(config.name);
      process.exit(code);
    });
  }

  const server = new PtyServer({
    name: config.name,
    command: config.command,
    args: config.args ?? [],
    displayCommand: config.displayCommand,
    cwd: config.cwd ?? process.cwd(),
    rows: config.rows ?? 24,
    cols: config.cols ?? 80,
    tags: config.tags,
    isolateEnv: config.isolateEnv === true,
    extraEnv: config.extraEnv,
    onExit: (code) => {
      // Give clients a moment to receive the exit message, then shut down
      setTimeout(() => cleanShutdown(code), 500);
    },
  });

  process.on("SIGTERM", () => cleanShutdown(0));
  process.on("SIGINT", () => cleanShutdown(0));
}
