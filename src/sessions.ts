import * as fs from "node:fs";
import * as fsp from "node:fs/promises";
import * as path from "node:path";
import * as os from "node:os";
import * as net from "node:net";
import { createHash } from "node:crypto";
// Circular import: events.ts imports getEventsPath/ensureSessionDir from
// this file. Cycle is safe — `appendEventSync` is only called at runtime
// from inside functions, never at module-init time.
import { appendEventSync } from "./events.ts";

export const DEFAULT_SESSION_DIR = path.join(os.homedir(), ".local", "state", "pty");

let hasWarnedLegacyRootEnv = false;
let hasWarnedRootMasksLegacy = false;

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
  const root = process.env.PTY_ROOT;
  const legacy = process.env.PTY_SESSION_DIR;
  if (root && root.length > 0) {
    // PTY_ROOT (canonical) wins. If a caller ALSO set the deprecated
    // PTY_SESSION_DIR — e.g. a test/scratch harness trying to isolate — it's
    // silently masked, so their sessions land under PTY_ROOT instead of the dir
    // they asked for. Warn once (unless silenced) so the masking is visible
    // rather than an invisible leak into the wrong registry.
    if (legacy && legacy.length > 0 && !hasWarnedRootMasksLegacy && !process.env.PTY_ROOT_LEGACY_SILENT) {
      hasWarnedRootMasksLegacy = true;
      process.stderr.write(
        `pty: both PTY_ROOT and PTY_SESSION_DIR are set — using PTY_ROOT (${root}); ` +
        `PTY_SESSION_DIR (${legacy}) is ignored (deprecated). For isolation, set PTY_ROOT.\n`
      );
    }
    return root;
  }
  if (legacy && legacy.length > 0) {
    if (!hasWarnedLegacyRootEnv && !process.env.PTY_ROOT_LEGACY_SILENT) {
      hasWarnedLegacyRootEnv = true;
      process.stderr.write(
        "pty: PTY_SESSION_DIR is deprecated; use PTY_ROOT (same shape, canonical name).\n"
      );
    }
    return legacy;
  }
  return DEFAULT_SESSION_DIR;
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

  // Find running sessions (have .sock files). A live session is destroyed here
  // ONLY on POSITIVE proof of death — a readable pid whose process is gone AND
  // an unreachable socket. A transiently-unreadable pid must NOT be mistaken
  // for a dead process: the daemon creates its .sock (listen) BEFORE it writes
  // its .pid, and the plain pidfile write can be caught mid-flight, so under
  // concurrent multi-agent load a `pty list` can momentarily read a null pid
  // for a perfectly healthy session. Reaping on that (the old behavior) deleted
  // a live daemon's socket/pid out from under it, making it invisible and
  // getting it GC'd + re-launched by consumers that reconcile on not-running.
  const sockFiles = entries.filter((e) => e.endsWith(".sock"));
  for (const sockFile of sockFiles) {
    const name = sockFile.replace(/\.sock$/, "");
    seen.add(name);
    const socketPath = getSocketPath(name);
    const pid = readPid(name);
    const pidAlive = pid !== null && isProcessAlive(pid);
    // A reachable control socket proves the daemon is alive INDEPENDENTLY of
    // whether we could read the pidfile this instant.
    const socketReachable = await isSocketReachable(socketPath);

    if (pidAlive || socketReachable) {
      // Alive: a live process, or a reachable control socket (busy/mid-startup
      // daemon whose pid we couldn't read). The daemon writes exit metadata
      // before its cleanup delay, so a reachable socket can briefly coexist
      // with exitedAt being set.
      const metadata = readMetadata(name);
      const status = metadata?.exitedAt ? "exited" : "running";
      sessions.push({ name, socketPath, pid, status, metadata });
    } else if (pid !== null) {
      // Positively dead: the pid read SUCCEEDED and its process is gone, and
      // the socket is unreachable. Safe to reap the stale socket/pid (keep
      // metadata so the session stays addressable as exited/vanished below).
      cleanupSocket(name);
    } else {
      // pid UNREADABLE and socket unreachable — we can prove neither life nor
      // death (most likely a daemon mid-startup, or a pidfile write that raced
      // our read under load). Do NOT destroy it; report it running defensively
      // (a .sock exists). A later read resolves the true state once the pidfile
      // settles or the socket comes up.
      const metadata = readMetadata(name);
      const status = metadata?.exitedAt ? "exited" : "running";
      sessions.push({ name, socketPath, pid, status, metadata });
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

/** Tag key that exempts a session from every form of dead-session reaping:
 *  the exit-time self-reap in the daemon AND `pty gc`'s sweep of exited
 *  non-permanent sessions. Set it when you want a session's metadata,
 *  `lastLines`, and events file to survive its own death so you can inspect
 *  them afterwards. Mirrors the `keep` field in the agent spec. */
export const KEEP_TAG = "keep";

/** Values that read as "no" for the `keep` tag; everything else reads as
 *  "yes". The CLI's tag grammar is strictly `key=value`, so the form an
 *  operator types is `--tag keep=true` / `pty tag <ref> keep=true`.
 *
 *  Two deliberate choices. Presence-with-any-other-value counts as yes, so
 *  a tool that writes `keep=1` or `keep=yes` straight into metadata (convoy
 *  translating the agent spec's `keep #true`) gets the safe answer without
 *  having to match our spelling — and the failure mode of a typo is
 *  retaining a session, not destroying one. And `keep=false` explicitly
 *  reads as no, so the exemption can be turned off in place rather than
 *  only by removing the key. */
const KEEP_FALSEY = new Set(["false", "0", "no", "off"]);

/** Returns `true` when `tags` asks for this session to be retained after
 *  death. Callers should read tags from the CURRENT on-disk metadata rather
 *  than from a spawn-time config snapshot — `pty tag <ref> keep=true` on a
 *  *running* session is exactly how an operator pins a session they are
 *  about to debug, and that must be honoured at exit. */
export function isKeepRequested(tags?: Record<string, string>): boolean {
  const raw = tags?.[KEEP_TAG];
  if (raw === undefined) return false;
  return !KEEP_FALSEY.has(raw.trim().toLowerCase());
}

/** Should the daemon remove its own registry entry as it shuts down?
 *
 *  Exit-time reaping is CONFIGURABLE. `defaultReap` is the config default (see
 *  `reapOnExitDefault` — the `PTY_REAP_ON_EXIT` network/global knob), and two
 *  per-session flags override it either way. Precedence, highest first:
 *
 *    1. `keep` — force PRESERVE. Always wins, even over `--ephemeral`, and also
 *       exempts the session from `pty gc`'s sweep. Retains a dead session's
 *       logs and scrollback for debugging past even a gc pass.
 *    2. `--ephemeral` — force REAP. Reaps as the session shuts down (the
 *       aggressive opt-in), even for a `strategy=permanent` session, so a
 *       caller that wants no trace left gets it regardless of the config
 *       default.
 *    3. `strategy=permanent` — force PRESERVE. Its supervisor / `pty gc`'s
 *       respawn step reconciles against the dead session's metadata, so
 *       reaping it would destroy the record the respawn needs.
 *    4. `defaultReap` — the config default when none of the above apply.
 *       `true` reaps a finished non-permanent session at exit; `false`
 *       PRESERVES it (its metadata lingers, peekable, until `pty gc`'s sweep
 *       reclaims it).
 *
 *  A session whose daemon was SIGKILL'd (`status=vanished`) never runs this
 *  code and is reclaimed by gc's sweep. */
export function shouldReapAtExit(
  tags: Record<string, string> | undefined,
  ephemeral: boolean,
  // Optional so existing 2-arg callers (relay/layout/supervisors read this to
  // answer "is this session exempt from reaping?") keep working AND get the
  // correct env-driven default without having to thread it themselves.
  defaultReap: boolean = reapOnExitDefault(),
): boolean {
  if (isKeepRequested(tags)) return false;
  if (ephemeral) return true;
  if (tags?.strategy === "permanent") return false;
  return defaultReap;
}

/** Resolve the config default for exit-time reaping from the environment.
 *
 *  `PTY_REAP_ON_EXIT` is the network/global config knob: the daemon reads its
 *  own env (which the launching network sets), so setting it fleet-wide
 *  configures the default for every session — mirroring the env-var config
 *  style pty already uses for `PTY_SHUTDOWN_DEADLINE_MS`. `false`/`0`/`no`/`off`
 *  → PRESERVE; unset or anything else → REAP (the shipped default). Per-session
 *  `keep` / `--ephemeral` override this default either way. */
export function reapOnExitDefault(
  env: NodeJS.ProcessEnv = process.env,
): boolean {
  const raw = env.PTY_REAP_ON_EXIT;
  if (raw === undefined) return true;
  return !KEEP_FALSEY.has(raw.trim().toLowerCase());
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
  /** Dead non-permanent sessions left in place because they carry the
   *  `keep` tag. Reported rather than silently skipped so an operator can
   *  see why `pty ls` still shows a dead session after a gc pass. */
  kept: string[];
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
  /** Permanent sessions the fast-fail cap flipped to `flapping` on this
   *  tick. Each entry records the counter at the moment of flip plus the
   *  effective `limit`/`window` in play. Sessions already flagged before
   *  this tick are silently skipped from the respawn loop and do NOT
   *  appear here — this bucket is transitions only. */
  flapped: { name: string; counter: number; limit: number; window: number }[];
  /** Permanent sessions skipped this tick because they are already
   *  `strategy.status=flapping`. Distinct from `flapped` (transitions),
   *  `respawnFailed` (attempted + failed), and `respawned` (attempted +
   *  succeeded). Consumers can render "N flapping" without having to
   *  read tags themselves. */
  flappingSkipped: string[];
}

/** Default fast-fail respawn cap window (seconds). A permanent session
 *  that exits within `DEFAULT_FAST_FAIL_WINDOW_SEC` of its previous gc
 *  respawn counts as a fast fail. Overridden by `opts.fastFailWindowSec`
 *  or the per-session `strategy.fast-fail-window` tag. */
export const DEFAULT_FAST_FAIL_WINDOW_SEC = 60;

/** Default fast-fail limit. `DEFAULT_FAST_FAIL_LIMIT` consecutive fast
 *  fails flip the session to `strategy.status=flapping` and stop future
 *  respawns until the operator intervenes (or the stored command changes,
 *  which auto-resets). Overridden by `opts.fastFailLimit` or the
 *  per-session `strategy.fast-fail-limit` tag. */
export const DEFAULT_FAST_FAIL_LIMIT = 3;

/** SHA-256 of a session's respawn command line, used to auto-reset the
 *  fast-fail counter when the operator edits the pty.toml (or otherwise
 *  changes the stored command). Kept short — the tag surface is user-
 *  facing, not a cryptographic identifier. */
function commandFingerprint(command: string, args: string[]): string {
  const h = createHash("sha256");
  h.update(command);
  h.update("\0");
  h.update(args.join("\0"));
  return h.digest("hex").slice(0, 16);
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
 *         A fast-fail cap prevents a crash-looping leaf from being
 *         respawned forever: `strategy.fast-fail-limit` consecutive
 *         respawns whose leaf exited within `strategy.fast-fail-window`
 *         seconds flip the session to `strategy.status=flapping` and
 *         skip it on subsequent ticks. Auto-reset when the stored command
 *         changes; manual reset via `pty tag <name> --rm strategy.status`.
 *    3.   Residual sweep: exited/vanished sessions that aren't permanent
 *         and aren't tagged `keep` get `cleanupAll`'d.
 *
 *  Step 3 is now a BACKSTOP rather than the primary path: a non-permanent
 *  session that runs to completion reaps itself as it shuts down (see
 *  `shouldReapAtExit`). It is NOT redundant, though — everything below
 *  still reaches it, so step 3 cannot simply be deleted:
 *
 *    - `pty kill`'d sessions. This is the most common residual case, and
 *      easy to miss: the exit path deliberately retains a session stopped
 *      from outside, but the child's `onExit` still wrote an exit record,
 *      so the session lands here as `status=exited` and gets swept. The
 *      retention is until the next sweep, not forever — `keep` is what
 *      makes it forever.
 *    - `status=vanished` sessions — the daemon was SIGKILL'd / OOM-killed
 *      / lost to a reboot, so no exit-time code ran at all. This is the
 *      case exit-time cleanup structurally *cannot* cover, since the
 *      process that would do the cleaning is the one that died. Note a
 *      reboot puts EVERY non-permanent session in this bucket at once.
 *    - sessions created before the exit-time policy existed, and any
 *      whose final `cleanupAll` lost a race with an external `pty rm`.
 *    - sessions demoted out of `strategy=permanent` after they died. */
export async function gc(
  opts: {
    dryRun?: boolean;
    idleDays?: number;
    fastFailWindowSec?: number;
    fastFailLimit?: number;
  } = {},
): Promise<GcResult> {
  const dryRun = !!opts.dryRun;
  const globalIdleDays = opts.idleDays;
  const globalFastFailWindow = opts.fastFailWindowSec;
  const globalFastFailLimit = opts.fastFailLimit;
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
  const flapped: GcResult["flapped"] = [];
  const flappingSkipped: GcResult["flappingSkipped"] = [];
  for (const s of afterStep15) {
    if (s.metadata?.tags?.strategy !== "permanent") continue;
    if (!isGone(s.status)) continue;
    const ptyfileReread = !!s.metadata?.tags?.ptyfile;

    // Fast-fail classifier: was the previous respawn a fast crash? What's
    // the running counter? Should we flip to flapping? Runs before any
    // spawn so a session at the limit boundary flaps this tick instead
    // of respawning one more time.
    const decision = classifyFlapping(
      s,
      new Date(),
      globalFastFailWindow,
      globalFastFailLimit,
    );

    if (decision.action === "skip-flapping") {
      flappingSkipped.push(s.name);
      continue;
    }

    if (dryRun) {
      if (decision.action === "flap-now") {
        flapped.push({
          name: s.name,
          counter: decision.counter,
          limit: decision.effectiveLimit,
          window: decision.effectiveWindow,
        });
        continue;
      }
      respawned.push({ name: s.name, ptyfileReread });
      continue;
    }

    if (decision.action === "flap-now") {
      // Persist the flapping mark to on-disk metadata so subsequent
      // ticks see it. We update the metadata file directly instead of
      // going through updateTags — the session's daemon is gone, there's
      // no live connection to notify, and cleanupAll ordering constraints
      // in respawnPermanent don't apply here (we're NOT respawning).
      try {
        const meta = readMetadata(s.name);
        if (meta) {
          const merged: Record<string, string> = {
            ...(meta.tags ?? {}),
            ...decision.newBookkeeping,
          };
          writeMetadata(s.name, { ...meta, tags: merged });
        }
      } catch {
        // Best-effort — if we can't persist the flag now, the next tick
        // will recompute the same decision and try again.
      }
      try {
        appendEventSync(s.name, {
          session: s.name,
          type: "session_flapping",
          ts: new Date().toISOString(),
          counter: decision.counter,
          limit: decision.effectiveLimit,
          window: decision.effectiveWindow,
        });
      } catch {}
      flapped.push({
        name: s.name,
        counter: decision.counter,
        limit: decision.effectiveLimit,
        window: decision.effectiveWindow,
      });
      continue;
    }

    try {
      await respawnPermanent(s.name, s.metadata!, decision.newBookkeeping);
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
  const kept: string[] = [];
  for (const s of finalList) {
    if (!isGone(s.status)) continue;
    if (s.metadata?.tags?.strategy === "permanent") continue;
    // `keep` must mean the same thing to both reapers. The daemon's
    // exit-time cleanup already honours it; if gc did not, a `keep`
    // session would merely survive its own exit only to be swept moments
    // later by the next tick — which is not "keep" in any useful sense.
    if (isKeepRequested(s.metadata?.tags)) {
      kept.push(s.name);
      continue;
    }
    if (!dryRun) cleanupAll(s.name);
    removed.push(s.name);
  }

  return {
    removed,
    kept,
    killedOrphanChildren,
    abandoned,
    respawned,
    respawnFailed,
    flapped,
    flappingSkipped,
  };
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

/** Decide whether a `strategy=permanent` session that's exited/vanished
 *  should be respawned, marked flapping, or silently skipped because
 *  it's already flapping. Reads three bookkeeping tags from the session:
 *    - `strategy.last-respawn-at` (ISO ts): when gc last respawned it
 *    - `strategy.consecutive-fast-fails` (int): running fast-fail counter
 *    - `strategy.command-hash` (16-char hex): command fingerprint at last
 *      respawn. If the current fingerprint differs, the operator edited
 *      the pty.toml (or otherwise changed the command); reset the
 *      counter and clear any stale `strategy.status=flapping`.
 *
 *  Effective window/limit resolution:
 *    per-session tag (strategy.fast-fail-window / -limit)
 *    → global opt (CLI --fast-fail-window / --fast-fail-limit)
 *    → DEFAULT_FAST_FAIL_WINDOW_SEC / DEFAULT_FAST_FAIL_LIMIT.
 *
 *  Returned `newBookkeeping` MUST be merged onto the session's tags map
 *  before/instead of respawn. The `flap-now` action never respawns; the
 *  `respawn` action does; the `skip-flapping` action skips entirely. */
interface FlappingDecision {
  action: "respawn" | "flap-now" | "skip-flapping";
  effectiveWindow: number;
  effectiveLimit: number;
  /** Fast-fail counter after this tick's classification. Only meaningful
   *  for `respawn` (stamped on the session) and `flap-now` (the counter
   *  that crossed the threshold, surfaced in the event payload). */
  counter: number;
  /** Tag deltas to persist. Empty for `skip-flapping`. For `respawn`,
   *  carries the fresh timestamp, counter, and command hash. For
   *  `flap-now`, adds `strategy.status=flapping` on top. */
  newBookkeeping: Record<string, string>;
}

function classifyFlapping(
  s: SessionInfo,
  now: Date,
  globalWindowSec: number | undefined,
  globalLimit: number | undefined,
): FlappingDecision {
  const tags = s.metadata?.tags ?? {};

  const tagWindow = parseInt(tags["strategy.fast-fail-window"] ?? "", 10);
  const effectiveWindow = Number.isFinite(tagWindow) && tagWindow > 0
    ? tagWindow
    : (globalWindowSec !== undefined && globalWindowSec > 0
      ? globalWindowSec
      : DEFAULT_FAST_FAIL_WINDOW_SEC);

  const tagLimit = parseInt(tags["strategy.fast-fail-limit"] ?? "", 10);
  const effectiveLimit = Number.isFinite(tagLimit) && tagLimit > 0
    ? tagLimit
    : (globalLimit !== undefined && globalLimit > 0
      ? globalLimit
      : DEFAULT_FAST_FAIL_LIMIT);

  const command = s.metadata?.command ?? "";
  const args = s.metadata?.args ?? [];
  const currentHash = commandFingerprint(command, args);
  const storedHash = tags["strategy.command-hash"];
  const commandChanged = storedHash !== undefined && storedHash !== currentHash;

  // Command change wins over an existing flapping mark: the operator has
  // edited the pty.toml (or manually mutated the command), so give it a
  // fresh chance. `strategy.status` clears; counter resets to 0.
  if (tags["strategy.status"] === "flapping" && !commandChanged) {
    return {
      action: "skip-flapping",
      effectiveWindow,
      effectiveLimit,
      counter: parseInt(tags["strategy.consecutive-fast-fails"] ?? "0", 10) || 0,
      newBookkeeping: {},
    };
  }

  // Was the previous respawn a fast fail? Compare the exit timestamp
  // against the last-respawn stamp; anything under `window` seconds is
  // fast. If no prior stamp exists (never respawned by gc) or the exit
  // is missing (vanished session), treat as slow — the counter resets.
  const lastRespawnAt = tags["strategy.last-respawn-at"];
  const exitedAt = s.metadata?.exitedAt;
  let liveMs: number | null = null;
  if (lastRespawnAt && exitedAt) {
    const lr = Date.parse(lastRespawnAt);
    const ex = Date.parse(exitedAt);
    if (Number.isFinite(lr) && Number.isFinite(ex)) liveMs = ex - lr;
  }
  const wasFastFail = liveMs !== null && liveMs >= 0 && liveMs < effectiveWindow * 1000;

  const prevCounter = parseInt(tags["strategy.consecutive-fast-fails"] ?? "0", 10) || 0;
  const nextCounter = commandChanged ? 0 : (wasFastFail ? prevCounter + 1 : 0);

  if (nextCounter >= effectiveLimit) {
    // Threshold crossed. Mark flapping, don't respawn. The counter goes
    // into the tags at its final value so subsequent listers can see how
    // deep the streak went.
    const bookkeeping: Record<string, string> = {
      "strategy.status": "flapping",
      "strategy.consecutive-fast-fails": String(nextCounter),
      "strategy.command-hash": currentHash,
    };
    if (lastRespawnAt) bookkeeping["strategy.last-respawn-at"] = lastRespawnAt;
    return {
      action: "flap-now",
      effectiveWindow,
      effectiveLimit,
      counter: nextCounter,
      newBookkeeping: bookkeeping,
    };
  }

  // Respawn. Stamp fresh bookkeeping. If we're clearing a stale flap
  // mark from a command change, drop `strategy.status` explicitly by
  // storing an empty string — updateTags treats that as a remove.
  const bookkeeping: Record<string, string> = {
    "strategy.last-respawn-at": now.toISOString(),
    "strategy.consecutive-fast-fails": String(nextCounter),
    "strategy.command-hash": currentHash,
  };
  return {
    action: "respawn",
    effectiveWindow,
    effectiveLimit,
    counter: nextCounter,
    newBookkeeping: bookkeeping,
  };
}

/** Restart a `strategy=permanent` session whose daemon is gone. If the
 *  session was toml-managed (`ptyfile` + `ptyfile.session` tags), re-read
 *  the pty.toml so the new daemon picks up command/env edits since the
 *  last spawn. On any read error fall back to the stored metadata
 *  verbatim (last-known-good) so a temporarily-missing toml doesn't
 *  prevent restart.
 *
 *  `bookkeepingOverlay` (optional) carries pty-internal tags that must
 *  survive the pty.toml re-read: gc backoff state (`strategy.last-*`,
 *  `strategy.command-hash`, `strategy.consecutive-fast-fails`). Passed
 *  by `gc()` STEP-2; ignored by other callers.
 *
 *  Lazy-imports `spawn.ts` so the `sessions.ts ↔ spawn.ts` cycle doesn't
 *  bite at module-init time. After spawn, appends a `session_respawn`
 *  event to the session's event log so consumers see the restart. */
async function respawnPermanent(
  name: string,
  metadata: SessionMetadata,
  bookkeepingOverlay: Record<string, string> = {},
): Promise<void> {
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
        cwd = sessDef.cwd ?? ptyFile.dir;
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

  // Merge gc's backoff bookkeeping last so it survives the pty.toml
  // overlay above. Callers pass an empty overlay when they aren't gc.
  // If a command change is clearing a stale flap mark, the caller
  // omits `strategy.status` from the overlay; we also clear any
  // existing flag on the merged map so a rebuilt tags dict doesn't
  // silently carry it forward from the previous metadata.
  tags = { ...(tags ?? {}), ...bookkeepingOverlay };
  if (bookkeepingOverlay["strategy.status"] === undefined) {
    delete tags["strategy.status"];
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

export function isProcessAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

/** Poll until `pid` is gone (or `timeoutMs` elapses). Returns true if the
 *  process exited within the budget, false if it was still alive at timeout.
 *  Used by `pty kill` to wait for the daemon's shutdown (which re-flushes exit
 *  metadata) to finish before returning, so a following `pty rm` can't race it. */
export async function waitForProcessExit(pid: number, timeoutMs: number): Promise<boolean> {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    if (!isProcessAlive(pid)) return true;
    await new Promise((r) => setTimeout(r, 50));
  }
  return !isProcessAlive(pid);
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
