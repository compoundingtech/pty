export interface Screenshot {
  /** Plain text lines (trailing whitespace trimmed per line) */
  lines: string[];
  /** All lines joined with newline */
  text: string;
  /** ANSI-serialized terminal state (includes escape codes) */
  ansi: string;
}

export interface SpawnOptions {
  rows?: number;
  cols?: number;
  cwd?: string;
  env?: Record<string, string>;
}

export interface ServerOptions {
  name?: string;
  rows?: number;
  cols?: number;
  cwd?: string;
}
