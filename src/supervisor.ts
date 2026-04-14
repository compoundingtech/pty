import * as fs from "node:fs";
import * as path from "node:path";
import {
  getSessionDir, ensureSessionDir, readMetadata, cleanupAll,
  acquireLock, releaseLock, updateTags,
  type SessionMetadata,
} from "./sessions.ts";
import { spawnDaemon } from "./spawn.ts";
import { readPtyFile } from "./ptyfile.ts";
import { EventWriter, EventType, type EventRecord } from "./events.ts";

/** Supervisor state lives in its own subdirectory to avoid polluting the session dir. */
export function getSupervisorDir(): string {
  const dir = path.join(getSessionDir(), "supervisor");
  fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
  return dir;
}

const MAX_RESTARTS = 5;
const RESTART_WINDOW_MS = 60_000;
const INITIAL_BACKOFF_MS = 1_000;
const MAX_BACKOFF_MS = 16_000;
const BACKOFF_MULTIPLIER = 2;
const SCAN_INTERVAL_MS = 10_000;
const TEMPORARY_CLEANUP_DELAY_MS = 1_000;

interface SupervisedSession {
  name: string;
  strategy: "permanent" | "temporary";
  restartCount: number;
  restartWindowStart: number;
  lastRestartAt: number;
  nextBackoffMs: number;
  failed: boolean;
  pendingTimer: ReturnType<typeof setTimeout> | null;
}

interface PersistedState {
  sessions: Record<string, {
    restartCount: number;
    restartWindowStart: number;
    lastRestartAt: number;
    nextBackoffMs: number;
    failed: boolean;
  }>;
  savedAt: string;
}

export class Supervisor {
  private sessions = new Map<string, SupervisedSession>();
  private dirWatcher: fs.FSWatcher | null = null;
  private scanInterval: ReturnType<typeof setInterval> | null = null;
  private eventWriter: EventWriter;
  private stopping = false;
  private lockAcquired = false;

  constructor(private supervisorName: string) {
    this.eventWriter = new EventWriter(supervisorName);
  }

  start(): void {
    ensureSessionDir();

    // Acquire lock to prevent multiple supervisors
    if (!acquireLock("supervisor")) {
      console.error("[supervisor] another supervisor is already running");
      process.exit(1);
    }
    this.lockAcquired = true;

    // Write PID file so `pty supervisor stop` can find us
    const pidPath = path.join(getSupervisorDir(), "supervisor.pid");
    fs.writeFileSync(pidPath, process.pid.toString());

    this.loadState();
    this.scanAllSessions();
    this.startWatching();

    this.scanInterval = setInterval(() => {
      if (!this.stopping) this.scanAllSessions();
    }, SCAN_INTERVAL_MS);

    this.emitEvent(EventType.SUPERVISOR_START);
  }

  stop(): void {
    this.stopping = true;

    for (const tracked of this.sessions.values()) {
      if (tracked.pendingTimer) clearTimeout(tracked.pendingTimer);
    }

    this.dirWatcher?.close();
    this.dirWatcher = null;

    if (this.scanInterval) {
      clearInterval(this.scanInterval);
      this.scanInterval = null;
    }

    this.persistState();
    this.emitEvent(EventType.SUPERVISOR_STOP);

    // Clean up PID file
    try { fs.unlinkSync(path.join(getSupervisorDir(), "supervisor.pid")); } catch {}

    if (this.lockAcquired) {
      releaseLock("supervisor");
    }
  }

  private scanAllSessions(): void {
    const sessionDir = getSessionDir();
    let entries: string[];
    try {
      entries = fs.readdirSync(sessionDir);
    } catch {
      return;
    }

    const jsonFiles = entries.filter((e) => e.endsWith(".json"));
    const seenNames = new Set<string>();

    for (const file of jsonFiles) {
      const name = file.replace(/\.json$/, "");
      if (name === this.supervisorName) continue;
      seenNames.add(name);
      this.evaluateSession(name);
    }

    // Remove tracked sessions whose metadata no longer exists
    for (const name of this.sessions.keys()) {
      if (!seenNames.has(name)) {
        const tracked = this.sessions.get(name)!;
        if (tracked.pendingTimer) clearTimeout(tracked.pendingTimer);
        this.sessions.delete(name);
      }
    }
  }

  private evaluateSession(name: string): void {
    const metadata = readMetadata(name);
    if (!metadata) return;

    const strategy = metadata.tags?.strategy;

    // No strategy tag — stop tracking if previously tracked
    if (strategy !== "permanent" && strategy !== "temporary") {
      const existing = this.sessions.get(name);
      if (existing) {
        if (existing.pendingTimer) clearTimeout(existing.pendingTimer);
        this.sessions.delete(name);
        console.log(`[supervisor] stopped tracking ${name} (strategy removed)`);
      }
      return;
    }

    // Check both metadata and whether the process is actually alive.
    // If the daemon was killed externally, exitedAt may not be set.
    let isExited = !!metadata.exitedAt;
    if (!isExited) {
      const sockPath = path.join(getSessionDir(), `${name}.sock`);
      const pidPath = path.join(getSessionDir(), `${name}.pid`);
      try {
        const pid = parseInt(fs.readFileSync(pidPath, "utf-8").trim(), 10);
        process.kill(pid, 0); // throws if not alive
      } catch {
        // Process is dead but metadata doesn't know — treat as exited
        if (!fs.existsSync(sockPath)) {
          isExited = true;
        }
      }
    }

    if (strategy === "permanent") {
      if (!this.sessions.has(name)) {
        this.sessions.set(name, {
          name,
          strategy: "permanent",
          restartCount: 0,
          restartWindowStart: 0,
          lastRestartAt: 0,
          nextBackoffMs: INITIAL_BACKOFF_MS,
          failed: false,
          pendingTimer: null,
        });
        console.log(`[supervisor] tracking ${name} (permanent)`);
      }

      const tracked = this.sessions.get(name)!;
      tracked.strategy = "permanent";

      if (isExited && !tracked.failed && !tracked.pendingTimer) {
        this.scheduleRestart(name);
      }

      // If it's running, reset the restart window if enough time has passed
      if (!isExited && tracked.restartWindowStart > 0) {
        const elapsed = Date.now() - tracked.restartWindowStart;
        if (elapsed > RESTART_WINDOW_MS) {
          tracked.restartCount = 0;
          tracked.restartWindowStart = 0;
          tracked.nextBackoffMs = INITIAL_BACKOFF_MS;
        }
      }
    }

    if (strategy === "temporary") {
      if (isExited) {
        // Schedule cleanup
        if (!this.sessions.has(name) || !this.sessions.get(name)!.pendingTimer) {
          console.log(`[supervisor] cleaning up temporary session ${name}`);
          const timer = setTimeout(() => {
            cleanupAll(name);
            this.sessions.delete(name);
            console.log(`[supervisor] removed temporary session ${name}`);
          }, TEMPORARY_CLEANUP_DELAY_MS);
          this.sessions.set(name, {
            name,
            strategy: "temporary",
            restartCount: 0,
            restartWindowStart: 0,
            lastRestartAt: 0,
            nextBackoffMs: 0,
            failed: false,
            pendingTimer: timer,
          });
        }
      } else if (!this.sessions.has(name)) {
        this.sessions.set(name, {
          name,
          strategy: "temporary",
          restartCount: 0,
          restartWindowStart: 0,
          lastRestartAt: 0,
          nextBackoffMs: 0,
          failed: false,
          pendingTimer: null,
        });
        console.log(`[supervisor] tracking ${name} (temporary)`);
      }
    }
  }

  private scheduleRestart(name: string): void {
    const tracked = this.sessions.get(name);
    if (!tracked || tracked.strategy !== "permanent") return;

    const now = Date.now();

    // Reset window if expired
    if (tracked.restartWindowStart > 0 && now - tracked.restartWindowStart > RESTART_WINDOW_MS) {
      tracked.restartCount = 0;
      tracked.restartWindowStart = 0;
      tracked.nextBackoffMs = INITIAL_BACKOFF_MS;
    }

    // Check restart limit
    if (tracked.restartCount >= MAX_RESTARTS) {
      this.markFailed(name);
      return;
    }

    // Start window on first restart
    if (tracked.restartWindowStart === 0) {
      tracked.restartWindowStart = now;
    }

    const backoff = tracked.nextBackoffMs;
    console.log(`[supervisor] scheduling restart for ${name} in ${backoff}ms (attempt ${tracked.restartCount + 1}/${MAX_RESTARTS})`);

    tracked.pendingTimer = setTimeout(() => {
      tracked.pendingTimer = null;
      this.doRestart(name).catch((err) => {
        console.error(`[supervisor] restart failed for ${name}: ${err.message}`);
        tracked.restartCount++;
        tracked.nextBackoffMs = Math.min(tracked.nextBackoffMs * BACKOFF_MULTIPLIER, MAX_BACKOFF_MS);
        this.persistState();
        // Try again if under limit
        if (tracked.restartCount < MAX_RESTARTS) {
          this.scheduleRestart(name);
        } else {
          this.markFailed(name);
        }
      });
    }, backoff);
  }

  private async doRestart(name: string): Promise<void> {
    if (this.stopping) return;

    // Re-read metadata to verify session is still exited and supervised
    const metadata = readMetadata(name);
    if (!metadata) return;
    if (metadata.tags?.strategy !== "permanent") return;

    // Check if actually still dead (exitedAt may be missing if killed externally)
    if (!metadata.exitedAt) {
      const pidPath = path.join(getSessionDir(), `${name}.pid`);
      try {
        const pid = parseInt(fs.readFileSync(pidPath, "utf-8").trim(), 10);
        process.kill(pid, 0);
        return; // still alive, skip restart
      } catch {
        // dead — proceed with restart
      }
    }

    const tracked = this.sessions.get(name);
    if (!tracked) return;

    // If session was started from a pty.toml, re-read it for the latest config
    let command = metadata.command;
    let args = metadata.args;
    let displayCommand = metadata.displayCommand;
    let cwd = metadata.cwd;
    let tags = metadata.tags;

    const ptyfilePath = metadata.tags?.ptyfile;
    const ptyfileSession = metadata.tags?.["ptyfile.session"];
    if (ptyfilePath && ptyfileSession) {
      try {
        const dir = path.dirname(ptyfilePath);
        const ptyFile = readPtyFile(dir);
        const sessDef = ptyFile.sessions.find((s) => s.shortName === ptyfileSession);
        if (sessDef) {
          command = "/bin/sh";
          args = ["-c", sessDef.command];
          displayCommand = sessDef.command;
          cwd = ptyFile.dir;
          tags = { ...sessDef.tags, ptyfile: ptyfilePath, "ptyfile.session": ptyfileSession };
          console.log(`[supervisor] re-read pty.toml for ${name}`);
        }
      } catch (err: any) {
        console.log(`[supervisor] could not re-read pty.toml for ${name}: ${err.message}, using stored metadata`);
      }
    }

    // Clean up the dead session
    cleanupAll(name);

    // Respawn
    await spawnDaemon({ name, command, args, displayCommand, cwd, tags });

    tracked.restartCount++;
    tracked.lastRestartAt = Date.now();
    tracked.nextBackoffMs = Math.min(tracked.nextBackoffMs * BACKOFF_MULTIPLIER, MAX_BACKOFF_MS);

    console.log(`[supervisor] restarted ${name} (attempt ${tracked.restartCount}/${MAX_RESTARTS})`);

    this.emitEvent(EventType.SESSION_RESTART, {
      session: name,
      restartCount: tracked.restartCount,
      backoffMs: tracked.nextBackoffMs,
    });

    this.persistState();
  }

  private markFailed(name: string): void {
    const tracked = this.sessions.get(name);
    if (!tracked) return;

    tracked.failed = true;
    if (tracked.pendingTimer) {
      clearTimeout(tracked.pendingTimer);
      tracked.pendingTimer = null;
    }

    console.log(`[supervisor] ${name} marked as FAILED (${tracked.restartCount} restarts in window)`);

    // Set failed tag on the session
    try {
      updateTags(name, { "supervisor.status": "failed" }, []);
    } catch {
      // Metadata may have been cleaned up
    }

    this.emitEvent(EventType.SESSION_FAILED, {
      session: name,
      restartCount: tracked.restartCount,
      reason: `exceeded ${MAX_RESTARTS} restarts in ${RESTART_WINDOW_MS / 1000}s`,
    });

    this.persistState();
  }

  private emitEvent(type: string, fields?: Record<string, unknown>): void {
    this.eventWriter.append({
      session: this.supervisorName,
      type: type as EventRecord["type"],
      ts: new Date().toISOString(),
      ...fields,
    } as EventRecord);
  }

  private startWatching(): void {
    const sessionDir = getSessionDir();
    try {
      this.dirWatcher = fs.watch(sessionDir, (eventType, filename) => {
        if (this.stopping) return;
        if (!filename || !filename.endsWith(".json")) return;
        const name = filename.replace(/\.json$/, "");
        if (name === this.supervisorName) return;
        // Debounce: defer evaluation to next tick to let writes complete
        setImmediate(() => this.evaluateSession(name));
      });
    } catch (err) {
      console.error(`[supervisor] failed to watch session directory: ${err}`);
    }
  }

  private persistState(): void {
    const state: PersistedState = {
      sessions: {},
      savedAt: new Date().toISOString(),
    };
    for (const [name, tracked] of this.sessions) {
      if (tracked.strategy !== "permanent") continue;
      state.sessions[name] = {
        restartCount: tracked.restartCount,
        restartWindowStart: tracked.restartWindowStart,
        lastRestartAt: tracked.lastRestartAt,
        nextBackoffMs: tracked.nextBackoffMs,
        failed: tracked.failed,
      };
    }
    const filePath = path.join(getSupervisorDir(), "state.json");
    const tmp = filePath + ".tmp";
    try {
      fs.writeFileSync(tmp, JSON.stringify(state, null, 2));
      fs.renameSync(tmp, filePath);
    } catch {
      // Non-fatal
    }
  }

  private loadState(): void {
    const filePath = path.join(getSupervisorDir(), "state.json");
    try {
      const content = fs.readFileSync(filePath, "utf-8");
      const state: PersistedState = JSON.parse(content);
      for (const [name, saved] of Object.entries(state.sessions)) {
        this.sessions.set(name, {
          name,
          strategy: "permanent",
          ...saved,
          pendingTimer: null,
        });
      }
      console.log(`[supervisor] loaded state: ${Object.keys(state.sessions).length} tracked sessions`);
    } catch {
      // No state file or invalid — start fresh
    }
  }
}
