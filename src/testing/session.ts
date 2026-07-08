import * as net from "node:net";
import type { Terminal } from "@xterm/headless";
import type { SerializeAddon } from "@xterm/addon-serialize";
import xterm from "@xterm/headless";
import * as xtermSerialize from "@xterm/addon-serialize";
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

/**
 * Build the environment for a spawned `pty` process: the caller's base env
 * merged with `optsEnv`, minus the harness's own pty-internal context.
 *
 * Two hazards this guards against when the test harness ITSELF runs inside a
 * pty session:
 *   - PTY_SESSION / PTY_SERVER_CONFIG leaking in would trip the spawned CLI's
 *     nesting-prevention guard (or hand it a bogus server config). Always
 *     scrubbed.
 *   - PTY_ROOT / PTY_SESSION_DIR leaking in would override the caller's
 *     intended isolation, because getSessionDir() prefers ambient PTY_ROOT over
 *     the per-call PTY_SESSION_DIR — so the spawned `pty` would read the real
 *     live session dir. Scrubbed ONLY when the caller didn't set them
 *     explicitly via `optsEnv`, so a deliberate root override still wins.
 *
 * Pure and exported for unit testing.
 */
export function buildSpawnEnv(
  base: Record<string, string | undefined>,
  optsEnv?: Record<string, string>,
): Record<string, string> {
  const env: Record<string, string> = {
    ...(base as Record<string, string>),
    ...optsEnv,
  };
  delete env.PTY_SERVER_CONFIG;
  delete env.PTY_SESSION;
  if (optsEnv?.PTY_ROOT === undefined) delete env.PTY_ROOT;
  if (optsEnv?.PTY_SESSION_DIR === undefined) delete env.PTY_SESSION_DIR;
  return env;
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

  /**
   * Spawn a process in a direct PTY. Use this for testing CLI tools, TUI apps,
   * or any process where you send input and check screen output.
   *
   * ```typescript
   * const session = Session.spawn("node", ["--experimental-strip-types", "my-app.ts"], { rows: 30, cols: 100 });
   * await session.waitForText("Ready");
   * session.press("ctrl+c");
   * await session.close();
   * ```
   */
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
      scrollback: 10000,
      allowProposedApi: true,
    });
    const serialize = new xtermSerialize.SerializeAddon();
    terminal.loadAddon(serialize);

    const env = buildSpawnEnv(process.env as Record<string, string | undefined>, opts.env);

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

  /**
   * Create a persistent session backed by a PtyServer. Use this when testing
   * detach/reattach behavior, multiple clients, or resize. Call `attach()`
   * after creation to start receiving output.
   */
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
      scrollback: 10000,
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

  /**
   * Create a second client connected to the same server as an existing session.
   * Use this to test multi-client scenarios (e.g., two terminals attached to
   * the same process).
   */
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
      scrollback: 10000,
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

  /** Current terminal height in rows. */
  get rows(): number {
    return this._rows;
  }

  /** Current terminal width in columns. */
  get cols(): number {
    return this._cols;
  }

  /** Whether the process has exited. Server-mode only; always false for spawn-mode. */
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

  /** Send raw keystrokes to the process. Use for literal text or escape sequences. */
  sendKeys(keys: string): void {
    if (this.backend.kind === "spawn") {
      this.backend.ptyProcess.write(keys);
    } else {
      this.backend.socket.write(encodeData(keys));
    }
  }

  /**
   * Send a named key. Supports modifiers: `"ctrl+c"`, `"alt+x"`, `"shift+a"`.
   * See docs/testing.md for the full list of key names.
   */
  press(keyName: string): void {
    this.sendKeys(resolveKey(keyName));
  }

  /** Send text to the process. Alias for `sendKeys()`. */
  type(text: string): void {
    this.sendKeys(text);
  }

  // ── Screen ──

  /** Capture the current terminal state. Returns plain text lines, joined text, and ANSI output. */
  screenshot(): Screenshot {
    return captureScreenshot(this.terminal, this.serialize);
  }

  // ── Waiting ──

  /** Poll until the terminal contains the given text. Returns the matching screenshot. */
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

  /** Poll until the terminal no longer contains the given text. */
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

  /** Poll until a custom predicate returns true. The `description` is used in timeout error messages. */
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

  /** Start receiving output from the server. Required after `Session.server()`. Server-mode only. */
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

  /** Simulate a detach + reattach cycle. Destroys the socket, resets the terminal, and reconnects. Server-mode only. */
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

  /** Resize the terminal. Server-mode only. */
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

  /** Clean up the session. Kills the process (spawn) or destroys the socket and server (server). Always call this in `afterEach`. */
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
      let packets;
      try { packets = backend.reader.feed(data); } catch {
        try { backend.socket.destroy(); } catch {}
        return;
      }
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
