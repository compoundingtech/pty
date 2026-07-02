import * as fs from "node:fs";
import * as fsp from "node:fs/promises";
import * as path from "node:path";
import * as os from "node:os";
import * as net from "node:net";
// Circular import: events.ts imports getEventsPath/ensureSessionDir from
// this file. Cycle is safe — `appendEventSync` is only called at runtime
// from inside functions, never at module-init time.
import { appendEventSync } from "./events.ts";

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

/** Permissive validator for display labels. Allowed: anything printable
 *  including spaces and punctuation, ≤ 500 chars. Rejected: empty, NUL,
 *  slashes (would confuse path-shaped UIs), backslashes, newlines, other
 *  control characters. Length cap is a sanity limit, not a kernel limit —
 *  display labels live in metadata.json, not in the socket path. */
export function validateDisplayName(name: string): void {
  if (!name || name.length === 0) {
    throw new Error("Display name cannot be empty.");
  }
  if (name.length > 500) {
    throw new Error("Display name too long (max 500 characters).");
  }
  if (/[\0\/\\\n\r\t\x00-\x1f\x7f]/.test(name)) {
    throw new Error(
      `Invalid display name. Slashes, backslashes, newlines, and control characters are not allowed.`
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

// PUBLIC FORMAT — this is the on-disk shape of `<name>.json`. Any change
// to fields here (add / rename / remove / type change) MUST be reflected in
// `docs/disk-layout.md` and called out under `### Storage format` in the
// next CHANGELOG entry. A smoke test (`tests/disk-layout-docs.test.ts`)
// asserts every field name on this interface appears in the docs.
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
  /** Free-form per-session data bag. Separate from `tags` (which are
   *  string-valued, filterable, and rendered in `pty list`) — `state`
   *  holds complex JSON values a session or a consumer wants to track
   *  (web server port, agent turn count, cached result, etc.). Mutated
   *  via `setState` / `deleteState`, which emit `state.set` /
   *  `state.delete` events automatically. Keep to small-to-medium JSON
   *  — the metadata file is rewritten on every update. */
  state?: Record<string, unknown>;
  /** Optional human-friendly alias for the session. Mutable via `pty rename`.
   *  The immutable stable id is always `SessionInfo.name`. Most code should
   *  keep using `name`; `displayName` is purely for presentation and as an
   *  additional lookup key alongside `name`. */
  displayName?: string;
  /** ISO 8601 timestamp of the last non-readonly client ATTACH. Written by
   *  the daemon on every attach. Used by `pty gc --idle-days N` (and the
   *  per-session `strategy.idle-days=N` tag) to decide whether a permanent
   *  session has been abandoned. Absent on sessions that have never had a
   *  client attach — those are excluded from idle-reap (a session that
   *  was just spawned but not yet attached to isn't "idle"). */
  lastAttachAt?: string;
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

/** Atomic file publish: write to a unique per-writer tmp file in the
 *  same directory, then rename over the target. Readers see either
 *  the old file or the new one, never a half-written intermediate.
 *  Concurrent writers do NOT coordinate — the last rename wins — but
 *  they can't corrupt each other's tmp files because each writer uses
 *  its own unique path. Same-filesystem rename on POSIX is atomic. */
export function atomicWriteFileSync(target: string, content: string): void {
  const tmp = `${target}.tmp.${process.pid}.${randomHex(8)}`;
  try {
    fs.writeFileSync(tmp, content);
    fs.renameSync(tmp, target);
  } catch (e) {
    // If writeFileSync or renameSync fails, try to clean up the tmp.
    // Silent — the original target is still intact either way.
    try { fs.unlinkSync(tmp); } catch {}
    throw e;
  }
}

/** Async twin of `atomicWriteFileSync` for code paths that are already
 *  async (EventWriter, etc). Same semantics, same guarantees. */
export async function atomicWriteFile(target: string, content: string): Promise<void> {
  const tmp = `${target}.tmp.${process.pid}.${randomHex(8)}`;
  try {
    await fsp.writeFile(tmp, content);
    await fsp.rename(tmp, target);
  } catch (e) {
    try { await fsp.unlink(tmp); } catch {}
    throw e;
  }
}

function randomHex(bytes: number): string {
  // Small inline hex generator — keeping sessions.ts free of a `node:crypto`
  // import for this tiny helper. Not cryptographic; just needs low
  // collision probability across concurrent writers in the same dir.
  let out = "";
  for (let i = 0; i < bytes; i++) out += Math.floor(Math.random() * 256).toString(16).padStart(2, "0");
  return out;
}

export function writeMetadata(name: string, metadata: SessionMetadata): void {
  ensureSessionDir();
  atomicWriteFileSync(getMetadataPath(name), JSON.stringify(metadata, null, 2));
}

/** Set or clear the displayName on an existing session. Atomic read-modify-write.
 *  Pass `null` to remove the alias. Throws if `name` doesn't exist. Emits a
 *  `display_name_change` event when (and only when) the value actually
 *  changed — no-op renames don't ping downstream watchers. */
export function setDisplayName(name: string, displayName: string | null): void {
  const metadata = readMetadata(name);
  if (!metadata) {
    throw new Error(`Session "${name}" not found.`);
  }
  const previous = metadata.displayName ?? null;
  const next = displayName === null || displayName === "" ? null : displayName;
  if (previous === next) return; // no-op write + no-op event

  if (next === null) {
    delete metadata.displayName;
  } else {
    metadata.displayName = next;
  }
  writeMetadata(name, metadata);
  appendEventSync(name, {
    session: name,
    type: "display_name_change",
    ts: new Date().toISOString(),
    previous,
    value: next,
  });
}

/** Update tags on an existing session. Performs an atomic read-modify-write.
 *  Emits a `tags_change` event carrying snapshots of the full previous and
 *  new tag maps when the effective tags change. No-op updates (e.g. setting
 *  a key to the same value, removing a key that isn't there) don't emit. */
export function updateTags(
  name: string,
  updates: Record<string, string>,
  removals: string[] = [],
): void {
  const metadata = readMetadata(name);
  if (!metadata) {
    throw new Error(`Session "${name}" not found.`);
  }
  const previous = { ...(metadata.tags ?? {}) };
  const tags = { ...previous };
  for (const [k, v] of Object.entries(updates)) {
    tags[k] = v;
  }
  for (const k of removals) {
    delete tags[k];
  }
  if (tagsEqual(previous, tags)) return; // no-op write + no-op event

  if (Object.keys(tags).length > 0) {
    metadata.tags = tags;
  } else {
    delete metadata.tags;
  }
  writeMetadata(name, metadata);
  appendEventSync(name, {
    session: name,
    type: "tags_change",
    ts: new Date().toISOString(),
    previous,
    value: tags,
  });
}

function tagsEqual(a: Record<string, string>, b: Record<string, string>): boolean {
  const aKeys = Object.keys(a);
  const bKeys = Object.keys(b);
  if (aKeys.length !== bKeys.length) return false;
  for (const k of aKeys) {
    if (!Object.prototype.hasOwnProperty.call(b, k) || a[k] !== b[k]) return false;
  }
  return true;
}

/** Read a session's state bag. Returns an empty object when the session
 *  has no state. Throws if the session doesn't exist. */
export function getState(name: string): Record<string, unknown> {
  const metadata = readMetadata(name);
  if (!metadata) throw new Error(`Session "${name}" not found.`);
  return { ...(metadata.state ?? {}) };
}

/** Read a single state key. Returns `undefined` if the key isn't set.
 *  Uses own-property lookup so prototype names like `toString` /
 *  `hasOwnProperty` return `undefined` instead of leaking inherited
 *  methods. */
export function getStateKey(name: string, key: string): unknown {
  const metadata = readMetadata(name);
  if (!metadata) throw new Error(`Session "${name}" not found.`);
  const state = metadata.state;
  if (!state || !Object.prototype.hasOwnProperty.call(state, key)) return undefined;
  return state[key];
}

/** Set a key on the state bag. Atomic read-modify-write of the metadata
 *  file. Emits a `state.set` event on every successful write — callers
 *  that want the full reactive signal (pty-layout, activity viewers,
 *  etc.) get it whether they use the CLI or the programmatic API. */
export function setState(name: string, key: string, value: unknown): void {
  const metadata = readMetadata(name);
  if (!metadata) throw new Error(`Session "${name}" not found.`);
  const state = { ...(metadata.state ?? {}) };
  state[key] = value;
  metadata.state = state;
  writeMetadata(name, metadata);
  appendEventSync(name, {
    session: name,
    type: "state.set",
    ts: new Date().toISOString(),
    key,
    value,
  });
}

/** Delete a key from the state bag. Returns `true` when the key existed and
 *  was removed, `false` when the key wasn't set (no write performed).
 *  Emits a `state.delete` event only when something was actually removed,
 *  so a delete on a missing key is a true no-op (no ghost event).
 *  Uses own-property lookup — inherited names like `toString` never match. */
export function deleteState(name: string, key: string): boolean {
  const metadata = readMetadata(name);
  if (!metadata) throw new Error(`Session "${name}" not found.`);
  if (!metadata.state || !Object.prototype.hasOwnProperty.call(metadata.state, key)) return false;
  const state = { ...metadata.state };
  delete state[key];
  if (Object.keys(state).length > 0) {
    metadata.state = state;
  } else {
    delete metadata.state;
  }
  writeMetadata(name, metadata);
  appendEventSync(name, {
    session: name,
    type: "state.delete",
    ts: new Date().toISOString(),
    key,
  });
  return true;
}

/** List every key currently set on the state bag. */
export function listStateKeys(name: string): string[] {
  const metadata = readMetadata(name);
  if (!metadata) throw new Error(`Session "${name}" not found.`);
  return Object.keys(metadata.state ?? {});
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
    } else if (pid !== null && isProcessAlive(pid)) {
      // Pid is still alive but the socket isn't reachable right now (busy
      // daemon, transient EAGAIN, race with a service restart). Keep the
      // socket on disk and report the session as running — deleting the
      // socket would render the still-alive daemon permanently invisible.
      const metadata = readMetadata(name);
      const status = metadata?.exitedAt ? "exited" : "running";
      sessions.push({ name, socketPath, pid, status, metadata });
    } else {
      // Process really died — clean up socket/pid but keep metadata
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
    // back regardless of why it died — *unless* the recorded pid is still
    // alive, in which case the daemon outlived its socket and we must keep
    // metadata around so the session stays addressable.
    const ageAnchor = metadata.exitedAt ?? metadata.createdAt;
    if (ageAnchor) {
      const anchoredAt = new Date(ageAnchor).getTime();
      if (Date.now() - anchoredAt > DEAD_SESSION_TTL_MS) {
        const pid = readPid(name);
        if (pid === null || !isProcessAlive(pid)) {
          cleanupAll(name);
          continue;
        }
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

/** Result of a `gc()` reconciliation pass. Five buckets correspond to the
 *  reconciliation steps: orphan-kill (step 1), abandoned-reap (step 1.5),
 *  permanent respawn success / failure (step 2), and the sweep of exited
 *  non-permanent sessions (step 3 — the historic `gc()` behavior). */
export interface GcResult {
  /** Names of exited/vanished non-permanent sessions whose metadata was
   *  removed. Empty under `dryRun: true` callers should treat the same
   *  list as the preview. */
  removed: string[];
  /** Children killed because their `parent=` referent is dead or missing. */
  killedOrphanChildren: { name: string; parent: string; reason: "missing" | "dead" }[];
  /** Live `strategy=permanent` sessions reaped because they've been
   *  detected as abandoned. `cwd-gone` fires on-by-default when the
   *  session's cwd no longer resolves; `idle` fires only when an
   *  `idleDays` threshold is set (via CLI flag or per-session tag)
   *  and `lastAttachAt` is older than that threshold. */
  abandoned: { name: string; reason: "cwd-gone" | "idle"; idleDays?: number }[];
  /** Permanent sessions respawned this pass. `ptyfileReread` indicates
   *  whether the spawn used a fresh `pty.toml` read (when the session
   *  carries `ptyfile` + `ptyfile.session` tags) or its stored metadata. */
  respawned: { name: string; ptyfileReread: boolean }[];
  /** Permanent sessions where respawn was attempted but failed (e.g. the
   *  binary is on an unmounted volume). Cron interval is the rate limit;
   *  next tick tries again. */
  respawnFailed: { name: string; error: string }[];
}

/** Reconciliation pass driven by `pty gc`. Stateless: every invocation
 *  re-derives intent from on-disk metadata. Four steps run in order:
 *
 *    1.   Orphan-kill: children with a `parent=<name>` tag whose parent's
 *         metadata is gone OR whose parent's pid isn't alive get SIGTERM'd
 *         and `cleanupAll`'d. Runs first so a permanent child whose parent
 *         has died isn't immediately respawned by step 2.
 *    1.5. Abandoned-reap: live `strategy=permanent` sessions whose recorded
 *         cwd is gone from disk are SIGTERM'd + `cleanupAll`'d + get a
 *         `session_abandoned` event. When `opts.idleDays` is set OR the
 *         session carries a `strategy.idle-days=N` tag, sessions whose
 *         `lastAttachAt` is older than that threshold are also reaped
 *         with reason `idle`. Runs before step 2 so a session reaped for
 *         abandonment isn't immediately respawned by permanent-restart.
 *    2.   Permanent respawn: every `strategy=permanent` session that's
 *         exited/vanished is respawned via `spawnDaemon` (lazy-imported to
 *         avoid the `sessions ↔ spawn` cycle). Sessions with `ptyfile` +
 *         `ptyfile.session` tags re-read the toml to pick up any edits.
 *    3.   Existing sweep: the historic behavior — exited/vanished sessions
 *         that aren't permanent get `cleanupAll`'d. */
export async function gc(opts: { dryRun?: boolean; idleDays?: number } = {}): Promise<GcResult> {
  const dryRun = !!opts.dryRun;
  const globalIdleDays = opts.idleDays;
  // First call to `listSessions` is intentionally throwaway — it has a
  // side effect (`cleanupSocket`) on sessions whose daemon SIGKILL'd
  // without writing an exit record, and those sessions are then *missing*
  // from the returned array (their entry is dropped because `seen` set
  // contained the name but the alive checks failed). A second call sees
  // them via the `.json` files loop as `status=vanished`. Without this
  // priming pass, step 1's orphan-kill misses vanished sessions whose
  // sockets were still on disk when gc started.
  await listSessions();
  const initial = await listSessions();

  // STEP 1: orphan-children. Sort by name so cycles (A→B, B→A) resolve
  // deterministically — whichever name sorts first wins this tick; the
  // loser dies; on the next tick the winner has no live parent either
  // and dies too. No cycle detection needed.
  const killedOrphanChildren: GcResult["killedOrphanChildren"] = [];
  const withParent = initial
    .filter((s) => s.metadata?.tags?.parent)
    .sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
  for (const s of withParent) {
    const parentRef = s.metadata!.tags!.parent;
    const parentMeta = readMetadata(parentRef);
    const parentPid = parentMeta ? readPid(parentRef) : null;
    const parentAlive = parentMeta != null && parentPid !== null && isProcessAlive(parentPid);
    if (parentAlive) continue;
    const reason: "missing" | "dead" = parentMeta ? "dead" : "missing";
    if (!dryRun) {
      if (s.status === "running" && s.pid != null) {
        // SIGTERM the live daemon, then wait briefly for it to exit so
        // its shutdown handler doesn't race our cleanupAll by writing
        // metadata back to disk after we've removed it. We poll the
        // pid (up to ~1s) and fall through whether or not the daemon
        // shut down in time — cleanupAll wipes whatever remains.
        try { process.kill(s.pid, "SIGTERM"); } catch {}
        const deadline = Date.now() + 1000;
        while (Date.now() < deadline) {
          if (!isProcessAlive(s.pid)) break;
          await new Promise((r) => setTimeout(r, 25));
        }
      }
      cleanupAll(s.name);
    }
    killedOrphanChildren.push({ name: s.name, parent: parentRef, reason });
  }

  // STEP 1.5: abandoned-reap. Live permanent sessions whose cwd is gone,
  // or (opt-in) whose lastAttachAt is older than the idle threshold, get
  // SIGTERM'd + cleaned up + emit `session_abandoned`. Runs before step 2
  // so the reap isn't racing an immediate respawn on the same tick.
  const afterStep1 = dryRun ? initial : await listSessions();
  const abandoned: GcResult["abandoned"] = [];
  for (const s of afterStep1) {
    if (s.metadata?.tags?.strategy !== "permanent") continue;
    const decision = classifyAbandoned(s, globalIdleDays);
    if (!decision) continue;

    if (!dryRun) {
      if (s.status === "running" && s.pid != null) {
        try { process.kill(s.pid, "SIGTERM"); } catch {}
        const deadline = Date.now() + 1000;
        while (Date.now() < deadline) {
          if (!isProcessAlive(s.pid)) break;
          await new Promise((r) => setTimeout(r, 25));
        }
      }
      // Emit the abandoned event BEFORE cleanupAll — cleanupAll unlinks
      // the events file, and appendEventSync into a nonexistent file
      // would just create a stub with a single event and leave orphaned
      // JSONL on disk. Ordering: event → cleanup → gone.
      try {
        appendEventSync(s.name, {
          session: s.name,
          type: "session_abandoned",
          ts: new Date().toISOString(),
          reason: decision.reason,
          ...(decision.idleDays !== undefined ? { idleDays: decision.idleDays } : {}),
        });
      } catch {}
      cleanupAll(s.name);
    }
    abandoned.push({
      name: s.name,
      reason: decision.reason,
      ...(decision.idleDays !== undefined ? { idleDays: decision.idleDays } : {}),
    });
  }

  // STEP 2: permanent respawn. Re-list since steps 1 and 1.5 may have
  // removed some metadata. In dryRun mode we filter out anything step
  // 1.5 would have reaped so the preview reflects the same intent.
  const afterStep15 = dryRun
    ? initial.filter((s) => !abandoned.some((a) => a.name === s.name))
    : await listSessions();
  const respawned: GcResult["respawned"] = [];
  const respawnFailed: GcResult["respawnFailed"] = [];
  for (const s of afterStep15) {
    if (s.metadata?.tags?.strategy !== "permanent") continue;
    if (!isGone(s.status)) continue;
    const ptyfileReread = !!s.metadata?.tags?.ptyfile;
    if (dryRun) {
      respawned.push({ name: s.name, ptyfileReread });
      continue;
    }
    try {
      await respawnPermanent(s.name, s.metadata!);
      respawned.push({ name: s.name, ptyfileReread });
    } catch (err: any) {
      respawnFailed.push({ name: s.name, error: err?.message ?? String(err) });
    }
  }

  // STEP 3: historic sweep. Exited/vanished non-permanent sessions get
  // their metadata removed. Permanent sessions are handled by step 2 —
  // if their respawn succeeded they're back to `running` and skipped;
  // if it failed we leave the metadata around so the next tick can try
  // again.
  const finalList = dryRun ? initial : await listSessions();
  const removed: string[] = [];
  for (const s of finalList) {
    if (!isGone(s.status)) continue;
    if (s.metadata?.tags?.strategy === "permanent") continue;
    if (!dryRun) cleanupAll(s.name);
    removed.push(s.name);
  }

  return { removed, killedOrphanChildren, abandoned, respawned, respawnFailed };
}

/** Decide whether a permanent session is abandoned. Order:
 *
 *    1. cwd-gone (`fs.statSync` throws `ENOENT` on `metadata.cwd`) —
 *       strong low-false-positive signal, on-by-default. Escape hatch:
 *       `strategy.abandon-if-cwd-gone=false` tag opts a session out.
 *    2. idle (only if `idleDays` is resolved from CLI or per-session
 *       `strategy.idle-days=N` tag) — requires `lastAttachAt` to be set
 *       AND to be older than the threshold.
 *
 *  Returns `null` when the session is NOT abandoned. A cwd-gone verdict
 *  always wins over an idle verdict — the session is abandoned regardless
 *  of attach recency once the cwd is gone. */
function classifyAbandoned(
  s: SessionInfo,
  globalIdleDays?: number,
): { reason: "cwd-gone" | "idle"; idleDays?: number } | null {
  const cwd = s.metadata?.cwd;
  const optOutCwd = s.metadata?.tags?.["strategy.abandon-if-cwd-gone"] === "false";
  if (cwd && !optOutCwd) {
    let cwdGone = false;
    try {
      fs.statSync(cwd);
    } catch (err: any) {
      if (err?.code === "ENOENT") cwdGone = true;
    }
    if (cwdGone) return { reason: "cwd-gone" };
  }

  const tagIdle = s.metadata?.tags?.["strategy.idle-days"];
  const perSessionIdleDays = tagIdle !== undefined ? parseInt(tagIdle, 10) : NaN;
  const effectiveIdleDays = Number.isFinite(perSessionIdleDays) && perSessionIdleDays > 0
    ? perSessionIdleDays
    : (globalIdleDays !== undefined && globalIdleDays > 0 ? globalIdleDays : undefined);
  if (effectiveIdleDays === undefined) return null;

  const lastAttach = s.metadata?.lastAttachAt;
  if (!lastAttach) return null;

  const lastAttachMs = Date.parse(lastAttach);
  if (!Number.isFinite(lastAttachMs)) return null;
  const ageDays = Math.floor((Date.now() - lastAttachMs) / (1000 * 60 * 60 * 24));
  if (ageDays < effectiveIdleDays) return null;
  return { reason: "idle", idleDays: ageDays };
}

/** Restart a `strategy=permanent` session whose daemon is gone. If the
 *  session was toml-managed (`ptyfile` + `ptyfile.session` tags), re-read
 *  the pty.toml so the new daemon picks up command/env edits since the
 *  last spawn. On any read error fall back to the stored metadata
 *  verbatim (last-known-good) so a temporarily-missing toml doesn't
 *  prevent restart.
 *
 *  Lazy-imports `spawn.ts` so the `sessions.ts ↔ spawn.ts` cycle doesn't
 *  bite at module-init time. After spawn, appends a `session_respawn`
 *  event to the session's event log so consumers see the restart. */
async function respawnPermanent(name: string, metadata: SessionMetadata): Promise<void> {
  let command = metadata.command;
  let args = metadata.args;
  let displayCommand = metadata.displayCommand;
  let cwd = metadata.cwd;
  let tags: Record<string, string> | undefined = metadata.tags;
  const displayName = metadata.displayName;

  const ptyfilePath = metadata.tags?.ptyfile;
  const ptyfileSession = metadata.tags?.["ptyfile.session"];
  if (ptyfilePath && ptyfileSession) {
    try {
      const { readPtyFile, commandWithEnvExports } = await import("./ptyfile.ts");
      const dir = path.dirname(ptyfilePath);
      const ptyFile = readPtyFile(dir);
      const sessDef = ptyFile.sessions.find((s) => s.shortName === ptyfileSession);
      if (sessDef) {
        command = "/bin/sh";
        args = ["-c", commandWithEnvExports(sessDef)];
        displayCommand = sessDef.command;
        cwd = ptyFile.dir;
        tags = {
          ...sessDef.tags,
          ptyfile: ptyfilePath,
          "ptyfile.session": ptyfileSession,
        };
      }
    } catch {
      // pty.toml unreadable (volume not mounted yet, file deleted, parse
      // error). Fall back to stored metadata — better to respawn with
      // last-known-good than to give up.
    }
  }

  // Wipe stale socket/pid/events before respawn so spawnDaemon doesn't
  // trip over leftovers from the dead daemon. Metadata is recreated by
  // spawnDaemon.
  cleanupAll(name);

  const { spawnDaemon } = await import("./spawn.ts");
  await spawnDaemon({
    name, command, args, displayCommand, cwd, tags,
    ...(displayName ? { displayName } : {}),
  });

  // Best-effort event; respawn already succeeded if we got here.
  try {
    appendEventSync(name, {
      session: name,
      type: "session_respawn",
      ts: new Date().toISOString(),
    });
  } catch {}
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
