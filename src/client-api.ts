// Public API for programmatic session management.
// Import from "@myobie/pty/client".

// Session management
export {
  listSessions, getSession, gc, pruneOrphanLayoutTags,
  validateName, updateTags, setDisplayName,
  getSessionDir, getSocketPath,
  cleanupSocket, cleanupAll,
  type SessionInfo, type SessionMetadata, type PrunedTagResult,
} from "./sessions.ts";

// Session creation
export { spawnDaemon, resolveCommand, waitForSocket, type SpawnDaemonOptions } from "./spawn.ts";

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
  type EventRecord, type EventBase,
  type BellEvent, type TitleChangeEvent, type NotificationEvent,
  type FocusRequestEvent, type CursorVisibleEvent,
  type SessionStartEvent, type SessionExitEvent, type SessionExecEvent,
  type SessionRestartEvent, type SessionFailedEvent,
  type SupervisorStartEvent, type SupervisorStopEvent,
  type FollowerOptions,
} from "./events.ts";

// Project files
export { readPtyFile, type PtyFile, type PtySessionDef } from "./ptyfile.ts";

// Tag filter helpers (used by --filter-tag; shared with pty-relay)
export { extractFilterTags, matchesAllTags, isReservedTagKey } from "./tags.ts";

// Keys
export { resolveKey, parseSeqValue } from "./keys.ts";

// Protocol (advanced)
export {
  PacketReader, MessageType,
  type Packet,
} from "./protocol.ts";
