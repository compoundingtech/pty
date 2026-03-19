import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";
import * as net from "node:net";

const DEFAULT_SESSION_DIR = path.join(os.homedir(), ".local", "state", "pty");

const DEAD_SESSION_TTL_MS = 24 * 60 * 60 * 1000; // 24 hours

const VALID_NAME_RE = /^[a-zA-Z0-9._-]+$/;

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
}

export function getSessionDir(): string {
  return process.env.PTY_SESSION_DIR ?? DEFAULT_SESSION_DIR;
}

export function ensureSessionDir(): void {
  fs.mkdirSync(getSessionDir(), { recursive: true });
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

export interface SessionMetadata {
  command: string;
  args: string[];
  displayCommand: string; // original command as the user typed it
  cwd: string;
  createdAt: string;
  exitCode?: number;
  exitedAt?: string;
  lastLines?: string[];
}

export interface SessionInfo {
  name: string;
  socketPath: string;
  pid: number | null;
  status: "running" | "exited";
  metadata: SessionMetadata | null;
}

export function writeMetadata(name: string, metadata: SessionMetadata): void {
  ensureSessionDir();
  fs.writeFileSync(getMetadataPath(name), JSON.stringify(metadata, null, 2));
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

    // Auto-clean dead sessions older than 24h
    if (metadata.exitedAt) {
      const exitedAt = new Date(metadata.exitedAt).getTime();
      if (Date.now() - exitedAt > DEAD_SESSION_TTL_MS) {
        cleanupAll(name);
        continue;
      }
    }

    sessions.push({
      name,
      socketPath: getSocketPath(name),
      pid: null,
      status: "exited",
      metadata,
    });
  }

  return sessions;
}

export async function getSession(name: string): Promise<SessionInfo | null> {
  const sessions = await listSessions();
  return sessions.find((s) => s.name === name) ?? null;
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
  releaseLock(name);
}

function getLockPath(name: string): string {
  return path.join(getSessionDir(), `${name}.lock`);
}

/**
 * Acquire an exclusive lock for a session name. Prevents concurrent
 * `pty run` calls from racing to create the same session.
 * Returns true if acquired, false if another process holds it.
 */
export function acquireLock(name: string): boolean {
  ensureSessionDir();
  const lockPath = getLockPath(name);
  try {
    fs.writeFileSync(lockPath, process.pid.toString(), { flag: "wx" });
    return true;
  } catch (e: any) {
    if (e.code === "EEXIST") {
      // Lock file exists — check if the holding process is still alive
      let shouldSteal = false;
      try {
        const pid = parseInt(fs.readFileSync(lockPath, "utf-8").trim(), 10);
        if (isNaN(pid)) {
          // Garbage content — treat as stale
          shouldSteal = true;
        } else {
          process.kill(pid, 0); // throws if process is dead
          return false; // process is alive, lock is valid
        }
      } catch {
        // Couldn't read, or holding process is dead
        shouldSteal = true;
      }

      if (shouldSteal) {
        try {
          fs.unlinkSync(lockPath);
          fs.writeFileSync(lockPath, process.pid.toString(), { flag: "wx" });
          return true;
        } catch {
          return false;
        }
      }
    }
    return false;
  }
}

export function releaseLock(name: string): void {
  try {
    fs.unlinkSync(getLockPath(name));
  } catch {}
}

// Keep backward compat for server.ts close()
export { cleanupSocket as cleanup };
