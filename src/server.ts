import * as net from "node:net";
import * as fs from "node:fs";
import * as path from "node:path";
import { execFileSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import { isUtf8 } from "node:buffer";
import * as pty from "node-pty";
// @xterm/headless is CJS-only, so keep its default import. The serialize addon
// ships native ESM with named exports, so import its runtime namespace.
import type { Terminal } from "@xterm/headless";
import type { SerializeAddon } from "@xterm/addon-serialize";
import xterm from "@xterm/headless";
import * as xtermSerialize from "@xterm/addon-serialize";
import {
  MessageType,
  PacketReader,
  encodeData,
  encodeExit,
  encodeScreen,
  encodeStatusResponse,
  encodeGeometry,
  decodeSize,
} from "./protocol.ts";
import {
  DaemonExtensionType,
  MACHINE_PROTOCOL_VERSION,
  canonicalizeMachineInputModeSnapshotV1,
  decodeDaemonOpenV2,
  decodeMachineRequest,
  encodeDaemonAdmissionV2,
  encodeMachineResponse,
  type MachineCapability,
  type MachineInputModeSnapshotV1,
  type MachineResponse,
} from "./machine-protocol.ts";
import { readPackageVersion } from "./version.ts";
import {
  getSocketPath,
  getPidPath,
  getMetadataPath,
  getSessionDir,
  ensureSessionDir,
  cleanupOwnedSocket,
  cleanupOwnedAll,
  writeMetadata,
  readMetadata,
  mutateMetadataUnderLock,
  shouldReapAtExit,
  reapOnExitDefault,
  type SessionMetadata,
  type MetadataMutationResult,
} from "./sessions.ts";
import { EventWriter, clearEvents, EventType, type EventRecord } from "./events.ts";
import {
  RECOVERY_PROTOCOL,
  assertPrivateRecoveryPaths,
  atomicWritePrivate,
  ensureRecoveryDir,
  launchIdentity,
  metadataRevision,
  publishPrivateNoReplace,
  readBoundedJson,
  readProcessStartToken,
  recoveryDir,
  recoveryLockContents,
  recoveryLockIdentity,
  recoveryRequestPath,
  recoveryRevisionPath,
  recoveryResultPath,
  signRecoveryRevision,
  signRecoveryResult,
  stampRecoveryMetadata,
  verifyRecoveryRequest,
  verifyRecoveryRevision,
  type RecoveryCapability,
  type RecoveryRequest,
  type RecoveryRevision,
  type RecoveryResultPayload,
} from "./recovery.ts";
import type { StatsResult } from "./client.ts";

interface Client {
  socket: net.Socket;
  output: BoundedClientOutput;
  reader: PacketReader;
  role:
    | { readonly _tag: "Command" }
    | { readonly _tag: "Rejected" }
    | {
        readonly _tag: "Legacy";
        readonly phase:
          | { readonly _tag: "Settling" }
          | {
              readonly _tag: "Cutting";
              readonly hasPendingExit: boolean;
            }
          | { readonly _tag: "Live" };
        readonly rows: number;
        readonly cols: number;
        readonly attachSeq: number;
      }
    | {
        readonly _tag: "Readonly";
        readonly phase:
          | { readonly _tag: "Settling" }
          | {
              readonly _tag: "Cutting";
              readonly hasPendingExit: boolean;
            }
          | { readonly _tag: "Live" };
      }
    | {
        readonly _tag: "MachineAwaitingSentinel";
        readonly rows: number;
        readonly cols: number;
      }
    | {
        readonly _tag: "Machine";
        readonly phase: "syncing" | "live" | "ended";
        readonly rows: number;
        readonly cols: number;
        readonly attachSeq: number;
      };
  /** Invalidates a delayed SCREEN when the same socket changes attach mode. */
  initialScreenGeneration: number;
}

/** One legal maximum-sized protocol frame plus its five-byte envelope. */
export const DEFAULT_MAX_CLIENT_OUTPUT_BYTES = 32 * 1024 * 1024 + 5;
/** Reserved independently so queue saturation can still produce a typed outcome. */
export const CLIENT_TERMINAL_OUTCOME_RESERVE_BYTES = 64 * 1024 + 5;

/**
 * Owns all retained output for one client. Socket backpressure never pauses
 * the PTY or another client: later frames wait here, in order, until `drain`.
 * The normal byte limit includes Node's socket buffer, the live queue, and
 * frames held behind an initial-screen cut; only a final typed outcome may use
 * the additional fixed reserve.
 */
export class BoundedClientOutput {
  private readonly queued: Buffer[] = [];
  private queuedBytes = 0;
  private readonly held: Buffer[] = [];
  private heldBytes = 0;
  private blocked = false;
  private ending = false;
  private closed = false;

  constructor(
    private readonly socket: net.Socket,
    private readonly maxBufferedBytes = DEFAULT_MAX_CLIENT_OUTPUT_BYTES,
  ) {
    if (!Number.isSafeInteger(maxBufferedBytes) || maxBufferedBytes <= 0) {
      throw new RangeError("maxBufferedBytes must be a positive safe integer");
    }
    socket.on("drain", () => this.flush());
    socket.on("close", () => this.discard());
  }

  write(packet: Buffer): boolean {
    if (!this.canRetain(packet.length, false)) return false;
    this.enqueue(packet);
    return true;
  }

  hold(packet: Buffer): boolean {
    if (!this.canRetain(packet.length, false)) return false;
    this.held.push(packet);
    this.heldBytes += packet.length;
    return true;
  }

  releaseHeld(initialPacket: Buffer): boolean {
    if (!this.canRetain(initialPacket.length, false)) return false;
    const held = this.held.splice(0);
    this.heldBytes = 0;
    this.enqueue(initialPacket);
    for (const packet of held) this.enqueue(packet);
    return true;
  }

  clearHeld(): void {
    this.held.length = 0;
    this.heldBytes = 0;
  }

  end(packet?: Buffer): boolean {
    if (packet !== undefined) {
      if (!this.canRetain(packet.length, true)) return false;
      this.enqueue(packet);
    }
    this.ending = true;
    this.finishEndIfReady();
    return true;
  }

  destroy(): void {
    this.discard();
    this.socket.destroy();
  }

  private canRetain(additionalBytes: number, includeOutcomeReserve: boolean): boolean {
    const limit = this.maxBufferedBytes +
      (includeOutcomeReserve ? CLIENT_TERMINAL_OUTCOME_RESERVE_BYTES : 0);
    return !this.closed && !this.ending && !this.socket.destroyed &&
      this.socket.writableLength + this.queuedBytes + this.heldBytes + additionalBytes <=
        limit;
  }

  private enqueue(packet: Buffer): void {
    if (this.blocked || this.queued.length > 0) {
      this.queued.push(packet);
      this.queuedBytes += packet.length;
      return;
    }
    try {
      this.blocked = !this.socket.write(packet);
    } catch {
      this.destroy();
    }
  }

  private flush(): void {
    if (this.closed) return;
    this.blocked = false;
    while (!this.blocked && this.queued.length > 0) {
      const packet = this.queued.shift()!;
      this.queuedBytes -= packet.length;
      try {
        this.blocked = !this.socket.write(packet);
      } catch {
        this.destroy();
        return;
      }
    }
    this.finishEndIfReady();
  }

  private finishEndIfReady(): void {
    if (!this.closed && this.ending && !this.blocked && this.queued.length === 0) {
      this.closed = true;
      this.socket.end();
    }
  }

  private discard(): void {
    this.closed = true;
    this.queued.length = 0;
    this.queuedBytes = 0;
    this.clearHeld();
  }
}

interface TerminalInputModeState {
  readonly revision: number;
  readonly applicationCursorKeys: boolean;
  readonly applicationKeypad: boolean;
  readonly bracketedPaste: boolean;
  readonly focusReporting: boolean;
  readonly modifyOtherKeys: 0 | 1 | 2;
  readonly mouseTracking9: boolean;
  readonly mouseTracking1000: boolean;
  readonly mouseTracking1002: boolean;
  readonly mouseTracking1003: boolean;
  readonly mouseEncoding1005: boolean;
  readonly mouseEncoding1006: boolean;
  readonly mouseEncoding1015: boolean;
  readonly mouseEncoding1016: boolean;
  readonly kittyKeyboardFlags: readonly number[];
}

const MACHINE_CAPABILITIES = [
  "framed-utf8-input",
  "typed-outcome",
  "input-mode-snapshot",
] as const satisfies readonly MachineCapability[];
const MAX_KITTY_KEYBOARD_STACK_DEPTH = 64;

export interface ServerOptions {
  name: string;
  generation?: string;
  command: string;
  args: string[];
  displayCommand: string;
  cwd: string;
  rows: number;
  cols: number;
  ephemeral?: boolean;
  tags?: Record<string, string>;
  /** Optional human-friendly alias recorded in SessionMetadata.displayName.
   *  Mutable via `pty rename`; `name` stays the immutable stable id. */
  displayName?: string;
  onExit?: (code: number) => void;
  /** Maximum normal output retained per client, excluding the bounded terminal-outcome reserve. */
  maxClientOutputBytes?: number;
  /** When true, spawn the child with a scrubbed environment containing only
   *  a small allow-list of variables (plus any entries in `extraEnv`).
   *  Intended for contexts where the daemon may have inherited secrets that
   *  shouldn't leak into the session (e.g., a daemon launched by pty-relay
   *  for a remote client). See BUG-4. */
  isolateEnv?: boolean;
  /** Additional `KEY=VALUE` env entries overlaid on the inherited child
   *  environment, or on the safe allow-list when `isolateEnv` is true. */
  extraEnv?: Record<string, string>;
  /** Environment keys removed from the inherited child environment. Applied
   *  before `extraEnv`, so an explicit assignment wins when both mention a key. */
  unsetEnv?: string[];
  /** Use this env dict verbatim for the spawned child — no inheritance from
   *  the daemon's `process.env`, no allow-list. `PTY_SESSION` and the opaque
   *  `PTY_SESSION_GENERATION` owner token are always injected on top so
   *  nesting detection and generation-safe `pty exec` keep working.
   *
   *  Mutually exclusive with `isolateEnv` / `extraEnv` / `unsetEnv` — passing
   *  `env` together with inherited-environment policy throws. Use this when
   *  the caller wants total control of the child environment (e.g., a
   *  launcher shell that injects a shim tmux on `PATH`). */
  env?: Record<string, string>;
}

/** Env variables that are safe to pass through to a session child when
 *  `isolateEnv` is on. Keeps terminal/locale/path functionality working
 *  without propagating the operator's shell secrets. */
const ISOLATED_ENV_ALLOWLIST = new Set([
  "PATH", "HOME", "USER", "LOGNAME", "SHELL",
  "TERM", "COLORTERM", "LANG", "TZ", "PWD", "TMPDIR",
  // pty-internal
  "PTY_ROOT",
  "PTY_SESSION_DIR",
]);

/** Fallback TERM for child PTYs when no value was inherited. `xterm-256color`
 *  is the lowest common denominator every modern TUI knows how to drive; the
 *  kitty keyboard / modifyOtherKeys handshakes are dynamic CSI probes that
 *  work fine on top of it. Important specifically for daemons launched from
 *  a parent with a minimal env (launchd, systemd, cron, sparse CI runners) —
 *  those contexts drop TERM entirely, and a child without TERM causes many
 *  TUIs (Claude Code, vim, etc.) to fall back to legacy key encoding where
 *  Shift+Enter is indistinguishable from Enter. */
const DEFAULT_CHILD_TERM = "xterm-256color";

/** Apply the TERM default in-place after the env has been assembled. Never
 *  overrides an explicit value — only fills in when it's absent. */
function ensureChildTerm(env: Record<string, string>): void {
  if (!env.TERM) env.TERM = DEFAULT_CHILD_TERM;
}

function buildChildEnv(options: ServerOptions): Record<string, string> {
  // Mutual exclusion: `env` (explicit, verbatim) can't be combined with the
  // inherited-environment policy path. If you want total control you pass
  // `env`; otherwise isolation/removals/assignments compose explicitly. Picking
  // one implicitly would hide intent.
  if (options.env && (options.isolateEnv || options.extraEnv || options.unsetEnv?.length)) {
    throw new Error(
      "ServerOptions.env is mutually exclusive with isolateEnv/extraEnv/unsetEnv. " +
      "Use env for verbatim control, or inherited environment policy options — not both."
    );
  }

  // Explicit verbatim env. No inheritance. PTY's identity and owner token are
  // forced on top so internal tooling can fail closed across same-id reuse.
  if (options.env) {
    const env = { ...options.env };
    env.PTY_SESSION = options.name;
    if (options.generation) env.PTY_SESSION_GENERATION = options.generation;
    ensureChildTerm(env);
    return env;
  }

  const source = process.env as Record<string, string>;

  if (!options.isolateEnv) {
    // Legacy behaviour: full inheritance, minus the server-config handoff.
    const env = { ...source };
    delete env.PTY_SERVER_CONFIG;
    for (const key of options.unsetEnv ?? []) delete env[key];
    if (options.extraEnv) {
      for (const [k, v] of Object.entries(options.extraEnv)) env[k] = v;
    }
    env.PTY_SESSION = options.name;
    if (options.generation) env.PTY_SESSION_GENERATION = options.generation;
    ensureChildTerm(env);
    return env;
  }

  const env: Record<string, string> = {};
  for (const [k, v] of Object.entries(source)) {
    if (v === undefined) continue;
    if (ISOLATED_ENV_ALLOWLIST.has(k) || k.startsWith("LC_")) env[k] = v;
  }
  for (const key of options.unsetEnv ?? []) delete env[key];
  if (options.extraEnv) {
    for (const [k, v] of Object.entries(options.extraEnv)) env[k] = v;
  }
  env.PTY_SESSION = options.name;
  if (options.generation) env.PTY_SESSION_GENERATION = options.generation;
  ensureChildTerm(env);
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
  private retiredSocketServers: net.Server[] = [];
  private clients = new Map<net.Socket, Client>();
  private exited = false;
  private exitCode = 0;
  private name: string;
  private options: ServerOptions;
  private attachCounter = 0;
  private cursorHidden = false;
  private inputModeState: TerminalInputModeState = {
    revision: 0,
    applicationCursorKeys: false,
    applicationKeypad: false,
    bracketedPaste: false,
    focusReporting: false,
    modifyOtherKeys: 0,
    mouseTracking9: false,
    mouseTracking1000: false,
    mouseTracking1002: false,
    mouseTracking1003: false,
    mouseEncoding1005: false,
    mouseEncoding1006: false,
    mouseEncoding1015: false,
    mouseEncoding1016: false,
    kittyKeyboardFlags: [],
  };
  private lastCommittedInputModeRevision = 0;
  private outputRevision = 0;
  // Alt-screen buffer state (DEC private modes ?1049 / ?1047 / ?47). Set when
  // the child process enters the alternate screen buffer; cleared when it
  // leaves. Replayed to attaching clients so the SCREEN snapshot lands in the
  // right host-terminal buffer — without this, a TUI's alt-screen frames get
  // painted into the host's main buffer, which under tmux means every frame
  // enters scrollback (see #41).
  private altScreenActive = false;
  // Mouse tracking modes — these are separate DEC private modes (set/cleared
  // independently by the child process) that control WHICH events the
  // terminal should report. SGR mode (1006) only controls the ENCODING of
  // reports, not whether tracking is active. Clients attaching to a session
  // that's already mid-stream need all active modes replayed so their own
  // mouse forwarding logic sees the correct state.
  private lastResizeTime = 0;
  private eventWriter: EventWriter;
  private generation: string;
  private recoveryCapability: RecoveryCapability | null = null;
  private recoveryRoot = "";
  private recoveryInFlight = false;
  private recoveryWatcher: fs.FSWatcher | null = null;
  private lastTitle = "";
  readonly ready: Promise<void>;
  // Resolves when the child process's onExit has fired — used by close() to
  // make sure session_exit has been queued to the event chain before we
  // flush and exit the daemon. See flake #2.
  private childExited: Promise<void>;
  private resolveChildExited!: () => void;

  constructor(options: ServerOptions) {
    this.name = options.name;
    this.options = options;
    this.generation = options.generation ?? randomBytes(16).toString("hex");
    this.eventWriter = new EventWriter(options.name);
    this.childExited = new Promise<void>((resolve) => {
      this.resolveChildExited = resolve;
    });

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
          if (v === 1) this.updateInputModes({ applicationCursorKeys: true });
          if (v === 9) this.updateInputModes({ mouseTracking9: true });
          if (v === 1000) this.updateInputModes({ mouseTracking1000: true });
          if (v === 1002) this.updateInputModes({ mouseTracking1002: true });
          if (v === 1003) this.updateInputModes({ mouseTracking1003: true });
          if (v === 1004) this.updateInputModes({ focusReporting: true });
          if (v === 1005) this.updateInputModes({ mouseEncoding1005: true });
          if (v === 1006) this.updateInputModes({ mouseEncoding1006: true });
          if (v === 1015) this.updateInputModes({ mouseEncoding1015: true });
          if (v === 1016) this.updateInputModes({ mouseEncoding1016: true });
          if (v === 2004) this.updateInputModes({ bracketedPaste: true });
          if (v === 1049 || v === 1047 || v === 47) this.altScreenActive = true;
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
          if (v === 1) this.updateInputModes({ applicationCursorKeys: false });
          if (v === 9) this.updateInputModes({ mouseTracking9: false });
          if (v === 1000) this.updateInputModes({ mouseTracking1000: false });
          if (v === 1002) this.updateInputModes({ mouseTracking1002: false });
          if (v === 1003) this.updateInputModes({ mouseTracking1003: false });
          if (v === 1004) this.updateInputModes({ focusReporting: false });
          if (v === 1005) this.updateInputModes({ mouseEncoding1005: false });
          if (v === 1006) this.updateInputModes({ mouseEncoding1006: false });
          if (v === 1015) this.updateInputModes({ mouseEncoding1015: false });
          if (v === 1016) this.updateInputModes({ mouseEncoding1016: false });
          if (v === 2004) this.updateInputModes({ bracketedPaste: false });
          if (v === 1049 || v === 1047 || v === 47) this.altScreenActive = false;
          if (v === 25) this.cursorHidden = true;
        }
        return false;
      }
    );
    this.terminal.parser.registerCsiHandler(
      { prefix: ">", final: "u" },
      (params) => {
        const first = params[0];
        const flags = first === undefined
          ? 0
          : typeof first === "number"
            ? first
            : first[0];
        if (!Number.isInteger(flags) || flags < 0 || flags > 0xffffffff) {
          this.failMachineInputModes("kitty-keyboard-flags-out-of-range");
          return false;
        }
        if (
          this.inputModeState.kittyKeyboardFlags.length >=
          MAX_KITTY_KEYBOARD_STACK_DEPTH
        ) {
          this.failMachineInputModes("kitty-keyboard-stack-overflow");
          return false;
        }
        this.updateInputModes({
          kittyKeyboardFlags: [...this.inputModeState.kittyKeyboardFlags, flags],
        });
        return false;
      }
    );
    this.terminal.parser.registerCsiHandler(
      { prefix: "=", final: "u" },
      (params) => {
        const first = params[0];
        const flags = first === undefined
          ? 0
          : typeof first === "number"
            ? first
            : first[0];
        if (!Number.isInteger(flags) || flags < 0 || flags > 0xffffffff) {
          this.failMachineInputModes("kitty-keyboard-flags-out-of-range");
          return false;
        }
        const second = params[1];
        const mode = second === undefined
          ? 1
          : typeof second === "number"
            ? second
            : second[0];
        const stack = [...this.inputModeState.kittyKeyboardFlags];
        const current = stack.at(-1) ?? 0;
        const updated = mode === 2
          ? (current | flags) >>> 0
          : mode === 3
            ? (current & ~flags) >>> 0
            : flags;
        if (stack.length === 0) stack.push(updated);
        else stack[stack.length - 1] = updated;
        this.updateInputModes({ kittyKeyboardFlags: stack });
        return false;
      }
    );
    this.terminal.parser.registerCsiHandler(
      { prefix: ">", final: "m" },
      (params) => {
        const first = params[0];
        const command = first === undefined
          ? 0
          : typeof first === "number"
            ? first
            : first[0];
        if (command !== 4) return false;
        const second = params[1];
        const requested = second === undefined
          ? 0
          : typeof second === "number"
            ? second
            : second[0];
        const level: 0 | 1 | 2 = requested === 1 ? 1 : requested === 2 ? 2 : 0;
        this.updateInputModes({ modifyOtherKeys: level });
        return false;
      }
    );
    this.terminal.parser.registerCsiHandler(
      { prefix: "<", final: "u" },
      (params) => {
        const first = params[0];
        const requested = first === undefined
          ? 1
          : typeof first === "number"
            ? first
            : first[0];
        const count = Math.max(1, Math.min(requested, MAX_KITTY_KEYBOARD_STACK_DEPTH));
        this.updateInputModes({
          kittyKeyboardFlags: this.inputModeState.kittyKeyboardFlags.slice(0, -count),
        });
        return false;
      }
    );
    this.terminal.parser.registerEscHandler({ final: "=" }, () => {
      this.updateInputModes({ applicationKeypad: true });
      return false;
    });
    this.terminal.parser.registerEscHandler({ final: ">" }, () => {
      this.updateInputModes({ applicationKeypad: false });
      return false;
    });

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
    const childEnv = buildChildEnv({ ...options, generation: this.generation });

    const invalidCwd = describeInvalidCwd(options.cwd);
    if (invalidCwd !== undefined) {
      throw new Error(
        `${invalidCwd}\nCannot start session "${options.name}" for command "${options.command}".`
      );
    }

    try {
      // NOTE: intentionally no `name:` option here — node-pty's `name`
      // unconditionally clobbers env.TERM, which would hide any TERM the
      // caller inherited or set explicitly. `buildChildEnv` guarantees
      // childEnv.TERM is populated (defaulting to xterm-256color if absent),
      // so node-pty will pick it up naturally. Was `name: "xterm-256color"`
      // before; removing it lets inherited values like `xterm-kitty` flow
      // through and lets TUIs negotiate the richer capabilities they allow.
      this.ptyProcess = pty.spawn(
        "/bin/sh",
        ["-c", 'exec "$@"', "sh", options.command, ...options.args],
        {
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
    this.ptyProcess.onData((data: string) => this.handlePtyOutput(data));

    this.ptyProcess.onExit(({ exitCode, signal }) => {
      this.exited = true;
      // A signal death (e.g. an OS OOM SIGKILL) arrives from node-pty with a
      // nonzero `signal` and often exitCode 0 — if we recorded only the raw
      // exitCode, a killed process would look like a clean finish and any
      // consumer gating on "nonzero exit" (convoy's crash→ding) would miss it.
      // Surface it the way a shell does: 128 + signal (SIGKILL 9 → 137).
      const code = signal ? 128 + signal : exitCode;
      this.exitCode = code;
      this.broadcastLegacy(MessageType.EXIT, encodeExit(code));
      this.terminal.write("", () => {
        this.broadcastMachine({
          _tag: "Exited",
          code,
          signal: signal ? String(signal) : null,
        });
      });
      this.emitEvent(EventType.SESSION_EXIT, {
        exitCode: code,
        ...(signal ? { signal } : {}),
      });
      // Save exit status immediately so the session shows as "exited"
      // in pty list during the cleanup window. lastLines may be incomplete
      // here since PTY data could still be in-flight — close() will
      // update with the final output.
      const exitMetadataStatus = this.saveExitMetadata(code);
      if (exitMetadataStatus === "busy" || exitMetadataStatus === "stale") {
        // Startup may still hold the creation lock for this generation. Retry
        // within the existing 500ms client grace so exit metadata is observable
        // before shutdown cleanup, while close() retains the final bounded retry.
        void this.saveExitMetadataUntilSettled(code, 400).catch(() => {});
      }
      this.resolveChildExited();
      options.onExit?.(code);
    });

    // Create Unix socket server
    ensureSessionDir();
    this.recoveryRoot = path.resolve(getSessionDir());
    const processStartToken = readProcessStartToken(process.pid);
    try {
      ensureRecoveryDir(this.recoveryRoot);
      const paths = assertPrivateRecoveryPaths(this.recoveryRoot);
      if (processStartToken === null) throw new Error("process start identity unavailable");
      const identity = launchIdentity({
        command: options.command,
        args: options.args,
        displayCommand: options.displayCommand,
        cwd: options.cwd,
        rows: options.rows,
        cols: options.cols,
        ephemeral: options.ephemeral,
        isolateEnv: options.isolateEnv,
        extraEnv: options.extraEnv,
        env: options.env,
      });
      this.recoveryCapability = {
        protocol: RECOVERY_PROTOCOL,
        secret: randomBytes(32).toString("hex"),
        processStartToken,
        launchIdentity: identity,
        ...paths,
        metadataRevision: "",
      };
      this.startRecoveryWatcher();
    } catch {}
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
          generation: this.generation,
          daemonPid: process.pid,
          ...(this.recoveryCapability ? { recovery: this.recoveryCapability } : {}),
          command: options.command,
          args: options.args,
          displayCommand: options.displayCommand,
          cwd: options.cwd,
          rows: options.rows,
          cols: options.cols,
          ephemeral: options.ephemeral === true,
          createdAt: new Date().toISOString(),
          ...(options.tags && Object.keys(options.tags).length > 0 ? { tags: options.tags } : {}),
          ...(options.displayName ? { displayName: options.displayName } : {}),
          ...(options.isolateEnv ? { isolateEnv: true } : {}),
          ...(options.extraEnv && Object.keys(options.extraEnv).length > 0 ? { extraEnv: options.extraEnv } : {}),
          ...(options.unsetEnv && options.unsetEnv.length > 0 ? { unsetEnv: options.unsetEnv } : {}),
          ...(options.env ? { env: options.env } : {}),
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

  private startRecoveryWatcher(): void {
    const requestPath = recoveryRequestPath(this.recoveryRoot, this.name);
    this.recoveryWatcher = fs.watch(
      recoveryDir(this.recoveryRoot),
      { persistent: false },
      (_event, filename) => {
        if (
          filename === path.basename(requestPath) &&
          fs.existsSync(requestPath) &&
          !this.recoveryInFlight
        ) {
          void this.handleRecoveryRequest();
        }
      },
    );
  }

  private recoveryMetadata(
    observed: SessionMetadata,
    capability: RecoveryCapability,
  ): SessionMetadata {
    return stampRecoveryMetadata({
      ...observed,
      generation: this.generation,
      daemonPid: process.pid,
      recovery: capability,
    });
  }

  private async handleRecoveryRequest(): Promise<void> {
    const capability = this.recoveryCapability;
    if (!capability || this.recoveryInFlight) return;
    this.recoveryInFlight = true;
    const requestPath = recoveryRequestPath(this.recoveryRoot, this.name);
    const resultPath = recoveryResultPath(this.recoveryRoot, this.name);
    let request: RecoveryRequest | null = null;
    let result: RecoveryResultPayload | null = null;
    try {
      assertPrivateRecoveryPaths(this.recoveryRoot, capability);
      request = readBoundedJson<RecoveryRequest>(requestPath);
      const currentStart = readProcessStartToken(process.pid);
      const metadataCapability = request.metadata?.recovery;
      const lockPath = path.join(this.recoveryRoot, `${this.name}.lock`);
      const lockContents = recoveryLockContents(process.pid, request.lockIdentity);
      const expectedLockIdentity = recoveryLockIdentity({
        name: request.name,
        daemonPid: request.daemonPid,
        processStartToken: request.processStartToken,
        rootDevice: request.rootDevice,
        rootInode: request.rootInode,
        recoveryDirDevice: capability.recoveryDirDevice,
        recoveryDirInode: capability.recoveryDirInode,
      });
      const revision = readBoundedJson<RecoveryRevision>(
        recoveryRevisionPath(this.recoveryRoot, this.name),
      );
      const exact =
        request.protocol === RECOVERY_PROTOCOL &&
        request.name === this.name &&
        request.daemonPid === process.pid &&
        request.generation === this.generation &&
        request.processStartToken === capability.processStartToken &&
        request.launchIdentity === capability.launchIdentity &&
        request.rootDevice === capability.rootDevice &&
        request.rootInode === capability.rootInode &&
        currentStart === capability.processStartToken &&
        request.lockIdentity === expectedLockIdentity &&
        fs.readFileSync(lockPath, "utf8") === lockContents &&
        metadataCapability?.protocol === capability.protocol &&
        metadataCapability.secret === capability.secret &&
        metadataCapability.processStartToken === capability.processStartToken &&
        metadataCapability.launchIdentity === capability.launchIdentity &&
        metadataCapability.rootDevice === capability.rootDevice &&
        metadataCapability.rootInode === capability.rootInode &&
        metadataCapability.recoveryDirDevice === capability.recoveryDirDevice &&
        metadataCapability.recoveryDirInode === capability.recoveryDirInode &&
        metadataCapability.metadataRevision === metadataRevision(request.metadata) &&
        revision.protocol === RECOVERY_PROTOCOL &&
        revision.name === this.name &&
        revision.generation === this.generation &&
        revision.metadataRevision === metadataCapability.metadataRevision &&
        verifyRecoveryRevision(capability.secret, revision) &&
        verifyRecoveryRequest(capability.secret, request);
      if (!exact) throw new Error("recovery identity or authentication mismatch");

      const socketPath = getSocketPath(this.name);
      const pidPath = getPidPath(this.name);
      const metadataPath = getMetadataPath(this.name);
      for (const target of [socketPath, pidPath, metadataPath]) {
        if (fs.existsSync(target)) throw new Error("recovery target is no longer empty");
      }

      const replacement = net.createServer((socket) => this.handleClient(socket));
      await new Promise<void>((resolve, reject) => {
        replacement.once("error", reject);
        assertPrivateRecoveryPaths(this.recoveryRoot, capability);
        replacement.listen(socketPath, resolve);
      });
      replacement.on("error", (error) => {
        console.error(`Socket server error: ${error.message}`);
      });
      let socketIdentity: { dev: number; ino: number } | null = null;
      let publishedPid = false;
      let publishedMetadata = false;
      let rotatedCapability: RecoveryCapability | null = null;
      try {
        fs.chmodSync(socketPath, 0o600);
        const socketStat = fs.lstatSync(socketPath);
        socketIdentity = { dev: socketStat.dev, ino: socketStat.ino };
        if (fs.existsSync(pidPath) || fs.existsSync(metadataPath)) {
          throw new Error("recovery sidecar appeared during publication");
        }
        assertPrivateRecoveryPaths(this.recoveryRoot, capability);
        const rotated: RecoveryCapability = {
          ...capability,
          secret: randomBytes(32).toString("hex"),
          metadataRevision: "",
        };
        const recoveredMetadata = this.recoveryMetadata(request.metadata, rotated);
        rotatedCapability = recoveredMetadata.recovery!;
        // Advance the authoritative signed revision before any rotated
        // capability-bearing metadata becomes visible. A later publication
        // failure intentionally leaves recovery unavailable rather than
        // allowing the old snapshot/secret to roll metadata back.
        assertPrivateRecoveryPaths(this.recoveryRoot, capability);
        atomicWritePrivate(
          recoveryRevisionPath(this.recoveryRoot, this.name),
          signRecoveryRevision(rotatedCapability.secret, {
            protocol: RECOVERY_PROTOCOL,
            name: this.name,
            generation: this.generation,
            metadataRevision: rotatedCapability.metadataRevision,
          }),
        );
        publishPrivateNoReplace(pidPath, process.pid.toString());
        publishedPid = true;
        publishPrivateNoReplace(metadataPath, JSON.stringify(recoveredMetadata, null, 2));
        publishedMetadata = true;
        const finalSocket = fs.lstatSync(socketPath);
        if (finalSocket.dev !== socketIdentity.dev || finalSocket.ino !== socketIdentity.ino) {
          throw new Error("recovery pathname was replaced during publication");
        }

        const previous = this.socketServer;
        this.socketServer = replacement;
        this.recoveryCapability = rotatedCapability;
        // Node remembers a Unix server's pathname and unlinks it on close.
        // The old listener still remembers the same string even though its
        // inode was externally unlinked; closing it now would unlink the new
        // listener. Keep the unreachable fd unref'd until daemon shutdown.
        previous.unref();
        this.retiredSocketServers.push(previous);
        result = {
          protocol: RECOVERY_PROTOCOL,
          name: this.name,
          nonce: request.nonce,
          ok: true,
          daemonPid: process.pid,
          generation: this.generation,
          processStartToken: capability.processStartToken,
          launchIdentity: capability.launchIdentity,
        };
      } catch (error) {
        try { replacement.close(); } catch {}
        if (publishedMetadata && rotatedCapability) {
          try {
            const current = readMetadata(this.name);
            if (current?.recovery?.secret === rotatedCapability.secret) {
              fs.unlinkSync(metadataPath);
            }
          } catch {}
        }
        if (publishedPid) {
          try {
            if (fs.readFileSync(pidPath, "utf8").trim() === String(process.pid)) {
              fs.unlinkSync(pidPath);
            }
          } catch {}
        }
        if (socketIdentity) {
          try {
            const current = fs.lstatSync(socketPath);
            if (current.dev === socketIdentity.dev && current.ino === socketIdentity.ino) {
              fs.unlinkSync(socketPath);
            }
          } catch {}
        }
        throw error;
      }
    } catch (error) {
      result = {
        protocol: RECOVERY_PROTOCOL,
        name: this.name,
        nonce: request?.nonce ?? "",
        ok: false,
        error: error instanceof Error ? error.message : "recovery refused",
      };
    } finally {
      try {
        assertPrivateRecoveryPaths(this.recoveryRoot, capability);
        if (result) {
          atomicWritePrivate(
            resultPath,
            signRecoveryResult(capability.secret, result),
          );
        }
        fs.unlinkSync(requestPath);
      } catch {}
      this.recoveryInFlight = false;
    }
  }

  private inputModeSnapshot(): MachineInputModeSnapshotV1 {
    const state = this.inputModeState;
    const mouseTracking = state.mouseTracking1003
      ? "AnyMotion" as const
      : state.mouseTracking1002
        ? "ButtonMotion" as const
        : state.mouseTracking1000
          ? "Click" as const
          : state.mouseTracking9
            ? "X10Press" as const
            : "Off" as const;
    const mouseEncoding = state.mouseEncoding1016 || state.mouseEncoding1006
      ? "Sgr" as const
      : state.mouseEncoding1015
        ? "Urxvt" as const
        : state.mouseEncoding1005
          ? "Utf8" as const
          : "X10" as const;
    return canonicalizeMachineInputModeSnapshotV1({
      schema: "pty.input-mode.v1",
      wireEncoder: "xterm-input.v1",
      revision: state.revision,
      applicationCursorKeys: state.applicationCursorKeys,
      applicationKeypad: state.applicationKeypad,
      bracketedPaste: state.bracketedPaste,
      focusReporting: state.focusReporting,
      modifyOtherKeys: state.modifyOtherKeys,
      kittyKeyboardFlagsStack: [...state.kittyKeyboardFlags],
      mouseTracking,
      mouseEncoding,
      mouseCoordinates: state.mouseEncoding1016 ? "Pixel" : "Cell",
    });
  }

  private updateInputModes(
    patch: Partial<Omit<TerminalInputModeState, "revision">>
  ): void {
    const current = this.inputModeState;
    const next = { ...current, ...patch };
    const unchanged =
      current.applicationCursorKeys === next.applicationCursorKeys &&
      current.applicationKeypad === next.applicationKeypad &&
      current.bracketedPaste === next.bracketedPaste &&
      current.focusReporting === next.focusReporting &&
      current.modifyOtherKeys === next.modifyOtherKeys &&
      current.mouseTracking9 === next.mouseTracking9 &&
      current.mouseTracking1000 === next.mouseTracking1000 &&
      current.mouseTracking1002 === next.mouseTracking1002 &&
      current.mouseTracking1003 === next.mouseTracking1003 &&
      current.mouseEncoding1005 === next.mouseEncoding1005 &&
      current.mouseEncoding1006 === next.mouseEncoding1006 &&
      current.mouseEncoding1015 === next.mouseEncoding1015 &&
      current.mouseEncoding1016 === next.mouseEncoding1016 &&
      current.kittyKeyboardFlags.length === next.kittyKeyboardFlags.length &&
      current.kittyKeyboardFlags.every(
        (flags, index) => flags === next.kittyKeyboardFlags[index]
      );
    if (unchanged) return;
    this.inputModeState = { ...next, revision: current.revision + 1 };
  }

  private handlePtyOutput(data: string): void {
    const cleaned = stripTerminalQueries(data);
    if (cleaned.length > 0) {
      this.broadcastLegacy(MessageType.DATA, encodeData(cleaned));
    }
    this.terminal.write(data, () => {
      if (cleaned.length === 0) return;
      let inputModes: MachineInputModeSnapshotV1 | undefined;
      if (this.inputModeState.revision > this.lastCommittedInputModeRevision) {
        try {
          inputModes = this.inputModeSnapshot();
        } catch {
          this.failMachineInputModes("unrepresentable-input-mode");
          return;
        }
      }
      const outputRevision = this.outputRevision + 1;
      this.outputRevision = outputRevision;
      this.broadcastMachine({
        _tag: "Data",
        bytes: Buffer.from(cleaned),
        outputRevision,
        inputModeRevision: this.inputModeState.revision,
        ...(inputModes === undefined ? {} : { inputModes }),
      });
      this.lastCommittedInputModeRevision = this.inputModeState.revision;
    });
  }

  private failMachineInputModes(reason: string): void {
    for (const client of [...this.clients.values()]) {
      if (client.role._tag !== "Machine" || client.role.phase === "ended") continue;
      this.endMachineStream(client, {
        _tag: "StreamFailure",
        phase: client.role.phase === "syncing" ? "baseline" : "stream",
        reason,
      });
    }
  }

  private rejectMachineOpen(
    client: Client,
    reason: "generation-mismatch" | "not-found" | "unsupported-capability" | "malformed-request",
    detail?: string
  ): void {
    client.role = { _tag: "Rejected" };
    if (!client.output.end(
      encodeDaemonAdmissionV2({
        _tag: "Rejected",
        reason,
        ...(detail === undefined ? {} : { detail }),
      })
    )) client.output.destroy();
  }

  private handleMachineOpen(
    client: Client,
    frame: { readonly type: number; readonly payload: Buffer }
  ): void {
    if (client.role._tag !== "Command") {
      this.rejectMachineOpen(client, "malformed-request", "OPEN_V2 requires a fresh command connection");
      return;
    }
    let open;
    try {
      open = decodeDaemonOpenV2(frame);
    } catch (error) {
      this.rejectMachineOpen(
        client,
        "malformed-request",
        error instanceof Error ? error.message : "invalid OPEN_V2"
      );
      return;
    }
    if (open.sessionId !== this.name) {
      this.rejectMachineOpen(client, "not-found", "session id does not match socket");
      return;
    }
    if (open.expectedGeneration !== this.generation) {
      this.rejectMachineOpen(client, "generation-mismatch", "session generation changed");
      return;
    }
    const unsupported = open.requiredCapabilities.find(
      (capability) => !MACHINE_CAPABILITIES.includes(capability)
    );
    if (unsupported !== undefined) {
      this.rejectMachineOpen(
        client,
        "unsupported-capability",
        `unsupported capability: ${unsupported}`
      );
      return;
    }

    client.role = {
      _tag: "MachineAwaitingSentinel",
      rows: open.rows,
      cols: open.cols,
    };
  }

  private handleMachineRequest(
    client: Client,
    frame: { readonly type: number; readonly payload: Buffer }
  ): void {
    let request;
    try {
      request = decodeMachineRequest(frame);
    } catch (error) {
      this.endMachineStream(client, {
        _tag: "StreamFailure",
        phase:
          client.role._tag === "Machine" && client.role.phase === "syncing"
            ? "baseline"
            : "stream",
        reason: "malformed-request",
        diagnostic: error instanceof Error ? error.message : "invalid request",
      });
      return;
    }
    if (
      client.role._tag === "Machine" &&
      client.role.phase === "syncing" &&
      request._tag === "Detach"
    ) {
      this.endMachineStream(client, { _tag: "Detached" });
      return;
    }
    if (client.role._tag !== "Machine" || client.role.phase !== "live") {
      this.endMachineStream(client, {
        _tag: "StreamFailure",
        phase: "baseline",
        reason: "request-before-ready",
      });
      return;
    }
    switch (request._tag) {
      case "Open":
        this.endMachineStream(client, {
          _tag: "StreamFailure",
          phase: "stream",
          reason: "duplicate-open",
        });
        break;
      case "Input":
        if (!isUtf8(request.bytes)) {
          this.endMachineStream(client, {
            _tag: "StreamFailure",
            phase: "stream",
            reason: "invalid-utf8-input",
          });
          break;
        }
        if (!this.exited) this.ptyProcess.write(request.bytes.toString("utf8"));
        break;
      case "Resize":
        client.role = {
          _tag: "Machine",
          phase: "live",
          rows: request.rows,
          cols: request.cols,
          attachSeq: ++this.attachCounter,
        };
        this.negotiateSize();
        break;
      case "Detach":
        this.endMachineStream(client, { _tag: "Detached" });
        break;
    }
  }

  private endMachineStream(
    client: Client,
    outcome: Extract<MachineResponse, { _tag: "Detached" | "StreamFailure" }>
  ): void {
    if (client.role._tag === "Machine" && client.role.phase === "ended") return;
    const role = client.role;
    const rows = role._tag === "Machine" || role._tag === "MachineAwaitingSentinel"
      ? role.rows
      : this.terminal.rows;
    const cols = role._tag === "Machine" || role._tag === "MachineAwaitingSentinel"
      ? role.cols
      : this.terminal.cols;
    const attachSeq = role._tag === "Machine" ? role.attachSeq : 0;
    client.role = { _tag: "Machine", phase: "ended", rows, cols, attachSeq };
    client.initialScreenGeneration++;
    client.output.clearHeld();
    if (!client.output.end(encodeMachineResponse(outcome))) client.output.destroy();
    this.negotiateSize();
  }

  private handleSlowConsumer(client: Client): void {
    const role = client.role;
    client.initialScreenGeneration++;
    client.output.clearHeld();
    if (role._tag === "Machine" && role.phase !== "ended") {
      client.role = { ...role, phase: "ended" };
      const failure = encodeMachineResponse({
        _tag: "StreamFailure",
        phase: role.phase === "syncing" ? "baseline" : "stream",
        reason: "slow-consumer",
      });
      if (!client.output.end(failure)) client.output.destroy();
      this.negotiateSize();
      return;
    }
    this.clients.delete(client.socket);
    client.output.destroy();
    this.negotiateSize();
  }

  private writeClient(client: Client, packet: Buffer): boolean {
    if (client.output.write(packet)) return true;
    this.handleSlowConsumer(client);
    return false;
  }

  private admitWritableClient(
    client: Client,
    size: { readonly rows: number; readonly cols: number },
    protocol: "legacy" | "machine-v2",
    onCommitted?: () => void
  ): void {
    client.output.clearHeld();
    const sizeMatched =
      size.rows === this.terminal.rows && size.cols === this.terminal.cols;
    const attachSeq = ++this.attachCounter;
    client.role = protocol === "machine-v2"
      ? { _tag: "Machine", phase: "syncing", ...size, attachSeq }
      : { _tag: "Legacy", phase: { _tag: "Settling" }, ...size, attachSeq };
    const initialScreenGeneration = ++client.initialScreenGeneration;
    const resized = this.negotiateSize();
    if (!resized && protocol === "legacy") {
      this.writeClient(client, encodeGeometry(this.terminal.rows, this.terminal.cols));
    }
    try {
      mutateMetadataUnderLock(this.name, (meta) => {
        meta.lastAttachAt = new Date().toISOString();
        return true;
      }, { expectedGeneration: this.generation });
    } catch {}
    onCommitted?.();

    const sendScreen = () => {
      this.beginInitialScreenCut(
        client,
        initialScreenGeneration,
        () => protocol === "machine-v2"
          ? encodeMachineResponse({
              _tag: "Ready",
              rows: this.terminal.rows,
              cols: this.terminal.cols,
              outputRevision: this.outputRevision,
              inputModes: this.inputModeSnapshot(),
              screen: Buffer.from(this.serialize.serialize()),
            })
          : encodeScreen(this.getModePrefix(true) + this.serialize.serialize()),
        () => {
          if (!this.exited && !sizeMatched) this.nudgeRedraw();
        }
      );
    };

    if (!this.exited) {
      const sinceLast = Date.now() - this.lastResizeTime;
      const redrawSettleMs = 80;
      if (resized || sinceLast < redrawSettleMs) {
        const delay = resized ? redrawSettleMs : redrawSettleMs - sinceLast;
        setTimeout(sendScreen, delay);
      } else {
        sendScreen();
      }
    } else {
      sendScreen();
    }
  }

  private handleClient(socket: net.Socket): void {
    const client: Client = {
      socket,
      output: new BoundedClientOutput(
        socket,
        this.options.maxClientOutputBytes ?? DEFAULT_MAX_CLIENT_OUTPUT_BYTES,
      ),
      reader: new PacketReader(),
      role: { _tag: "Command" },
      initialScreenGeneration: 0,
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
        if (client.role._tag === "Rejected") continue;
        if (client.role._tag === "MachineAwaitingSentinel") {
          if (
            Number(packet.type) === MessageType.STATUS &&
            packet.payload.length === 0
          ) {
            const { rows, cols } = client.role;
            this.admitWritableClient(
              client,
              { rows, cols },
              "machine-v2",
              () => this.writeClient(client,
                encodeDaemonAdmissionV2({
                  _tag: "Accepted",
                  protocol: MACHINE_PROTOCOL_VERSION,
                  generation: this.generation,
                  capabilities: MACHINE_CAPABILITIES,
                  build: { version: readPackageVersion(), dirty: false },
                })
              )
            );
            continue;
          }
          this.rejectMachineOpen(
            client,
            "malformed-request",
            "STATUS sentinel must immediately follow OPEN_V2"
          );
          continue;
        }
        if (client.role._tag === "Machine") {
          this.handleMachineRequest(client, packet);
          continue;
        }
        if (Number(packet.type) === DaemonExtensionType.OPEN_V2) {
          this.handleMachineOpen(client, packet);
          continue;
        }
        switch (packet.type) {
          case MessageType.ATTACH: {
            if (packet.payload.length < 4) break;
            this.admitWritableClient(client, decodeSize(packet.payload), "legacy");
            break;
          }

          case MessageType.PEEK: {
            client.output.clearHeld();
            client.role = { _tag: "Readonly", phase: { _tag: "Settling" } };
            const initialScreenGeneration = ++client.initialScreenGeneration;
            const resized = this.negotiateSize();
            if (!resized) {
              this.writeClient(client, encodeGeometry(this.terminal.rows, this.terminal.cols));
            }
            const flags = packet.payload.length > 0 ? packet.payload.readUInt8(0) : 0;
            const plain = (flags & 1) !== 0;
            const full = (flags & 2) !== 0;

            this.beginInitialScreenCut(client, initialScreenGeneration, () => {
              if (plain) {
                return encodeScreen(
                  full ? this.getFullPlainScreen() : this.getPlainScreen()
                );
              }
              // scrollback: 0 for viewport only, omit for full scrollback
              const serializeOpts = full ? undefined : { scrollback: 0 };
              return encodeScreen(
                this.getModePrefix() + this.serialize.serialize(serializeOpts)
              );
            });
            break;
          }

          case MessageType.DATA: {
            if (!this.exited && client.role._tag !== "Readonly") {
              this.ptyProcess.write(packet.payload.toString());
            }
            break;
          }

          case MessageType.RESIZE: {
            if (client.role._tag === "Legacy" && packet.payload.length >= 4) {
              const size = decodeSize(packet.payload);
              client.role = {
                _tag: "Legacy",
                phase: client.role.phase,
                rows: size.rows,
                cols: size.cols,
                attachSeq: ++this.attachCounter,
              };
              this.negotiateSize();
            }
            break;
          }

          case MessageType.DETACH: {
            client.output.end();
            break;
          }

          case MessageType.STATUS: {
            const stats = this.collectStats();
            this.writeClient(client, encodeStatusResponse(JSON.stringify(stats)));
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

  private getModePrefix(includeAltScreen = false): string {
    let prefix = "";
    // Alt-screen mode is only prefixed for the ATTACH path, not PEEK. A
    // non-follow `pty peek` prints the snapshot to the caller's shell and
    // exits, so entering ?1049h would hide the output when the client-side
    // TERMINAL_SANITIZE exits alt-screen on close. Attaching clients want
    // the alt buffer to persist for the duration of the attach.
    if (includeAltScreen && this.altScreenActive) prefix += "\x1b[?1049h";
    if (this.inputModeState.applicationCursorKeys) prefix += "\x1b[?1h";
    if (this.inputModeState.applicationKeypad) prefix += "\x1b=";
    if (this.inputModeState.modifyOtherKeys > 0) {
      prefix += `\x1b[>4;${this.inputModeState.modifyOtherKeys}m`;
    }
    if (this.inputModeState.mouseTracking9) prefix += "\x1b[?9h";
    if (this.inputModeState.mouseTracking1000) prefix += "\x1b[?1000h";
    if (this.inputModeState.mouseTracking1002) prefix += "\x1b[?1002h";
    if (this.inputModeState.mouseTracking1003) prefix += "\x1b[?1003h";
    if (this.inputModeState.focusReporting) prefix += "\x1b[?1004h";
    if (this.inputModeState.mouseEncoding1005) prefix += "\x1b[?1005h";
    if (this.inputModeState.mouseEncoding1006) prefix += "\x1b[?1006h";
    if (this.inputModeState.mouseEncoding1015) prefix += "\x1b[?1015h";
    if (this.inputModeState.mouseEncoding1016) prefix += "\x1b[?1016h";
    if (this.inputModeState.bracketedPaste) prefix += "\x1b[?2004h";
    if (this.cursorHidden) prefix += "\x1b[?25l";
    for (const flags of this.inputModeState.kittyKeyboardFlags) {
      prefix += `\x1b[>${flags}u`;
    }
    return prefix;
  }

  private collectStats(): StatsResult {
    const buf = this.terminal.buffer.active;
    const meta = readMetadata(this.name);

    let attached = 0;
    let readOnly = 0;
    const connections: NonNullable<StatsResult["clients"]["connections"]> = [];
    for (const c of this.clients.values()) {
      if (c.role._tag === "Readonly") {
        readOnly++;
        connections.push({
          role: "readonly",
          constrains: { rows: false, cols: false },
        });
      } else if (
        c.role._tag === "Legacy" ||
        (c.role._tag === "Machine" && c.role.phase !== "ended")
      ) {
        attached++;
        connections.push({
          role: "writable",
          rows: c.role.rows,
          cols: c.role.cols,
          lastRequestSequence: c.role.attachSeq,
          constrains: {
            rows: c.role.rows === this.terminal.rows,
            cols: c.role.cols === this.terminal.cols,
          },
        });
      }
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
        connections,
      },
      modes: {
        sgrMouse: this.inputModeState.mouseEncoding1006,
        cursorHidden: this.cursorHidden,
        kittyKeyboard: this.inputModeState.kittyKeyboardFlags.length > 0,
        kittyKeyboardFlags: [...this.inputModeState.kittyKeyboardFlags],
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
      const role = client.role;
      if (
        role._tag === "Legacy" ||
        (role._tag === "Machine" && role.phase !== "ended")
      ) {
        rows = rows === 0 ? role.rows : Math.min(rows, role.rows);
        cols = cols === 0 ? role.cols : Math.min(cols, role.cols);
      }
    }

    if (rows > 0 && cols > 0) {
      if (rows !== this.terminal.rows || cols !== this.terminal.cols) {
        this.terminal.resize(cols, rows);
        this.broadcastGeometry(rows, cols);
        this.ptyProcess.resize(cols, rows);
        this.lastResizeTime = Date.now();
        return true;
      }
    }
    return false;
  }

  private broadcastGeometry(rows: number, cols: number): void {
    const legacyPacket = encodeGeometry(rows, cols);
    const machinePacket = encodeMachineResponse({ _tag: "Geometry", rows, cols });
    for (const client of this.clients.values()) {
      if (client.role._tag === "Legacy" || client.role._tag === "Readonly") {
        this.writeClient(client, legacyPacket);
      } else if (client.role._tag === "Machine" && client.role.phase === "live") {
        this.writeClient(client, machinePacket);
      }
    }
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

  private beginInitialScreenCut(
    client: Client,
    generation: number,
    getInitialPacket: () => Buffer,
    onLive?: () => void
  ): void {
    if (
      client.socket.destroyed ||
      client.initialScreenGeneration !== generation
    ) return;
    if (
      (client.role._tag === "Legacy" || client.role._tag === "Readonly") &&
      client.role.phase._tag === "Settling"
    ) {
      client.role = {
        ...client.role,
        phase: { _tag: "Cutting", hasPendingExit: false },
      };
    }

    /** xterm parses writes asynchronously. This empty write is an ordered
     *  marker: its callback runs after every earlier write and before later
     *  writes, giving SCREEN an exact parser cut. */
    this.terminal.write("", () => {
      if (
        client.socket.destroyed ||
        client.initialScreenGeneration !== generation
      ) return;

      const role = client.role;
      const hasPendingExit =
        (role._tag === "Legacy" || role._tag === "Readonly") &&
        role.phase._tag === "Cutting" && role.phase.hasPendingExit;
      let initialPacket: Buffer;
      try {
        initialPacket = getInitialPacket();
      } catch {
        if (client.role._tag === "Machine") {
          this.endMachineStream(client, {
            _tag: "StreamFailure",
            phase: "baseline",
            reason: "unrepresentable-input-mode",
          });
        } else {
          client.output.destroy();
        }
        return;
      }
      if (!client.output.releaseHeld(initialPacket)) {
        this.handleSlowConsumer(client);
        return;
      }
      if (role._tag === "Legacy") {
        client.role = { ...role, phase: { _tag: "Live" } };
      } else if (role._tag === "Readonly") {
        client.role = { ...role, phase: { _tag: "Live" } };
      } else if (role._tag === "Machine" && role.phase === "syncing") {
        client.role = { ...role, phase: "live" };
      }
      if (this.exited && !hasPendingExit) {
        this.writeClient(client,
          client.role._tag === "Machine"
            ? encodeMachineResponse({
                _tag: "Exited",
                code: this.exitCode,
                signal: null,
              })
            : encodeExit(this.exitCode)
        );
      }
      onLive?.();
    });
  }

  private broadcastLegacy(
    type: typeof MessageType.DATA | typeof MessageType.EXIT,
    packet: Buffer
  ): void {
    for (const client of this.clients.values()) {
      const role = client.role;
      if (role._tag !== "Legacy" && role._tag !== "Readonly") continue;
      if (role.phase._tag === "Live") {
        this.writeClient(client, packet);
      } else if (role.phase._tag === "Cutting") {
        if (!client.output.hold(packet)) {
          this.handleSlowConsumer(client);
        } else if (type === MessageType.EXIT && !role.phase.hasPendingExit) {
          client.role = {
            ...role,
            phase: { _tag: "Cutting", hasPendingExit: true },
          };
        }
      }
    }
  }

  private broadcastMachine(response: MachineResponse): void {
    const packet = encodeMachineResponse(response);
    for (const client of this.clients.values()) {
      if (client.role._tag === "Machine" && client.role.phase === "live") {
        this.writeClient(client, packet);
      }
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

  private saveExitMetadata(exitCode: number): MetadataMutationResult["status"] {
    const result = mutateMetadataUnderLock(this.name, (metadata) => {
      metadata.exitCode = exitCode;
      metadata.exitedAt = new Date().toISOString();
      metadata.lastLines = this.getLastLines();
      return true;
    }, { expectedGeneration: this.generation });
    return result.status;
  }

  private async saveExitMetadataUntilSettled(
    exitCode: number,
    waitMs = 2_000,
  ): Promise<void> {
    const deadline = Date.now() + waitMs;
    while (true) {
      const status = this.saveExitMetadata(exitCode);
      if (
        status === "changed" ||
        status === "unchanged" ||
        status === "missing" ||
        status === "generation-mismatch"
      ) return;
      if (Date.now() >= deadline) return;
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
  }

  /** Clean up resources. Does not call process.exit(). */
  close(): Promise<void> {
    if (this.recoveryRoot) {
      try { this.recoveryWatcher?.close(); } catch {}
      this.recoveryWatcher = null;
    }
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
      for (const retired of this.retiredSocketServers.splice(0)) {
        try { retired.close(); } catch {}
      }
      this.socketServer.close(async () => {
        cleanupOwnedSocket(this.name, {
          generation: this.generation,
          pid: process.pid,
        });
        try {
          this.ptyProcess.kill();
        } catch {}
        // Wait for the child's onExit to fire (which enqueues session_exit)
        // before draining the writer. Without this, SIGTERM-initiated
        // shutdowns race: kill() returns synchronously but onExit fires
        // later, after we've already flushed. Bound with a short timeout in
        // case the child never exits (shouldn't happen — we just killed it).
        await Promise.race([
          this.childExited,
          new Promise<void>((r) => setTimeout(r, 2000)),
        ]);
        if (this.exited) await this.saveExitMetadataUntilSettled(this.exitCode);
        try { await this.eventWriter.flush(); } catch {}
        resolve();
      });
    });
  }

  /** Hard-kill the child with SIGKILL, bypassing the graceful SIGHUP that
   *  close() sends. Used by the shutdown backstop: when a graceful close()
   *  has wedged (e.g. a frozen child that ignores SIGHUP), the daemon is about
   *  to force-exit and must not leave the child orphaned to init still alive.
   *  Best-effort — a SIGKILL is unblockable, but the child may already be gone. */
  forceKillChild(): void {
    try { this.ptyProcess.kill("SIGKILL"); } catch {}
  }
}

/** How often the spawner-PID watchdog checks for liveness. 5s is fast enough
 *  that a leaked daemon is reclaimed promptly without producing meaningful
 *  CPU load. */
const SPAWNER_POLL_INTERVAL_MS = 5000;

/** Returns true if `pid` refers to a live process this user can signal.
 *  `kill(pid, 0)` is the standard POSIX liveness probe — sends no signal,
 *  only validates the target. ESRCH means dead; EPERM means alive but
 *  unsignalable (still "alive" for our purposes). */
function isProcessAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    return (err as NodeJS.ErrnoException).code === "EPERM";
  }
}

function installSpawnerWatchdog(cleanShutdown: (code: number) => Promise<never>): void {
  const raw = process.env.PTY_SPAWNER_PID;
  if (!raw) return;
  const pid = Number(raw);
  if (!Number.isInteger(pid) || pid <= 1) return;
  if (!isProcessAlive(pid)) {
    // Already dead by the time we boot — exit before clients can connect.
    void cleanShutdown(0);
    return;
  }
  const interval = setInterval(() => {
    if (isProcessAlive(pid)) return;
    clearInterval(interval);
    void cleanShutdown(0);
  }, SPAWNER_POLL_INTERVAL_MS);
  // Don't keep the event loop alive just for this poll.
  interval.unref?.();
}

/** Entry point when this file is run as the daemon process. */
if (process.argv[1]?.endsWith("/server.js")) {
  // Name the daemon process so it shows up as "pty-daemon" in ps/top/htop/btm
  // rather than "MainThread" (V8's default main-thread name under Node 24+).
  // This is set only inside the daemon-entry guard — server.ts is also imported
  // as a library (PtyServer), and we must not rename those host processes.
  // `process.title` is the only override for /proc/<pid>/comm and Linux caps it
  // at 15 chars (TASK_COMM_LEN), so "pty-daemon" (10 chars) stays well under.
  try { process.title = "pty-daemon"; } catch {}

  const config = JSON.parse(process.env.PTY_SERVER_CONFIG ?? "{}");
  if (!config.name || !config.command) {
    console.error("PTY_SERVER_CONFIG env var required");
    process.exit(1);
  }

  const isEphemeral = config.ephemeral === true;
  const generation =
    typeof config.generation === "string" && config.generation.length > 0
      ? config.generation
      : randomBytes(16).toString("hex");
  const cleanupOwner = { generation, pid: process.pid };

  // Exit-time cleanup policy. Deliberately re-reads metadata at shutdown
  // instead of trusting `config.tags` (the spawn-time snapshot): `pty tag
  // <name> keep` and `pty tag <name> strategy=permanent` both mutate a
  // RUNNING session's tags on disk, and pinning a session you are about to
  // debug is the main reason anyone sets `keep` at all. A stale read here
  // would reap exactly the session the operator asked us to preserve.
  //
  // Falls back to `config.tags` only if the metadata file is unreadable
  // (already `pty rm`'d, or a truncated write) — a missing file means
  // there is nothing left to reap anyway.
  //
  // `externalKill` scopes the policy to the case it is actually about: the
  // session's own work ENDED. A daemon can shut down for two very different
  // reasons, and only one of them is an "exit":
  //
  //   - the child process terminated on its own (cleanly or by crashing)
  //     — the session is finished; whether it self-reaps is the config
  //     default (`reapOnExitDefault`, via `PTY_REAP_ON_EXIT`), unless a
  //     per-session `keep`/`--ephemeral` overrides it.
  //   - someone stopped the daemon from outside (`pty kill` → SIGTERM,
  //     SIGINT, or the spawner watchdog reclaiming a leaked daemon) — the
  //     child had not finished; an operator interrupted it, almost always
  //     to go look at what it was doing. `pty kill` is documented as
  //     stop-and-keep, deliberately distinct from `pty rm`. Collapsing the
  //     two would delete the evidence the operator killed the session to
  //     inspect. Keep.
  //
  // `--ephemeral` remains the pre-existing aggressive opt-in and reaps on
  // either path, so no existing caller of it regresses.
  let externalKill = false;
  function reapAtExit(): boolean {
    const metadata = readMetadata(config.name);
    if (
      metadata?.generation !== undefined &&
      metadata.generation !== generation
    ) {
      return false;
    }
    if (externalKill && !isEphemeral) return false;
    const tags = metadata?.tags ?? config.tags;
    // `PTY_REAP_ON_EXIT` (network/global config) sets the default when no
    // per-session `keep`/`--ephemeral` override applies; shipped default reaps.
    return shouldReapAtExit(tags, isEphemeral, reapOnExitDefault());
  }

  // Hard deadline for a graceful shutdown before the daemon force-exits. The
  // graceful path (server.close()) is itself only internally bounded on the
  // child-exit wait (~2s); the outer promise can still hang indefinitely if
  // socketServer.close()'s callback never fires (a lingering/untracked socket)
  // or eventWriter.flush() stalls. That is exactly how a restart wedged the
  // daemon alive+orphaned (ppid=1), needing kill -9. This backstop guarantees
  // the daemon exits — and reaps its child — regardless. Env-overridable so
  // tests can force the backstop path deterministically.
  const SHUTDOWN_DEADLINE_MS = (() => {
    const raw = Number(process.env.PTY_SHUTDOWN_DEADLINE_MS);
    return Number.isFinite(raw) && raw > 0 ? raw : 5000;
  })();

  // Idempotent: SIGTERM, SIGINT, the child's onExit, and the spawner watchdog
  // can all trigger shutdown, sometimes overlapping. Only the first arms the
  // deadline and drives close(); later callers get the same in-flight promise.
  let shutdownPromise: Promise<never> | null = null;
  function cleanShutdown(code: number): Promise<never> {
    if (shutdownPromise) return shutdownPromise;
    const deadline = setTimeout(() => {
      console.error(
        `pty daemon "${config.name}": graceful shutdown exceeded ` +
        `${SHUTDOWN_DEADLINE_MS}ms — forcing exit (child reaped)`,
      );
      // Don't leave a frozen child orphaned, and don't leave a stale pid/sock
      // pointing at a daemon that's about to vanish.
      server.forceKillChild();
      try {
        if (reapAtExit()) cleanupOwnedAll(config.name, cleanupOwner);
        else cleanupOwnedSocket(config.name, cleanupOwner);
      } catch {}
      process.exit(code);
    }, SHUTDOWN_DEADLINE_MS);
    shutdownPromise = server.close().then(() => {
      clearTimeout(deadline);
      // `close()` has already re-flushed exit metadata with the final
      // `lastLines`, so this reads the same tags a `pty gc` sweep would
      // have read one tick later — the decision is identical, only the
      // latency changes.
      if (reapAtExit()) cleanupOwnedAll(config.name, cleanupOwner);
      process.exit(code);
    });
    return shutdownPromise;
  }

  const server = new PtyServer({
    name: config.name,
    generation,
    command: config.command,
    args: config.args ?? [],
    displayCommand: config.displayCommand,
    cwd: config.cwd ?? process.cwd(),
    rows: config.rows ?? 24,
    cols: config.cols ?? 80,
    ephemeral: config.ephemeral === true,
    tags: config.tags,
    displayName: config.displayName,
    isolateEnv: config.isolateEnv === true,
    extraEnv: config.extraEnv,
    unsetEnv: config.unsetEnv,
    env: config.env,
    onExit: (code) => {
      // Give clients a moment to receive the exit message, then shut down
      setTimeout(() => cleanShutdown(code), 500);
    },
  });

  // Set the flag BEFORE calling cleanShutdown: the child's own onExit will
  // re-enter cleanShutdown moments later (we kill the child during close()),
  // and cleanShutdown is idempotent, so whichever caller arrives first wins.
  // Flagging first guarantees the external-kill verdict is the one in effect
  // when the reap decision is finally made.
  const killedExternally = (code: number) => {
    externalKill = true;
    return cleanShutdown(code);
  };
  process.on("SIGTERM", () => killedExternally(0));
  process.on("SIGINT", () => killedExternally(0));

  // Spawner-PID watchdog (opt-in via PTY_SPAWNER_PID).
  //
  // `detached: true` puts the daemon in its own session, so the kernel sends
  // no signal when the spawner exits — and the daemon ends up reparented to
  // init, surviving forever. When the spawner sets PTY_SPAWNER_PID, we poll
  // for its liveness and call cleanShutdown() once it's gone. Off when the
  // env var is absent, so existing callers see no behaviour change.
  // The watchdog reclaims a daemon whose spawner died. The child was still
  // running, so this is a stop-from-outside like `pty kill`, not an exit —
  // keep the metadata.
  installSpawnerWatchdog(killedExternally);
}
