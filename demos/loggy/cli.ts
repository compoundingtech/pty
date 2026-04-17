// Pure argv parser for loggy. No I/O — safe to unit-test.

export interface ParsedArgs {
  /** File to tee child stdout to (raw). */
  out: string | null;
  /** File to tee child stderr to (raw). */
  err: string | null;
  /** File to tee combined tagged output to (plain-text with prefix). */
  log: string | null;
  /** When true, don't set FORCE_COLOR=1 in the child's env. */
  noColor: boolean;
  /** In-memory line limit before oldest lines are dropped. */
  scrollback: number;
  /** The command to run. */
  command: string;
  /** Arguments to pass to the command. */
  args: string[];
}

export const DEFAULT_SCROLLBACK = 10_000;

export const USAGE =
  `Usage: loggy [flags] <command> [args...]\n\n` +
  `Wrap a command, capture stdout and stderr as separate streams, and stream\n` +
  `them into a live TUI with filter (o/e/b), search (/), and follow (f).\n\n` +
  `Flags:\n` +
  `  --out <path>         Tee child stdout to <path>\n` +
  `  --err <path>         Tee child stderr to <path>\n` +
  `  --log <path>         Tee both streams to <path> as tagged plain-text\n` +
  `  --no-color           Do not set FORCE_COLOR=1 in the child's env\n` +
  `  --scrollback <n>     In-memory line limit (default ${DEFAULT_SCROLLBACK})\n` +
  `  -h, --help           Show this help\n\n` +
  `Examples:\n` +
  `  loggy npm run build\n` +
  `  loggy --log /tmp/all.log -- tsc --watch\n` +
  `  loggy --out /tmp/o.log --err /tmp/e.log bin/my-server\n`;

export class UsageError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "UsageError";
  }
}

/** Parse loggy's argv. `argv` should exclude the node and script entries
 *  (i.e. `process.argv.slice(2)`). Throws `UsageError` on malformed input
 *  or when the user asked for help. */
export function parseArgs(argv: readonly string[]): ParsedArgs {
  const out: ParsedArgs = {
    out: null,
    err: null,
    log: null,
    noColor: false,
    scrollback: DEFAULT_SCROLLBACK,
    command: "",
    args: [],
  };

  let i = 0;
  while (i < argv.length) {
    const arg = argv[i];

    if (arg === "--") {
      // Everything after -- is the command + its args
      i++;
      if (i >= argv.length) throw new UsageError("Missing command after --");
      out.command = argv[i];
      out.args = argv.slice(i + 1);
      return finalize(out);
    }

    if (arg === "-h" || arg === "--help") {
      throw new UsageError("__help__");
    }

    if (arg === "--no-color") { out.noColor = true; i++; continue; }

    if (arg === "--out") {
      if (i + 1 >= argv.length) throw new UsageError("--out requires a path");
      out.out = argv[i + 1]; i += 2; continue;
    }
    if (arg === "--err") {
      if (i + 1 >= argv.length) throw new UsageError("--err requires a path");
      out.err = argv[i + 1]; i += 2; continue;
    }
    if (arg === "--log") {
      if (i + 1 >= argv.length) throw new UsageError("--log requires a path");
      out.log = argv[i + 1]; i += 2; continue;
    }
    if (arg === "--scrollback") {
      if (i + 1 >= argv.length) throw new UsageError("--scrollback requires a number");
      const n = parseInt(argv[i + 1], 10);
      if (!Number.isFinite(n) || n <= 0) throw new UsageError(`--scrollback must be a positive integer (got "${argv[i + 1]}")`);
      out.scrollback = n;
      i += 2;
      continue;
    }

    if (arg.startsWith("-")) {
      throw new UsageError(`Unknown flag: ${arg}`);
    }

    // First non-flag: treat as the command. Rest are its args.
    out.command = arg;
    out.args = argv.slice(i + 1);
    return finalize(out);
  }

  throw new UsageError("Missing command");
}

function finalize(out: ParsedArgs): ParsedArgs {
  if (!out.command) throw new UsageError("Missing command");
  return out;
}
