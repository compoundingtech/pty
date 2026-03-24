/** Captured terminal state at a point in time. */
export interface Screenshot {
  /** Plain text lines. Trailing whitespace is trimmed per line. Trailing empty lines are removed. */
  lines: string[];
  /** All lines joined with `"\n"`. Convenient for `.toContain()` assertions. */
  text: string;
  /** Full ANSI-serialized terminal state, including escape codes. Use to verify colors, bold, etc. */
  ansi: string;
}

/** Options for `Session.spawn()`. */
export interface SpawnOptions {
  /** Terminal height in rows. Default: 24. */
  rows?: number;
  /** Terminal width in columns. Default: 80. */
  cols?: number;
  /** Working directory for the spawned process. */
  cwd?: string;
  /** Extra environment variables, merged with `process.env`. */
  env?: Record<string, string>;
}

/** Options for `Session.server()`. */
export interface ServerOptions {
  /** Session name. Auto-generated if omitted. */
  name?: string;
  /** Terminal height in rows. Default: 24. */
  rows?: number;
  /** Terminal width in columns. Default: 80. */
  cols?: number;
  /** Working directory for the spawned process. */
  cwd?: string;
}
