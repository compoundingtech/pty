// Public API for programmatic session management.
// Import from "@myobie/pty/client".

// Session management
export {
  listSessions, getSession, gc, pruneOrphanLayoutTags, isGone,
  validateName, updateTags, setDisplayName,
  getSessionDir, getSocketPath,
  cleanupSocket, cleanupAll,
  type SessionInfo, type SessionMetadata, type PrunedTagResult, type GcResult,
} from "./sessions.ts";

// Session creation
export { spawnDaemon, resolveCommand, waitForSocket, setServerModulePath, type SpawnDaemonOptions } from "./spawn.ts";

// Session interaction (programmatic — no process.exit, no stdin/stdout)
export {
  SessionConnection, sendData, peekScreen,
  type SessionConnectionOptions, type SendDataOptions, type PeekScreenOptions,
} from "./connection.ts";

// Session interaction (CLI-oriented — uses process.stdin/stdout, may call process.exit)
export {
  attach, peek, send, queryStats,
  TERMINAL_SANITIZE,
  type AttachOptions, type PeekOptions, type SendOptions,
  type StatsResult, type ProcessResources,
} from "./client.ts";

// Events
export {
  EventType,
  EventFollower, readRecentEvents, formatEvent,
  emitUserEvent, appendEvent, isUserEvent, validateUserEventType,
  type EventRecord, type EventBase,
  type BellEvent, type TitleChangeEvent, type NotificationEvent,
  type FocusRequestEvent, type CursorVisibleEvent,
  type SessionStartEvent, type SessionExitEvent, type SessionExecEvent,
  type SessionRespawnEvent,
  type UserEvent,
  type DisplayNameChangeEvent, type TagsChangeEvent,
  type FollowerOptions,
} from "./events.ts";

// Project files — pty.toml reader shared with convoy's manifest processing
// (convoy reads pty.toml verbatim per notes/lean-pty-core-supervision-spec.md §4).
export { readPtyFile, commandWithEnvExports, type PtyFile, type PtySessionDef } from "./ptyfile.ts";

// Reboot-cutover: shared classifier primitives that convoy's respawn loop
// mirrors verbatim (spec §5 + §8.1 wire-format freeze).
export { commandFingerprint, DEFAULT_FAST_FAIL_WINDOW_SEC, DEFAULT_FAST_FAIL_LIMIT } from "./sessions.ts";

// Tag filter helpers (used by --filter-tag; shared with pty-relay)
export { extractFilterTags, matchesAllTags, isReservedTagKey } from "./tags.ts";

// Duration parse/format — used by `pty list --older-than/--newer-than`,
// available here so downstream tools can accept the same grammar.
export { parseDuration, formatDuration } from "./duration.ts";

// Keys
export { resolveKey, parseSeqValue } from "./keys.ts";

// Protocol (advanced)
export {
  PacketReader, MessageType,
  type Packet,
} from "./protocol.ts";
