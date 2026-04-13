import * as fs from "node:fs";
import * as path from "node:path";
import { parse as parseToml } from "smol-toml";

const PTY_TOML = "pty.toml";

export interface PtySessionDef {
  name: string;       // full session name (with prefix if set)
  shortName: string;  // name as written in the toml (for filtering)
  command: string;
  tags?: Record<string, string>;
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
      const name = prefix ? `${prefix}-${rawName}` : rawName;
      if (!def || typeof def !== "object") {
        throw new Error(`Invalid session "${name}" in ${filePath}: expected a table`);
      }
      const d = def as Record<string, unknown>;
      if (typeof d.command !== "string" || d.command.length === 0) {
        throw new Error(`Session "${name}" in ${filePath} is missing a "command" field`);
      }

      let tags: Record<string, string> | undefined;
      if (d.tags && typeof d.tags === "object") {
        tags = {};
        for (const [k, v] of Object.entries(d.tags as Record<string, unknown>)) {
          tags[k] = String(v);
        }
      }

      sessions.push({ name, shortName: rawName, command: d.command, tags });
    }
  }

  if (sessions.length === 0) {
    throw new Error(`No sessions defined in ${filePath}`);
  }

  return { dir: resolvedDir, prefix, sessions };
}
