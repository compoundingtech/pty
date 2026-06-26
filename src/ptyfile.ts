import * as fs from "node:fs";
import * as path from "node:path";
import { parse as parseToml } from "smol-toml";

const PTY_TOML = "pty.toml";

export interface PtySessionDef {
  /** Human-friendly label rendered by `pty ls` / events. By default:
   *  `<prefix>-<shortName>` if `prefix` is set, else `<shortName>`. The
   *  toml's optional `display_name = "..."` field overrides this. NOT the
   *  on-disk identifier — that's a separate short random id (or the toml's
   *  optional `id` field) generated at spawn time, so long display labels
   *  don't blow past the macOS `sockaddr_un.sun_path` limit. */
  displayName: string;
  /** Name as written in the toml (for `pty up <name>` filtering). */
  shortName: string;
  /** Explicit on-disk id from `id = "..."` in the toml. When null, `pty up`
   *  generates a random short id at spawn time. */
  id: string | null;
  command: string;
  tags?: Record<string, string>;
  env?: Record<string, string>;
}

export interface PtyFile {
  dir: string;
  prefix: string | null;
  sessions: PtySessionDef[];
}

/** Find and parse a pty.toml file. Searches the given directory (or cwd). */
export function readPtyFile(dir?: string): PtyFile {
  const resolvedDir = dir ? path.resolve(dir) : process.cwd();
  const filePath = path.join(resolvedDir, PTY_TOML);

  let content: string;
  try {
    content = fs.readFileSync(filePath, "utf-8");
  } catch (err: any) {
    if (err?.code === "ENOENT") {
      throw new Error(`No ${PTY_TOML} found in ${resolvedDir}`);
    }
    throw err;
  }

  let parsed: any;
  try {
    parsed = parseToml(content);
  } catch (err: any) {
    throw new Error(`Invalid ${PTY_TOML} in ${resolvedDir}: ${err?.message ?? String(err)}`);
  }

  const prefix = typeof parsed.prefix === "string" && parsed.prefix.length > 0 ? parsed.prefix : null;
  const sessions: PtySessionDef[] = [];

  if (parsed.sessions && typeof parsed.sessions === "object") {
    for (const [rawName, def] of Object.entries(parsed.sessions)) {
      const defaultDisplayName = prefix ? `${prefix}-${rawName}` : rawName;
      if (!def || typeof def !== "object") {
        throw new Error(`Invalid session "${defaultDisplayName}" in ${filePath}: expected a table`);
      }
      const d = def as Record<string, unknown>;
      if (typeof d.command !== "string" || d.command.length === 0) {
        throw new Error(`Session "${defaultDisplayName}" in ${filePath} is missing a "command" field`);
      }

      // Optional `display_name` override.
      let displayName = defaultDisplayName;
      if (d.display_name !== undefined) {
        if (typeof d.display_name !== "string" || d.display_name.length === 0) {
          throw new Error(`Session "${defaultDisplayName}" in ${filePath}: "display_name" must be a non-empty string`);
        }
        displayName = d.display_name;
      }

      // Optional `id` override (pinned on-disk identifier).
      let id: string | null = null;
      if (d.id !== undefined) {
        if (typeof d.id !== "string" || d.id.length === 0) {
          throw new Error(`Session "${defaultDisplayName}" in ${filePath}: "id" must be a non-empty string`);
        }
        id = d.id;
      }

      let tags: Record<string, string> | undefined;
      if (d.tags && typeof d.tags === "object") {
        tags = {};
        for (const [k, v] of Object.entries(d.tags as Record<string, unknown>)) {
          tags[k] = String(v);
        }
      }

      let env: Record<string, string> | undefined;
      if (d.env !== undefined) {
        if (!d.env || typeof d.env !== "object" || Array.isArray(d.env)) {
          throw new Error(`Session "${defaultDisplayName}" in ${filePath}: "env" must be a table of string values`);
        }
        env = {};
        for (const [k, v] of Object.entries(d.env as Record<string, unknown>)) {
          if (typeof v !== "string") {
            throw new Error(`Session "${defaultDisplayName}" in ${filePath}: env.${k} must be a string`);
          }
          env[k] = v;
        }
      }

      sessions.push({ displayName, shortName: rawName, id, command: d.command, tags, env });
    }
  }

  if (sessions.length === 0) {
    throw new Error(`No sessions defined in ${filePath}`);
  }

  return { dir: resolvedDir, prefix, sessions };
}

/** Build the `/bin/sh -c` payload for a session: an optional `export K='V'; ...`
 *  prefix derived from `sess.env`, followed by the session's own command. */
export function commandWithEnvExports(sess: PtySessionDef): string {
  const entries = sess.env ? Object.entries(sess.env) : [];
  if (entries.length === 0) return sess.command;
  const prefix = entries
    .map(([k, v]) => `export ${k}='${v.replace(/'/g, `'\\''`)}'`)
    .join("; ");
  return `${prefix}; ${sess.command}`;
}
