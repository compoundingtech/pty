import * as net from "node:net";
import * as tty from "node:tty";
import {
  MessageType,
  PacketReader,
  encodeAttach,
  encodeData,
  encodeDetach,
  encodePeek,
  encodeResize,
  encodeStatus,
  decodeExit,
} from "./protocol.ts";
import { getSocketPath } from "./sessions.ts";
import { stripAnsi } from "./tui/colors.ts";
import { BRACKETED_PASTE_START, BRACKETED_PASTE_END } from "./paste.ts";

const DETACH_KEY = 0x1c; // Ctrl+\ (legacy encoding)
const DETACH_KEY_KITTY = "\x1b[92;5u"; // Ctrl+\ (Kitty keyboard protocol)

/** Replace Kitty keyboard protocol encoding of Ctrl+\ with the legacy byte
 *  so the rest of the detach logic can work with a single representation. */
function normalizeDetachKey(data: Buffer): Buffer {
  const str = data.toString();
  if (!str.includes(DETACH_KEY_KITTY)) return data;
  return Buffer.from(
    str.replaceAll(DETACH_KEY_KITTY, String.fromCharCode(DETACH_KEY))
  );
}

// Reset terminal modes that programs may have enabled. This prevents
// "poisoned" terminals after detach/peek (e.g., mouse tracking, hidden
// cursor, alternate screen buffer, bracketed paste). Does NOT clear
// screen content.
export const TERMINAL_SANITIZE =
  "\x1b[?1049l" + // leave alternate screen buffer (TUI apps: vim, htop, mactop…)
  "\x1b[?1l" + // reset cursor keys to normal mode (DECCKM)
  "\x1b[?7h" + // re-enable autowrap (DECAWM)
  "\x1b[?6l" + // reset origin mode (DECOM)
  "\x1b[?1000l" + // disable mouse click tracking
  "\x1b[?1002l" + // disable mouse button-event tracking
  "\x1b[?1003l" + // disable mouse any-event tracking
  "\x1b[?1004l" + // disable focus event reporting
  "\x1b[?1006l" + // disable SGR mouse mode
  "\x1b[?25h" + // show cursor
  "\x1b[?2004l" + // disable bracketed paste
  "\x1b[4l" + // reset insert mode (IRM) to replace
  "\x1b[r" + // reset scroll region (DECSTBM) to full terminal
  "\x1b[0m" + // reset SGR attributes (colors, bold, etc.)
  "\x1b[0 q" + // reset cursor style to terminal default
  "\x1b>" + // reset application keypad mode (DECKPNM)
  "\x1b(B" + // reset G0 character set to ASCII
  "\x1b[<99u"; // pop all Kitty keyboard protocol levels

// Move cursor to bottom of visible screen so status messages (e.g.
// "[detached]") appear below the session content, not mid-screen.
const CURSOR_TO_BOTTOM = "\x1b[999;1H";

export interface PeekOptions {
  name: string;
  follow?: boolean; // If true, stay connected and stream (like tail -f). If false, print screen and exit.
  plain?: boolean; // If true, output plain text without ANSI codes.
  full?: boolean; // If true, include full scrollback, not just the viewport.
  onExit?: (code: number) => void;
  onDetach?: () => void;
  /** Speak the peek protocol over this ALREADY-CONNECTED socket instead of
   *  dialing the local `<name>.sock`. Used by `peek --remote`: a fabric-dialed,
   *  control-server-routed socket that transparently pipes to the remote
   *  session's daemon. When set, `name` is only used for display. */
  socket?: net.Socket;
}

/** Read-only view of a session. Input is ignored by the server. */
export function peek(options: PeekOptions): void {
  const reader = new PacketReader();
  const socket = options.socket ?? net.createConnection(getSocketPath(options.name));
  const stdout = process.stdout;
  const follow = options.follow ?? false;

  const onReady = () => {
    socket.write(encodePeek(options.plain, options.full));

    if (follow) {
      // In follow mode, Ctrl+\ detaches
      const stdin = process.stdin;
      if (stdin.isTTY) stdin.setRawMode(true);

      stdin.on("data", (raw: Buffer) => {
        const data = normalizeDetachKey(raw);
        for (let i = 0; i < data.length; i++) {
          if (data[i] === DETACH_KEY) {
            if (stdin.isTTY) stdin.setRawMode(false);
            socket.destroy();
            stdout.write(TERMINAL_SANITIZE + CURSOR_TO_BOTTOM + "\r\n[detached]\r\n");
            options.onDetach?.();
            return;
          }
        }
        // All other input is silently ignored (read-only)
      });
      stdin.resume();
    }
  };

  // A caller-supplied socket is already connected (dialed + routed over fabric),
  // so there's no "connect" event to wait for — kick off on the next tick.
  if (options.socket) process.nextTick(onReady);
  else socket.on("connect", onReady);

  // Track whether we ever received a screen. If the connection closes before
  // any screen arrives in one-shot mode, the session isn't serving us (e.g. a
  // `--remote` route to a name that doesn't exist on the peer, where the control
  // server closes the tunnel) — surface that instead of exiting 0 silently.
  let gotScreen = false;

  socket.on("data", (data: Buffer) => {
    let packets;
    try { packets = reader.feed(data); } catch (err: any) {
      console.error(`pty client: dropping connection — ${err.message}`);
      try { socket.destroy(); } catch {}
      return;
    }
    for (const packet of packets) {
      switch (packet.type) {
        case MessageType.SCREEN:
          gotScreen = true;
          stdout.write(packet.payload);
          if (!follow) {
            if (!options.plain) {
              stdout.write(TERMINAL_SANITIZE + CURSOR_TO_BOTTOM);
            }
            stdout.write("\n");
            socket.destroy();
            return;
          }
          break;

        case MessageType.DATA:
          if (follow) {
            stdout.write(options.plain ? stripAnsi(packet.payload.toString()) : packet.payload);
          }
          break;

        case MessageType.EXIT: {
          const code = decodeExit(packet.payload);
          socket.destroy();
          if (!options.plain) {
            stdout.write(TERMINAL_SANITIZE + CURSOR_TO_BOTTOM);
          }
          if (follow) {
            stdout.write(`\r\n[${options.name} exited with code ${code}]\r\n`);
          }
          options.onExit?.(code);
          return;
        }
      }
    }
  });

  socket.on("error", (err: NodeJS.ErrnoException) => {
    // ECONNRESET/EPIPE also mean "gone": a `--remote` route to a missing session
    // has the control server close the tunnel as we write the first frame.
    const notReachable = err.code === "ENOENT" || err.code === "ECONNREFUSED"
      || err.code === "ECONNRESET" || err.code === "EPIPE";
    if (notReachable) {
      console.error(
        options.socket
          ? `Remote session "${options.name}" not found or not running.`
          : `Session "${options.name}" not found or not running.`,
      );
    } else {
      console.error(`Connection error: ${err.message}`);
    }
    process.exit(1);
  });

  socket.on("close", () => {
    if (process.stdin.isTTY && process.stdin.isRaw) {
      process.stdin.setRawMode(false);
    }
    // Closed in one-shot mode before any screen — the (possibly remote) session
    // isn't reachable. Don't exit 0 with no output.
    if (!follow && !gotScreen) {
      console.error(
        options.socket
          ? `Remote session "${options.name}" not found or not running.`
          : `Session "${options.name}" not found or not running.`,
      );
      process.exit(1);
    }
  });
}

export interface SendOptions {
  name: string;
  data: string[];
  delayMs?: number;
  /** Wrap the entire payload (all `data` entries taken together) in
   *  bracketed-paste markers (CSI 200 ~ … CSI 201 ~). The receiving TUI
   *  treats everything between the markers as one paste event rather
   *  than a sequence of keystrokes — useful for injecting multi-line
   *  prompts into agent sessions without premature submission. Receiver
   *  must have bracketed paste enabled (DECSET 2004); most modern
   *  shells and TUIs do by default. */
  paste?: boolean;
  /** Speak the send protocol over this ALREADY-CONNECTED socket instead of
   *  dialing the local `<name>.sock`. Used by `send --remote`: a fabric-dialed,
   *  control-server-routed socket. When set, `name` is only used for display. */
  socket?: net.Socket;
}

/** Default spacing (ms) the `pty send` CLI inserts between `--seq` items when
 *  the caller doesn't pass `--with-delay`. A burst of bytes and spaced-out
 *  input are processed differently by terminal programs — a trailing `key:return`
 *  fired with zero delay routinely lands before the program has parsed/rendered
 *  the typed text, submitting an empty or partial line. 0.3s lets each chunk be
 *  consumed. See docs/SKILL.md. This default lives in the CLI layer only; the
 *  library `send()` still treats `delayMs` literally (undefined/0 = no spacing). */
export const DEFAULT_SEQ_DELAY_MS = 300;

/** Resolve the `pty send` inter-item delay in ms from the `--with-delay <sec>`
 *  argument: absent → the 0.3s default; an explicit value (including 0, the
 *  straight-stream escape hatch) → that value. Pure; exported for testing. */
export function resolveSeqDelayMs(withDelaySecs: number | undefined): number {
  return withDelaySecs != null ? Math.round(withDelaySecs * 1000) : DEFAULT_SEQ_DELAY_MS;
}

/** Send data to a session without attaching. Silent on success. */
export function send(options: SendOptions): void {
  const socket = options.socket ?? net.createConnection(getSocketPath(options.name));

  const onReady = async () => {
    if (options.paste && options.data.length > 0) {
      socket.write(encodeData(BRACKETED_PASTE_START));
    }
    for (let i = 0; i < options.data.length; i++) {
      if (i > 0 && options.delayMs) {
        await new Promise((resolve) => setTimeout(resolve, options.delayMs));
      }
      socket.write(encodeData(options.data[i]));
    }
    if (options.paste && options.data.length > 0) {
      socket.write(encodeData(BRACKETED_PASTE_END));
    }
    socket.end();
  };

  // A caller-supplied socket is already connected (dialed + routed over fabric).
  if (options.socket) process.nextTick(onReady);
  else socket.on("connect", onReady);

  let finished = false;

  socket.on("error", (err: NodeJS.ErrnoException) => {
    const notReachable = err.code === "ENOENT" || err.code === "ECONNREFUSED"
      || err.code === "ECONNRESET" || err.code === "EPIPE";
    if (notReachable) {
      console.error(
        options.socket
          ? `Remote session "${options.name}" not found or not running.`
          : `Session "${options.name}" not found or not running.`,
      );
    } else {
      console.error(`Connection error: ${err.message}`);
    }
    process.exit(1);
  });

  socket.on("finish", () => {
    finished = true;
    process.exit(0);
  });

  // Closed before our write finished — the (possibly remote) session isn't
  // reachable (e.g. a `--remote` route to a missing session). Don't exit 0.
  socket.on("close", () => {
    if (!finished) {
      console.error(
        options.socket
          ? `Remote session "${options.name}" not found or not running.`
          : `Session "${options.name}" not found or not running.`,
      );
      process.exit(1);
    }
  });
}

export interface ProcessResources {
  rssKb: number;
  cpuPercent: number;
}

export interface StatsResult {
  name: string;
  terminal: {
    cols: number;
    rows: number;
    cursorX: number;
    cursorY: number;
    scrollbackUsed: number;
    scrollbackCapacity: number;
  };
  process: {
    alive: boolean;
    exitCode: number | null;
    pid: number | null;
    resources: ProcessResources | null;
  };
  daemon: {
    pid: number;
    resources: ProcessResources | null;
  };
  clients: { total: number; attached: number; readOnly: number };
  modes: {
    sgrMouse: boolean;
    cursorHidden: boolean;
    kittyKeyboard: boolean;
    kittyKeyboardFlags: number[];
  };
  uptimeSeconds: number | null;
  createdAt: string | null;
}

/** Query live stats from a running session. */
export function queryStats(name: string, timeoutMs = 2000): Promise<StatsResult> {
  return new Promise((resolve, reject) => {
    const socketPath = getSocketPath(name);
    const reader = new PacketReader();
    const socket = net.createConnection(socketPath);

    const timer = setTimeout(() => {
      socket.destroy();
      reject(new Error(`Timeout querying stats for "${name}"`));
    }, timeoutMs);

    socket.on("connect", () => {
      socket.write(encodeStatus());
    });

    socket.on("data", (data: Buffer) => {
      let packets;
    try { packets = reader.feed(data); } catch (err: any) {
      console.error(`pty client: dropping connection — ${err.message}`);
      try { socket.destroy(); } catch {}
      return;
    }
      for (const packet of packets) {
        if (packet.type === MessageType.STATUS) {
          clearTimeout(timer);
          socket.destroy();
          try {
            resolve(JSON.parse(packet.payload.toString()));
          } catch {
            reject(new Error(`Invalid stats response from "${name}"`));
          }
          return;
        }
      }
    });

    socket.on("error", (err: NodeJS.ErrnoException) => {
      clearTimeout(timer);
      if (err.code === "ENOENT" || err.code === "ECONNREFUSED") {
        reject(new Error(`Session "${name}" not found or not running.`));
      } else {
        reject(new Error(`Connection error: ${err.message}`));
      }
    });
  });
}

export interface AttachOptions {
  name: string;
  onExit?: (code: number) => void;
  onDetach?: () => void;
}

export function attach(options: AttachOptions): void {
  const socketPath = getSocketPath(options.name);
  const reader = new PacketReader();
  const socket = net.createConnection(socketPath);

  const stdin = process.stdin;
  const stdout = process.stdout;

  let detaching = false;
  let rawWasSet = false;
  let exitCode = 0;
  let stdinDataHandler: ((data: Buffer) => void) | null = null;
  let resizeHandler: (() => void) | null = null;

  function enterRawMode(): void {
    if (stdin.isTTY && !stdin.isRaw) {
      stdin.setRawMode(true);
      rawWasSet = true;
    }
  }

  function exitRawMode(): void {
    if (rawWasSet && stdin.isTTY) {
      stdin.setRawMode(false);
    }
  }

  function cleanExit(): void {
    if (stdinDataHandler) {
      stdin.removeListener("data", stdinDataHandler);
      stdinDataHandler = null;
    }
    if (resizeHandler && stdout instanceof tty.WriteStream) {
      stdout.removeListener("resize", resizeHandler);
      resizeHandler = null;
    }
    exitRawMode();
    socket.destroy();
  }

  socket.on("connect", () => {
    enterRawMode();

    // Tell the server our terminal size
    const rows = (stdout as tty.WriteStream).rows ?? 24;
    const cols = (stdout as tty.WriteStream).columns ?? 80;
    socket.write(encodeAttach(rows, cols));

    // Forward stdin to server
    // Double Ctrl+\ passthrough: press once = detach, press twice quickly = send Ctrl+\ to process
    let lastDetachKeyTime = 0;
    const DOUBLE_TAP_MS = 300;

    stdinDataHandler = (raw: Buffer) => {
      const data = normalizeDetachKey(raw);

      // Fast path: no detach key in this chunk
      if (data.indexOf(DETACH_KEY) === -1) {
        socket.write(encodeData(data.toString()));
        return;
      }

      // Slow path: detach key found — process byte by byte
      const forward: number[] = [];

      for (let i = 0; i < data.length; i++) {
        if (data[i] === DETACH_KEY) {
          const now = Date.now();
          if (now - lastDetachKeyTime < DOUBLE_TAP_MS) {
            // Double-tap: send Ctrl+\ to the process, reset timer
            lastDetachKeyTime = 0;
            forward.push(DETACH_KEY);
          } else {
            // First tap: schedule detach (will fire if no second tap)
            lastDetachKeyTime = now;
            setTimeout(() => {
              if (lastDetachKeyTime === now) {
                detaching = true;
                socket.write(encodeDetach());
                cleanExit();
                stdout.write(TERMINAL_SANITIZE + CURSOR_TO_BOTTOM + "\r\n[detached]\r\n");
                options.onDetach?.();
              }
            }, DOUBLE_TAP_MS);
          }
        } else {
          forward.push(data[i]);
        }
      }

      if (forward.length > 0) {
        socket.write(encodeData(Buffer.from(forward).toString()));
      }
    };
    stdin.on("data", stdinDataHandler);

    // Explicitly resume stdin. We cannot rely on the auto-resume from
    // .on("data") because Node.js skips it when _readableState.flowing
    // is exactly `false` (as opposed to the initial `null`). This state
    // can be left behind by readline (restart prompt) or other code that
    // previously consumed stdin.
    stdin.resume();

    // Handle terminal resize
    if (stdout instanceof tty.WriteStream) {
      resizeHandler = () => {
        const rows = stdout.rows;
        const cols = stdout.columns;
        socket.write(encodeResize(rows, cols));
      };
      stdout.on("resize", resizeHandler);
    }
  });

  socket.on("data", (data: Buffer) => {
    let packets;
    try { packets = reader.feed(data); } catch (err: any) {
      console.error(`pty client: dropping connection — ${err.message}`);
      try { socket.destroy(); } catch {}
      return;
    }
    for (const packet of packets) {
      switch (packet.type) {
        case MessageType.DATA:
          stdout.write(packet.payload);
          break;

        case MessageType.SCREEN:
          // Clear screen and write the replayed buffer
          stdout.write("\x1b[2J\x1b[H");
          stdout.write(packet.payload);
          break;

        case MessageType.EXIT:
          exitCode = decodeExit(packet.payload);
          exitHandled = true;
          cleanExit();
          stdout.write(TERMINAL_SANITIZE + CURSOR_TO_BOTTOM + `\r\n[${options.name} exited with code ${exitCode}]\r\n`);
          options.onExit?.(exitCode);
          return;
      }
    }
  });

  let exitHandled = false;

  socket.on("error", (err: NodeJS.ErrnoException) => {
    if (exitHandled) return;
    exitHandled = true;
    cleanExit();
    if (err.code === "ENOENT" || err.code === "ECONNREFUSED") {
      console.error(`Session "${options.name}" not found or not running.`);
    } else {
      console.error(`Connection error: ${err.message}`);
    }
    if (options.onExit) {
      options.onExit(1);
    } else {
      process.exit(1);
    }
  });

  socket.on("close", () => {
    if (!detaching && !exitHandled) {
      exitHandled = true;
      cleanExit();
      if (options.onExit) {
        options.onExit(exitCode);
      } else {
        process.exit(exitCode);
      }
    }
  });
}
