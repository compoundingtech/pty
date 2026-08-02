import * as net from "node:net";
import * as tty from "node:tty";
import { PassThrough, type Readable, type Writable } from "node:stream";
import {
  MessageType,
  PacketReader,
  encodeData,
  encodePeek,
  encodeStatus,
  decodeExit,
} from "./protocol.ts";
import { getSocketPath, readMetadata } from "./sessions.ts";
import { stripAnsi } from "./tui/colors.ts";
import { BRACKETED_PASTE_START, BRACKETED_PASTE_END } from "./paste.ts";
import { machineAttachV2 } from "./machine-attach.ts";
import {
  MACHINE_PROTOCOL_VERSION,
  MachineFrameReader,
  decodeMachineResponse,
  encodeMachineRequest,
  type MachineInputModeSnapshotV1,
  type MachineResponse,
} from "./machine-protocol.ts";

const DETACH_KEY = 0x1c; // Ctrl+\ (legacy encoding)
const DETACH_KEY_KITTY = Buffer.from("\x1b[92;5u");
const DETACH_KEY_MODIFY_OTHER_KEYS = Buffer.from("\x1b[27;5;92~");

interface InteractiveInputPolicyOptions {
  readonly onInput: (bytes: Buffer) => void;
  readonly onDetach: () => void;
  readonly onError?: (error: Error) => void;
  readonly ambiguityMs?: number;
  readonly doubleTapMs?: number | null;
}

/** Stateful terminal-only policy layered above the framed machine transport.
 * It reserves local detach without interpreting or re-encoding other bytes. */
export class InteractiveInputPolicy {
  private readonly onInput: (bytes: Buffer) => void;
  private readonly onDetach: () => void;
  private readonly onError: (error: Error) => void;
  private readonly ambiguityMs: number;
  private readonly doubleTapMs: number | null;
  private encodedDetachKeys: readonly Buffer[] = [];
  private encodedPrefix = Buffer.alloc(0);
  private utf8Prefix = Buffer.alloc(0);
  private encodedPrefixTimer: NodeJS.Timeout | null = null;
  private detachTimer: NodeJS.Timeout | null = null;
  private stopped = false;

  constructor(options: InteractiveInputPolicyOptions) {
    this.onInput = options.onInput;
    this.onDetach = options.onDetach;
    this.onError = options.onError ?? (() => {});
    this.ambiguityMs = options.ambiguityMs ?? 25;
    this.doubleTapMs = options.doubleTapMs === undefined ? 300 : options.doubleTapMs;
  }

  setInputModes(modes: MachineInputModeSnapshotV1): void {
    const next = [
      ...(modes.kittyKeyboardFlagsStack.length > 0 ? [DETACH_KEY_KITTY] : []),
      ...(modes.modifyOtherKeys === 2 ? [DETACH_KEY_MODIFY_OTHER_KEYS] : []),
    ];
    const unchanged = next.length === this.encodedDetachKeys.length &&
      next.every((key, index) => key.equals(this.encodedDetachKeys[index]));
    if (!unchanged) this.flushEncodedPrefix();
    this.encodedDetachKeys = next;
  }

  feed(chunk: Buffer): void {
    if (this.stopped || chunk.length === 0) return;
    if (this.encodedPrefixTimer) {
      clearTimeout(this.encodedPrefixTimer);
      this.encodedPrefixTimer = null;
    }
    const bytes = this.encodedPrefix.length === 0
      ? chunk
      : Buffer.concat([this.encodedPrefix, chunk]);
    this.encodedPrefix = Buffer.alloc(0);
    const forward: Buffer[] = [];
    let literalStart = 0;
    const flushLiteral = (end: number) => {
      if (end > literalStart) forward.push(bytes.subarray(literalStart, end));
    };
    const emitForward = () => {
      if (forward.length === 0) return;
      this.emitUtf8(Buffer.concat(forward));
      forward.length = 0;
    };
    for (let index = 0; index < bytes.length;) {
      if (bytes[index] === DETACH_KEY) {
        flushLiteral(index);
        emitForward();
        this.handleDetachKey();
        index++;
        literalStart = index;
        continue;
      }
      if (this.encodedDetachKeys.length > 0 && bytes[index] === 0x1b) {
        const remaining = bytes.subarray(index);
        const candidates = this.encodedDetachKeys.filter((key) => {
          const compared = Math.min(remaining.length, key.length);
          return remaining.subarray(0, compared).equals(key.subarray(0, compared));
        });
        if (candidates.length > 0) {
          flushLiteral(index);
          const complete = candidates.find((key) => remaining.length >= key.length);
          if (!complete) {
            this.encodedPrefix = Buffer.from(remaining);
            literalStart = bytes.length;
            this.encodedPrefixTimer = setTimeout(() => this.flushEncodedPrefix(), this.ambiguityMs);
            break;
          }
          emitForward();
          this.handleDetachKey();
          index += complete.length;
          literalStart = index;
          continue;
        }
      }
      index++;
    }
    flushLiteral(bytes.length);
    emitForward();
  }

  end(): void {
    if (this.stopped) return;
    this.flushEncodedPrefix();
    if (this.utf8Prefix.length > 0) {
      this.fail(new Error("stdin ended with an incomplete UTF-8 sequence"));
    }
  }

  discardPendingInput(): void {
    if (this.encodedPrefixTimer) clearTimeout(this.encodedPrefixTimer);
    this.encodedPrefixTimer = null;
    this.encodedPrefix = Buffer.alloc(0);
    this.utf8Prefix = Buffer.alloc(0);
  }

  dispose(): void {
    this.stopped = true;
    if (this.encodedPrefixTimer) clearTimeout(this.encodedPrefixTimer);
    if (this.detachTimer) clearTimeout(this.detachTimer);
    this.encodedPrefixTimer = null;
    this.detachTimer = null;
    this.encodedPrefix = Buffer.alloc(0);
    this.utf8Prefix = Buffer.alloc(0);
  }

  private handleDetachKey(): void {
    if (this.doubleTapMs === null) {
      this.stopped = true;
      this.onDetach();
      return;
    }
    if (this.detachTimer) {
      clearTimeout(this.detachTimer);
      this.detachTimer = null;
      this.emitUtf8(Buffer.from([DETACH_KEY]));
      return;
    }
    this.detachTimer = setTimeout(() => {
      this.detachTimer = null;
      this.stopped = true;
      this.onDetach();
    }, this.doubleTapMs);
  }

  private flushEncodedPrefix(): void {
    if (this.encodedPrefixTimer) clearTimeout(this.encodedPrefixTimer);
    this.encodedPrefixTimer = null;
    if (this.encodedPrefix.length === 0) return;
    const bytes = this.encodedPrefix;
    this.encodedPrefix = Buffer.alloc(0);
    this.emitUtf8(bytes);
  }

  private emitUtf8(chunk: Buffer): void {
    if (this.stopped || chunk.length === 0) return;
    const bytes = this.utf8Prefix.length === 0
      ? chunk
      : Buffer.concat([this.utf8Prefix, chunk]);
    let complete: number;
    try {
      complete = completeUtf8Prefix(bytes);
    } catch (error) {
      this.fail(error instanceof Error ? error : new Error(String(error)));
      return;
    }
    if (complete > 0) this.onInput(Buffer.from(bytes.subarray(0, complete)));
    this.utf8Prefix = Buffer.from(bytes.subarray(complete));
  }

  private fail(error: Error): void {
    this.dispose();
    this.onError(error);
  }
}

function completeUtf8Prefix(bytes: Buffer): number {
  let index = 0;
  const continuation = (offset: number, min = 0x80, max = 0xbf): boolean =>
    offset < bytes.length && bytes[offset] >= min && bytes[offset] <= max;
  while (index < bytes.length) {
    const first = bytes[index];
    if (first <= 0x7f) { index++; continue; }
    let width: number;
    let secondMin = 0x80;
    let secondMax = 0xbf;
    if (first >= 0xc2 && first <= 0xdf) width = 2;
    else if (first >= 0xe0 && first <= 0xef) {
      width = 3;
      if (first === 0xe0) secondMin = 0xa0;
      if (first === 0xed) secondMax = 0x9f;
    } else if (first >= 0xf0 && first <= 0xf4) {
      width = 4;
      if (first === 0xf0) secondMin = 0x90;
      if (first === 0xf4) secondMax = 0x8f;
    } else throw new Error("stdin contained invalid UTF-8");
    if (index + width > bytes.length) return index;
    if (!continuation(index + 1, secondMin, secondMax)) throw new Error("stdin contained invalid UTF-8");
    for (let offset = 2; offset < width; offset++) {
      if (!continuation(index + offset)) throw new Error("stdin contained invalid UTF-8");
    }
    index += width;
  }
  return index;
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
  let detachPolicy: InteractiveInputPolicy | null = null;
  let followInput: ((raw: Buffer) => void) | null = null;

  const onReady = () => {
    socket.write(encodePeek(options.plain, options.full));

    if (follow) {
      // In follow mode, Ctrl+\ detaches
      const stdin = process.stdin;
      if (stdin.isTTY) stdin.setRawMode(true);

      detachPolicy = new InteractiveInputPolicy({
        onInput: () => {},
        onDetach: () => {
          if (stdin.isTTY) stdin.setRawMode(false);
          socket.destroy();
          stdout.write(TERMINAL_SANITIZE + CURSOR_TO_BOTTOM + "\r\n[detached]\r\n");
          options.onDetach?.();
        },
        doubleTapMs: null,
      });
      detachPolicy.setInputModes({
        schema: "pty.input-mode.v1",
        wireEncoder: "xterm-input.v1",
        revision: 0,
        applicationCursorKeys: false,
        applicationKeypad: false,
        bracketedPaste: false,
        focusReporting: false,
        modifyOtherKeys: 0,
        mouseTracking: "Off",
        mouseEncoding: "X10",
        mouseCoordinates: "Cell",
        kittyKeyboardFlagsStack: [0],
      });
      followInput = (raw: Buffer) => detachPolicy?.feed(Buffer.from(raw));
      stdin.on("data", followInput);
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
    detachPolicy?.dispose();
    if (followInput) process.stdin.removeListener("data", followInput);
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
 *  consumed. See SKILL.md. This default lives in the CLI layer only; the
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
  generation: string;
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
  clients: {
    total: number;
    attached: number;
    readOnly: number;
    connections?: Array<
      | {
          role: "writable";
          rows: number;
          cols: number;
          lastRequestSequence: number;
          constrains: { rows: boolean; cols: boolean };
        }
      | {
          role: "readonly";
          constrains: { rows: false; cols: false };
        }
    >;
  };
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
  /** Exact daemon incarnation. Local attaches read it from metadata; routed
   *  attaches receive it from the route admission response. */
  expectedGeneration?: string | null;
  onExit?: (code: number) => void;
  onDetach?: () => void;
  /** Injectable terminal streams keep the interactive adapter usable as a
   *  library component. Production callers use the process terminal. */
  input?: Readable & Partial<Pick<tty.ReadStream, "isTTY" | "isRaw" | "setRawMode">>;
  output?: Writable & Partial<Pick<tty.WriteStream, "rows" | "columns">>;
  /** Attach over this ALREADY-CONNECTED socket instead of dialing the local
   *  `<name>.sock`. Used by `attach --remote`: a fabric-dialed, control-server-
   *  routed socket. When set, `name` is only used for display. */
  socket?: net.Socket;
  /** Re-establish the (routed) socket after a loud disconnect — e.g. for
   *  `attach --remote`. Contract:
   *   - RESOLVE a socket → reconnected; re-attach over it.
   *   - RESOLVE null → transient/transport failure (host unreachable) → keep
   *     retrying with backoff (unlimited by default — a roaming laptop can
   *     reopen hours later).
   *   - REJECT → clean give-up: the host is reachable but the session is gone.
   *  A recoverable stall keeps the socket open (no close event), so reconnect
   *  fires only on a genuine close (fabric's loud give-up), never on a stall. */
  reconnect?: (signal: AbortSignal) => Promise<net.Socket | null>;
}

/** Backoff schedule for `attach --remote` reconnect attempts, then a cap. */
const RECONNECT_BACKOFF_MS = [100, 250, 500, 1000, 2000, 5000, 10000];
const RECONNECT_BACKOFF_CAP_MS = 15000;
/** Consecutive transport-failure attempts before giving up. Default: UNLIMITED
 *  while the terminal is open — the faithful roaming behavior (close the laptop,
 *  travel, reopen, and it comes back; the user stops it with Ctrl-\).
 *  A reachable-but-gone session gives up cleanly regardless (a rejected reconnect,
 *  see AttachOptions.reconnect). Env-overridable for a finite bound in scripts. */
const RECONNECT_MAX_ATTEMPTS = (() => {
  const raw = Number(process.env.PTY_RECONNECT_MAX_ATTEMPTS);
  return Number.isInteger(raw) && raw > 0 ? raw : Infinity;
})();

export function attach(options: AttachOptions): void {
  const stdin = options.input ?? process.stdin;
  const stdout = options.output ?? process.stdout;
  const canReconnect = !!options.reconnect;
  const expectedGeneration = options.socket
    ? options.expectedGeneration
    : options.expectedGeneration ?? readMetadata(options.name)?.generation;
  if (!expectedGeneration) {
    try { options.socket?.destroy(); } catch {}
    console.error(`Session "${options.name}" has no daemon generation; restart it before attaching.`);
    if (options.onExit) options.onExit(1); else process.exit(1);
    return;
  }
  const generation = expectedGeneration;

  type Size = { readonly rows: number; readonly cols: number };
  type AttachPhase =
    | { readonly _tag: "Opening"; readonly controller: AbortController; readonly requests: PassThrough; readonly sentSize: Size }
    | { readonly _tag: "Ready"; readonly controller: AbortController; readonly requests: PassThrough; readonly sentSize: Size }
    | { readonly _tag: "Reconnecting"; readonly controller: AbortController }
    | { readonly _tag: "Detaching"; readonly controller: AbortController | null; readonly requests: PassThrough | null }
    | { readonly _tag: "Ended" };

  const terminalSize = (): Size => ({
    rows: (stdout as tty.WriteStream).rows ?? 24,
    cols: (stdout as tty.WriteStream).columns ?? 80,
  });
  const sameSize = (left: Size, right: Size): boolean =>
    left.rows === right.rows && left.cols === right.cols;

  let phase: AttachPhase = { _tag: "Ended" };
  let desiredSize = terminalSize();
  let adapterSequence = 0;
  let rawWasSet = false;
  let stdinDataHandler: ((data: Buffer) => void) | null = null;
  let stdinEndHandler: (() => void) | null = null;
  let resizeHandler: (() => void) | null = null;

  const phaseController = (value: AttachPhase): AbortController | null =>
    value._tag === "Opening" || value._tag === "Ready" || value._tag === "Reconnecting" || value._tag === "Detaching"
      ? value.controller
      : null;
  function transition(next: AttachPhase): void {
    const outgoing = phaseController(phase);
    const incoming = phaseController(next);
    phase = next;
    if (outgoing && outgoing !== incoming) outgoing.abort();
  }

  function enterRawMode(): void {
    if (stdin.isTTY && !stdin.isRaw && stdin.setRawMode) { stdin.setRawMode(true); rawWasSet = true; }
  }
  function exitRawMode(): void {
    if (rawWasSet && stdin.isTTY && stdin.setRawMode) stdin.setRawMode(false);
  }
  function teardownInput(): void {
    if (stdinDataHandler) { stdin.removeListener("data", stdinDataHandler); stdinDataHandler = null; }
    if (stdinEndHandler) { stdin.removeListener("end", stdinEndHandler); stdinEndHandler = null; }
    if (resizeHandler) { stdout.removeListener("resize", resizeHandler); resizeHandler = null; }
  }
  function cleanExit(requests: PassThrough | null): void {
    teardownInput();
    inputPolicy.dispose();
    exitRawMode();
    try { requests?.destroy(); } catch {}
  }
  function completeExit(code: number): void {
    if (options.onExit) options.onExit(code); else process.exit(code);
  }
  function finish(code: number): void {
    if (phase._tag === "Ended") return;
    const requests = phase._tag === "Opening" || phase._tag === "Ready" || phase._tag === "Detaching"
      ? phase.requests
      : null;
    transition({ _tag: "Ended" });
    cleanExit(requests);
    completeExit(code);
  }
  function completeDetach(): void {
    if (phase._tag === "Ended") return;
    const requests = phase._tag === "Opening" || phase._tag === "Ready" || phase._tag === "Detaching"
      ? phase.requests
      : null;
    transition({ _tag: "Ended" });
    cleanExit(requests);
    stdout.write(TERMINAL_SANITIZE + CURSOR_TO_BOTTOM + "\r\n[detached]\r\n");
    options.onDetach?.();
  }

  const writeRequest = (request: Parameters<typeof encodeMachineRequest>[0]): void => {
    if (phase._tag !== "Opening" && phase._tag !== "Ready" && phase._tag !== "Detaching") return;
    if (!phase.requests || phase.requests.destroyed) return;
    try { phase.requests.write(encodeMachineRequest(request)); } catch {}
  };
  const inputPolicy = new InteractiveInputPolicy({
    onInput: (bytes) => {
      if (phase._tag === "Ready") writeRequest({ _tag: "Input", bytes });
    },
    onDetach: () => {
      if (phase._tag === "Ended" || phase._tag === "Detaching") return;
      if (phase._tag === "Reconnecting" || phase._tag === "Opening") {
        const current = phase;
        transition({
          _tag: "Detaching",
          controller: current.controller,
          requests: current._tag === "Opening" ? current.requests : null,
        });
        completeDetach();
        return;
      }
      transition({ _tag: "Detaching", controller: phase.controller, requests: phase.requests });
      stdin.pause();
      writeRequest({ _tag: "Detach" });
    },
    onError: (error) => {
      console.error(`pty attach: ${error.message}`);
      finish(1);
    },
  });

  function wireInput(): void {
    if (stdinDataHandler) return;
    stdinDataHandler = (raw: Buffer) => inputPolicy.feed(Buffer.from(raw));
    stdinEndHandler = () => inputPolicy.end();
    stdin.on("data", stdinDataHandler);
    stdin.on("end", stdinEndHandler);
    resizeHandler = () => {
      desiredSize = terminalSize();
      if (phase._tag !== "Ready" || sameSize(phase.sentSize, desiredSize)) return;
      const { controller, requests } = phase;
      transition({ _tag: "Ready", controller, requests, sentSize: desiredSize });
      writeRequest({ _tag: "Resize", ...desiredSize });
    };
    stdout.on("resize", resizeHandler);
  }

  function handleResponse(response: MachineResponse): void {
    switch (response._tag) {
      case "Hello":
        break;
      case "Ready": {
        if (phase._tag !== "Opening") return;
        const { requests, sentSize } = phase;
        transition({ _tag: "Ready", controller: phase.controller, requests, sentSize });
        inputPolicy.setInputModes(response.inputModes);
        stdout.write("\x1b[2J\x1b[H");
        stdout.write(response.screen);
        if (!sameSize(sentSize, desiredSize)) {
          transition({ _tag: "Ready", controller: phase.controller, requests, sentSize: desiredSize });
          writeRequest({ _tag: "Resize", ...desiredSize });
        }
        stdin.resume();
        break;
      }
      case "Data":
        if (phase._tag !== "Ready") return;
        if (response.inputModes) inputPolicy.setInputModes(response.inputModes);
        stdout.write(response.bytes);
        break;
      case "Geometry":
        break;
      case "Exited":
        stdout.write(TERMINAL_SANITIZE + CURSOR_TO_BOTTOM + `\r\n[${options.name} exited with code ${response.code}]\r\n`);
        finish(response.code);
        break;
      case "Detached":
        completeDetach();
        break;
      case "AdmissionFailure":
        console.error(`pty attach: ${response.reason}${response.detail ? `: ${response.detail}` : ""}`);
        finish(1);
        break;
      case "StreamFailure":
        if (phase._tag === "Detaching") {
          completeDetach();
        } else if (canReconnect && (phase._tag === "Opening" || phase._tag === "Ready")) {
          beginReconnect();
        } else if (phase._tag !== "Ended") {
          console.error(`pty attach: ${response.reason}${response.diagnostic ? `: ${response.diagnostic}` : ""}`);
          finish(1);
        }
        break;
    }
  }

  function startAdapter(socket?: net.Socket): void {
    const sequence = ++adapterSequence;
    const controller = new AbortController();
    inputPolicy.discardPendingInput();
    const requests = new PassThrough();
    const responses = new PassThrough();
    const responseReader = new MachineFrameReader();
    desiredSize = terminalSize();
    transition({ _tag: "Opening", controller, requests, sentSize: desiredSize });
    stdin.resume();
    responses.on("data", (chunk: Buffer) => {
      if (sequence !== adapterSequence || phase._tag === "Ended") return;
      for (const frame of responseReader.feed(Buffer.from(chunk))) {
        handleResponse(decodeMachineResponse(frame));
      }
    });
    responses.once("end", () => {
      if (sequence !== adapterSequence || phase._tag === "Ended") return;
      if (phase._tag === "Detaching") completeDetach();
    });
    void machineAttachV2({
      input: requests,
      output: responses,
      ...(socket ? { connect: () => socket, preconnected: true } : {}),
      signal: controller.signal,
    }).catch((error) => {
      if (sequence !== adapterSequence || controller.signal.aborted || phase._tag === "Ended") return;
      console.error(`pty attach: ${error instanceof Error ? error.message : String(error)}`);
      finish(1);
    });
    requests.write(encodeMachineRequest({
      _tag: "Open",
      protocol: MACHINE_PROTOCOL_VERSION,
      sessionId: options.name,
      expectedGeneration: generation,
      rows: desiredSize.rows,
      cols: desiredSize.cols,
      requiredCapabilities: [
        "framed-utf8-input",
        "typed-outcome",
        "input-mode-snapshot",
        "host-terminal-replay",
      ],
    }));
  }

  // Re-establish the routed socket on a loud disconnect and re-attach; the
  // daemon replays screen+modes so the session resumes. Only reached when
  // `options.reconnect` is set (attach --remote) and the session didn't EXIT.
  function beginReconnect(): void {
    if (phase._tag !== "Opening" && phase._tag !== "Ready") return;
    adapterSequence++;
    const controller = new AbortController();
    transition({ _tag: "Reconnecting", controller });
    inputPolicy.discardPendingInput();
    stdout.write(`\r\n[reconnecting… — Ctrl-\\ to stop]\r\n`);
    stdin.resume();
    void reconnectLoop(controller);
  }

  async function reconnectLoop(controller: AbortController): Promise<void> {
    let attempt = 0;
    while (phase._tag === "Reconnecting" && !controller.signal.aborted) {
      const delay = RECONNECT_BACKOFF_MS[attempt] ?? RECONNECT_BACKOFF_CAP_MS;
      await new Promise<void>((resolve) => {
        const timer = setTimeout(done, delay);
        function done(): void {
          clearTimeout(timer);
          controller.signal.removeEventListener("abort", done);
          resolve();
        }
        controller.signal.addEventListener("abort", done, { once: true });
      });
      if (phase._tag !== "Reconnecting" || controller.signal.aborted) return;
      let fresh: net.Socket | null = null;
      try {
        fresh = await options.reconnect!(controller.signal);
      } catch {
        // Reject = reachable host that says the session is gone → clean give-up.
        // (Transport failures resolve null, so we keep retrying below.)
        if (phase._tag !== "Reconnecting" || controller.signal.aborted) return;
        stdout.write(TERMINAL_SANITIZE + CURSOR_TO_BOTTOM + `\r\n[${options.name} session ended]\r\n`);
        finish(0);
        return;
      }
      if (phase._tag !== "Reconnecting" || controller.signal.aborted) {
        try { fresh?.destroy(); } catch {}
        return;
      }
      if (fresh) {
        startAdapter(fresh);
        return;
      }
      // Transport failure (host unreachable) — retry, unlimited by default so a
      // laptop closed for hours still reconnects on reopen. Only a finite env cap
      // ends it (besides the user's Ctrl-\).
      if (++attempt >= RECONNECT_MAX_ATTEMPTS) {
        stdout.write(TERMINAL_SANITIZE + CURSOR_TO_BOTTOM + `\r\n[${options.name}: connection lost — re-run \`pty attach --remote\` to reconnect]\r\n`);
        finish(1);
        return;
      }
    }
  }

  enterRawMode();
  wireInput();
  startAdapter(options.socket);
}
