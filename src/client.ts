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
  onExit?: (code: number) => void;
  onDetach?: () => void;
}

/** Read-only view of a session. Input is ignored by the server. */
export function peek(options: PeekOptions): void {
  const socketPath = getSocketPath(options.name);
  const reader = new PacketReader();
  const socket = net.createConnection(socketPath);
  const stdout = process.stdout;
  const follow = options.follow ?? false;

  socket.on("connect", () => {
    socket.write(encodePeek(options.plain));

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
  });

  socket.on("data", (data: Buffer) => {
    const packets = reader.feed(data);
    for (const packet of packets) {
      switch (packet.type) {
        case MessageType.SCREEN:
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
            stdout.write(`\r\n[session exited with code ${code}]\r\n`);
          }
          options.onExit?.(code);
          return;
        }
      }
    }
  });

  socket.on("error", (err: NodeJS.ErrnoException) => {
    if (err.code === "ENOENT" || err.code === "ECONNREFUSED") {
      console.error(`Session "${options.name}" not found or not running.`);
    } else {
      console.error(`Connection error: ${err.message}`);
    }
    process.exit(1);
  });

  socket.on("close", () => {
    if (process.stdin.isTTY && process.stdin.isRaw) {
      process.stdin.setRawMode(false);
    }
  });
}

export interface SendOptions {
  name: string;
  data: string[];
  delayMs?: number;
}

/** Send data to a session without attaching. Silent on success. */
export function send(options: SendOptions): void {
  const socketPath = getSocketPath(options.name);
  const socket = net.createConnection(socketPath);

  socket.on("connect", async () => {
    for (let i = 0; i < options.data.length; i++) {
      if (i > 0 && options.delayMs) {
        await new Promise((resolve) => setTimeout(resolve, options.delayMs));
      }
      socket.write(encodeData(options.data[i]));
    }
    socket.end();
  });

  socket.on("error", (err: NodeJS.ErrnoException) => {
    if (err.code === "ENOENT" || err.code === "ECONNREFUSED") {
      console.error(`Session "${options.name}" not found or not running.`);
    } else {
      console.error(`Connection error: ${err.message}`);
    }
    process.exit(1);
  });

  socket.on("close", () => {
    process.exit(0);
  });
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
  process: { alive: boolean; exitCode: number | null };
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
      const packets = reader.feed(data);
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
    const packets = reader.feed(data);
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
          stdout.write(TERMINAL_SANITIZE + CURSOR_TO_BOTTOM + `\r\n[session exited with code ${exitCode}]\r\n`);
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
