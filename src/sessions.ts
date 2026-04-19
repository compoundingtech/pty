import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";
import * as net from "node:net";

const DEFAULT_SESSION_DIR = path.join(os.homedir(), ".local", "state", "pty");

const DEAD_SESSION_TTL_MS = 24 * 60 * 60 * 1000; // 24 hours

const VALID_NAME_RE = /^[a-zA-Z0-9._-]+$/;

// Maximum bytes available to `sockaddr_un.sun_path`. Darwin/BSD = 104,
// Linux = 108. Pick the smallest so the same name works everywhere.
const SUN_PATH_MAX = 104;

export function validateName(name: string): void {
  if (!name || name.length === 0) {
    throw new Error("Session name cannot be empty.");
  }
  if (name.length > 255) {
    throw new Error("Session name too long (max 255 characters).");
  }
  if (!VALID_NAME_RE.test(name)) {
    throw new Error(
      `Invalid session name "${name}". Names may only contain letters, numbers, dots, hyphens, and underscores.`
    );
  }

  // Reject names whose resulting Unix-socket path would exceed the kernel
  // limit. Without this check the daemon's listen() fails with EINVAL inside
  // an error handler that used to silently log and hang. See BUG-1.
  const socketPath = path.join(getSessionDir(), `${name}.sock`);
  const byteLen = Buffer.byteLength(socketPath, "utf-8");
  if (byteLen > SUN_PATH_MAX) {
    const overflow = byteLen - SUN_PATH_MAX;
    throw new Error(
      `Session name "${name}" produces a socket path of ${byteLen} bytes, ` +
      `which exceeds the ${SUN_PATH_MAX}-byte kernel limit by ${overflow}. ` +
      `Shorten the name or set PTY_SESSION_DIR to a shorter path.`
    );
  }
}

export function getSessionDir(): string {
  return process.env.PTY_SESSION_DIR ?? DEFAULT_SESSION_DIR;
}

export function ensureSessionDir(): void {
  fs.mkdirSync(getSessionDir(), { recursive: true, mode: 0o700 });
}

export function getSocketPath(name: string): string {
  return path.join(getSessionDir(), `${name}.sock`);
}

export function getPidPath(name: string): string {
  return path.join(getSessionDir(), `${name}.pid`);
}

export function getMetadataPath(name: string): string {
  return path.join(getSessionDir(), `${name}.json`);
}

export function getEventsPath(name: string): string {
  return path.join(getSessionDir(), `${name}.events.jsonl`);
}

export interface SessionMetadata {
  command: string;
  args: string[];
  displayCommand: string; // original command as the user typed it
  cwd: string;
  createdAt: string;
  exitCode?: number;
  exitedAt?: string;
  lastLines?: string[];
  tags?: Record<string, string>;
  /** Optional human-friendly alias for the session. Mutable via `pty rename`.
   *  The immutable stable id is always `SessionInfo.name`. Most code should
   *  keep using `name`; `displayName` is purely for presentation and as an
   *  additional lookup key alongside `name`. */
  displayName?: string;
}

export interface SessionInfo {
  name: string;
  socketPath: string;
  pid: number | null;
  /**
   * `running`  — daemon process is alive and its socket is reachable.
   * `exited`   — daemon wrote an exit record (`exitCode` / `exitedAt`) before
   *              shutting down; we know how it ended.
   * `vanished` — the daemon process is gone but no exit record was written.
   *              Most commonly caused by SIGKILL / OOM / power-loss, where the
   *              daemon had no chance to finalise metadata. Same reapability
   *              as `exited` (still cleaned up by `pty gc`), but the exit
   *              details are forever unknown.
   */
  status: "running" | "exited" | "vanished";
  metadata: SessionMetadata | null;
}

/** Semantic helper: session has metadata but no live daemon (either `exited`
 *  or `vanished`). Use this wherever the branch is "there's a record and we
 *  might want to re-use cwd/tags/displayName"; reserve `=== "exited"` for
 *  branches that specifically care about clean-exit details. */
export function isGone(status: SessionInfo["status"]): boolean {
  return status === "exited" || status === "vanished";
}

export function writeMetadata(name: string, metadata: SessionMetadata): void {
  ensureSessionDir();
  const target = getMetadataPath(name);
  const tmp = target + ".tmp";
  fs.writeFileSync(tmp, JSON.stringify(metadata, null, 2));
  fs.renameSync(tmp, target);
}

/** Set or clear the displayName on an existing session. Atomic read-modify-write.
 *  Pass `null` to remove the alias. Throws if `name` doesn't exist. */
export function setDisplayName(name: string, displayName: string | null): void {
  const metadata = readMetadata(name);
  if (!metadata) {
    throw new Error(`Session "${name}" not found.`);
  }
  if (displayName === null || displayName === "") {
    delete metadata.displayName;
  } else {
    metadata.displayName = displayName;
  }
  writeMetadata(name, metadata);
}

/** Update tags on an existing session. Performs an atomic read-modify-write. */
export function updateTags(
  name: string,
  updates: Record<string, string>,
  removals: string[] = [],
): void {
  const metadata = readMetadata(name);
  if (!metadata) {
    throw new Error(`Session "${name}" not found.`);
  }
  const tags = { ...(metadata.tags ?? {}) };
  for (const [k, v] of Object.entries(updates)) {
    tags[k] = v;
  }
  for (const k of removals) {
    delete tags[k];
  }
  if (Object.keys(tags).length > 0) {
    metadata.tags = tags;
  } else {
    delete metadata.tags;
  }
  writeMetadata(name, metadata);
}

export function readMetadata(name: string): SessionMetadata | null {
  try {
    const content = fs.readFileSync(getMetadataPath(name), "utf-8");
    return JSON.parse(content);
  } catch {
    return null;
  }
}

export async function listSessions(): Promise<SessionInfo[]> {
  ensureSessionDir();

  let entries: string[];
  try {
    entries = fs.readdirSync(getSessionDir());
  } catch {
    return [];
  }

  const sessions: SessionInfo[] = [];
  const seen = new Set<string>();

  // Find running sessions (have .sock files)
  const sockFiles = entries.filter((e) => e.endsWith(".sock"));
  for (const sockFile of sockFiles) {
    const name = sockFile.replace(/\.sock$/, "");
    seen.add(name);
    const socketPath = getSocketPath(name);
    const pid = readPid(name);
    const alive =
      pid !== null &&
      isProcessAlive(pid) &&
      (await isSocketReachable(socketPath));

    if (alive) {
      const metadata = readMetadata(name);
      // The daemon writes exit metadata before its cleanup delay, so a
      // reachable socket can briefly coexist with exitedAt being set.
      const status = metadata?.exitedAt ? "exited" : "running";
      sessions.push({ name, socketPath, pid, status, metadata });
    } else {
      // Process died — clean up socket/pid but keep metadata
      cleanupSocket(name);
    }
  }

  // Find dead sessions (have .json but no running process)
  const jsonFiles = entries.filter((e) => e.endsWith(".json"));
  for (const jsonFile of jsonFiles) {
    const name = jsonFile.replace(/\.json$/, "");
    if (seen.has(name)) continue; // already handled above

    const metadata = readMetadata(name);
    if (!metadata) {
      cleanupAll(name);
      continue;
    }

    // Auto-clean dead sessions older than 24h. For cleanly-exited sessions
    // this keys off exitedAt; for vanished sessions (no exit record written)
    // fall back to createdAt so they don't accumulate indefinitely. A session
    // with a missing daemon and a metadata file older than 24h is not coming
    // back regardless of why it died.
    const ageAnchor = metadata.exitedAt ?? metadata.createdAt;
    if (ageAnchor) {
      const anchoredAt = new Date(ageAnchor).getTime();
      if (Date.now() - anchoredAt > DEAD_SESSION_TTL_MS) {
        cleanupAll(name);
        continue;
      }
    }

    // Vanished = dead daemon with no exit record. SIGKILL / OOM / crash.
    const vanished =
      metadata.exitedAt == null && metadata.exitCode == null;

    sessions.push({
      name,
      socketPath: getSocketPath(name),
      pid: null,
      status: vanished ? "vanished" : "exited",
      metadata,
    });
  }

  return sessions;
}

/** Look up a session by either its stable `name` (immutable id) or its
 *  mutable `displayName` alias. Name match takes precedence over displayName
 *  match so the stable id always wins in case both happen to resolve. */
export async function getSession(ref: string): Promise<SessionInfo | null> {
  const sessions = await listSessions();
  const byName = sessions.find((s) => s.name === ref);
  if (byName) return byName;
  const byDisplay = sessions.find((s) => s.metadata?.displayName === ref);
  return byDisplay ?? null;
}

/** Return every reference (name or displayName) currently claimed by a live
 *  or exited session. Used for uniqueness checks at creation/rename time. */
export async function allRefs(): Promise<Set<string>> {
  const sessions = await listSessions();
  const refs = new Set<string>();
  for (const s of sessions) {
    refs.add(s.name);
    if (s.metadata?.displayName) refs.add(s.metadata.displayName);
  }
  return refs;
}

/** Remove all exited **and** vanished sessions. Returns the names of removed
 *  sessions. `dryRun: true` performs the same walk but doesn't delete — useful
 *  for preview UIs. */
export async function gc(opts: { dryRun?: boolean } = {}): Promise<string[]> {
  const sessions = await listSessions();
  const gone = sessions.filter((s) => isGone(s.status));
  if (!opts.dryRun) {
    for (const s of gone) cleanupAll(s.name);
  }
  return gone.map((s) => s.name);
}

/**
 * Layout tool tag keys follow `:l<pid>-<rand>` where the PID is the
 * pty-layout process that owns the view. When that process dies the
 * tag becomes an orphan. Same shape as the `:` reserved prefix
 * documented in `isReservedTagKey`.
 */
const ORPHAN_LAYOUT_TAG_RE = /^:l(\d+)-[a-z0-9]+$/;

export interface PrunedTagResult {
  name: string;
  removedKeys: string[];
}

/**
 * Walk **running** sessions and remove `:l<pid>-<rand>` tag keys whose
 * encoded PID no longer exists. Called by `pty gc` to clean up after a
 * pty-layout process that exited without clearing its tags.
 *
 * Returns a list of sessions that had at least one tag pruned, and
 * which keys were removed from each. `dryRun: true` performs the same
 * walk but doesn't call `updateTags`.
 */
export async function pruneOrphanLayoutTags(
  opts: { dryRun?: boolean } = {},
): Promise<PrunedTagResult[]> {
  const sessions = await listSessions();
  const results: PrunedTagResult[] = [];
  for (const s of sessions) {
    if (s.status !== "running") continue;
    const tags = s.metadata?.tags;
    if (!tags) continue;
    const toRemove: string[] = [];
    for (const key of Object.keys(tags)) {
      const match = ORPHAN_LAYOUT_TAG_RE.exec(key);
      if (!match) continue;
      const pid = parseInt(match[1], 10);
      if (!Number.isFinite(pid) || pid <= 0) {
        toRemove.push(key);
        continue;
      }
      if (!isProcessAlive(pid)) toRemove.push(key);
    }
    if (toRemove.length === 0) continue;
    if (!opts.dryRun) {
      try {
        updateTags(s.name, {}, toRemove);
      } catch {
        // Session metadata disappeared between listing and update — ignore.
        continue;
      }
    }
    results.push({ name: s.name, removedKeys: toRemove });
  }
  return results;
}

function readPid(name: string): number | null {
  try {
    const content = fs.readFileSync(getPidPath(name), "utf-8").trim();
    const pid = parseInt(content, 10);
    return isNaN(pid) ? null : pid;
  } catch {
    return null;
  }
}

function isProcessAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

function isSocketReachable(socketPath: string): Promise<boolean> {
  return new Promise((resolve) => {
    const socket = net.createConnection(socketPath);
    const timer = setTimeout(() => {
      socket.destroy();
      resolve(false);
    }, 500);
    socket.on("connect", () => {
      clearTimeout(timer);
      socket.destroy();
      resolve(true);
    });
    socket.on("error", () => {
      clearTimeout(timer);
      resolve(false);
    });
  });
}

/** Remove socket and pid files (but keep metadata). */
export function cleanupSocket(name: string): void {
  try {
    fs.unlinkSync(getSocketPath(name));
  } catch {}
  try {
    fs.unlinkSync(getPidPath(name));
  } catch {}
}

/** Remove everything including metadata. */
export function cleanupAll(name: string): void {
  cleanupSocket(name);
  try {
    fs.unlinkSync(getMetadataPath(name));
  } catch {}
  try {
    fs.unlinkSync(getEventsPath(name));
  } catch {}
  releaseLock(name);
}

function getLockPath(name: string): string {
  return path.join(getSessionDir(), `${name}.lock`);
}

/**
 * Acquire an exclusive lock for a session name. Prevents concurrent
 * `pty run` calls from racing to create the same session.
 * Returns true if acquired, false if another process holds it.
 *
 * BUG-2 fix: the whole acquisition is built on `open(O_CREAT|O_EXCL)` via
 * `openSync(..., "wx")`. Two racing processes can't both win because
 * `O_EXCL` is a kernel-level atomic create. When the lock looks stale, we
 * unlink it and retry the exclusive open: whichever process wins the
 * post-unlink open owns the lock; the other gets EEXIST and gives up.
 */
export function acquireLock(name: string): boolean {
  ensureSessionDir();
  const lockPath = getLockPath(name);

  const tryCreate = (): boolean => {
    try {
      const fd = fs.openSync(lockPath, "wx", 0o600);
      try {
        fs.writeSync(fd, process.pid.toString());
      } finally {
        fs.closeSync(fd);
      }
      return true;
    } catch (e: any) {
      if (e.code === "EEXIST") return false;
      throw e;
    }
  };

  if (tryCreate()) return true;

  // Lock file exists — inspect the holder.
  let holderAlive = false;
  try {
    const pid = parseInt(fs.readFileSync(lockPath, "utf-8").trim(), 10);
    if (!isNaN(pid)) {
      process.kill(pid, 0); // throws if process is dead
      holderAlive = true;
    }
  } catch {
    // Garbage content, unreadable, or holder dead → treat as stale.
  }

  if (holderAlive) return false;

  // Stale lock. Unlink and retry the exclusive create exactly once. If
  // another process is racing us to steal, only one wins the wx open; the
  // loser returns false instead of stomping on the winner's lock.
  try {
    fs.unlinkSync(lockPath);
  } catch (e: any) {
    // Someone else unlinked it first — that's fine, fall through to create.
    if (e.code !== "ENOENT") return false;
  }
  return tryCreate();
}

export function releaseLock(name: string): void {
  try {
    fs.unlinkSync(getLockPath(name));
  } catch {}
}

// Keep backward compat for server.ts close()
export { cleanupSocket as cleanup };
