import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, type ChildProcess } from "node:child_process";
import { Terminal } from "@xterm/headless";
import { SerializeAddon } from "@xterm/addon-serialize";
import * as pty from "node-pty";
import { resolveKey } from "../src/keys.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

export interface Screenshot {
  lines: string[];
  text: string;
  ansi: string;
}

/**
 * Spawns the CLI as a pty process and provides helpers for
 * driving the interactive TUI in tests.
 */
export class TuiSession {
  private ptyProcess: pty.IPty;
  private terminal: Terminal;
  private serialize: SerializeAddon;
  rows: number;
  cols: number;

  private constructor(
    ptyProcess: pty.IPty,
    rows: number,
    cols: number
  ) {
    this.ptyProcess = ptyProcess;
    this.rows = rows;
    this.cols = cols;
    this.terminal = new Terminal({
      rows,
      cols,
      scrollback: 1000,
      allowProposedApi: true,
    });
    this.serialize = new SerializeAddon();
    this.terminal.loadAddon(this.serialize);

    // Feed pty output into xterm-headless
    this.ptyProcess.onData((data: string) => {
      this.terminal.write(data);
    });
  }

  /**
   * Create a TUI session that runs `tsx src/cli.ts` with a custom session dir.
   */
  static create(opts: {
    sessionDir: string;
    rows?: number;
    cols?: number;
    cwd?: string;
    args?: string[];
  }): TuiSession {
    const rows = opts.rows ?? 24;
    const cols = opts.cols ?? 80;
    const cwd = opts.cwd ?? process.cwd();

    const tsxBin = path.join(__dirname, "..", "node_modules", ".bin", "tsx");
    const cliModule = path.join(__dirname, "..", "src", "cli.ts");
    const args = opts.args ?? [];

    const env = {
      ...process.env,
      PTY_SESSION_DIR: opts.sessionDir,
      TERM: "xterm-256color",
    };
    delete (env as any).PTY_SERVER_CONFIG;

    const proc = pty.spawn(tsxBin, [cliModule, ...args], {
      name: "xterm-256color",
      cols,
      rows,
      cwd,
      env: env as Record<string, string>,
    });

    return new TuiSession(proc, rows, cols);
  }

  screenshot(): Screenshot {
    const buffer = this.terminal.buffer.active;
    const lines: string[] = [];
    for (let i = 0; i < buffer.length; i++) {
      const line = buffer.getLine(i);
      if (line) {
        lines.push(line.translateToString(true));
      }
    }
    while (lines.length > 0 && lines[lines.length - 1].trim() === "") {
      lines.pop();
    }
    return {
      lines,
      text: lines.join("\n"),
      ansi: this.serialize.serialize(),
    };
  }

  sendKeys(keys: string): void {
    this.ptyProcess.write(keys);
  }

  press(keyName: string): void {
    this.sendKeys(resolveKey(keyName));
  }

  type(text: string): void {
    this.sendKeys(text);
  }

  async waitForText(text: string, timeoutMs = 5000): Promise<Screenshot> {
    const start = Date.now();
    while (Date.now() - start < timeoutMs) {
      await new Promise((r) => setTimeout(r, 50));
      const ss = this.screenshot();
      if (ss.text.includes(text)) return ss;
    }
    const ss = this.screenshot();
    throw new Error(
      `Timed out after ${timeoutMs}ms waiting for "${text}".\nScreen:\n${ss.text}`
    );
  }

  async waitForAbsent(text: string, timeoutMs = 5000): Promise<Screenshot> {
    const start = Date.now();
    while (Date.now() - start < timeoutMs) {
      await new Promise((r) => setTimeout(r, 50));
      const ss = this.screenshot();
      if (!ss.text.includes(text)) return ss;
    }
    const ss = this.screenshot();
    throw new Error(
      `Timed out after ${timeoutMs}ms waiting for "${text}" to disappear.\nScreen:\n${ss.text}`
    );
  }

  async waitFor(
    predicate: (ss: Screenshot) => boolean,
    timeoutMs = 5000,
    description = "predicate"
  ): Promise<Screenshot> {
    const start = Date.now();
    while (Date.now() - start < timeoutMs) {
      await new Promise((r) => setTimeout(r, 50));
      const ss = this.screenshot();
      if (predicate(ss)) return ss;
    }
    const ss = this.screenshot();
    throw new Error(
      `Timed out after ${timeoutMs}ms waiting for ${description}.\nScreen:\n${ss.text}`
    );
  }

  close(): void {
    try {
      this.ptyProcess.kill();
    } catch {}
    this.terminal.dispose();
  }
}

/**
 * Spawn a background session as a separate daemon process with PTY_SESSION_DIR set.
 * Returns the child process (and its PID for cleanup).
 */
export async function createBackgroundSession(
  sessionDir: string,
  name: string,
  command: string,
  args: string[] = [],
  cwd?: string
): Promise<{ child: ChildProcess; pid: number }> {
  const tsxBin = path.join(__dirname, "..", "node_modules", ".bin", "tsx");
  const serverModule = path.join(__dirname, "..", "src", "server.ts");

  const config = JSON.stringify({
    name,
    command,
    args,
    displayCommand: command,
    cwd: cwd ?? os.tmpdir(),
    rows: 24,
    cols: 80,
  });

  const child = spawn(tsxBin, [serverModule], {
    detached: true,
    stdio: ["ignore", "ignore", "pipe"],
    env: {
      ...process.env,
      PTY_SERVER_CONFIG: config,
      PTY_SESSION_DIR: sessionDir,
    },
  });

  let stderr = "";
  child.stderr?.on("data", (d: Buffer) => {
    stderr += d.toString();
  });

  let exitCode: number | null = null;
  child.on("exit", (code) => {
    exitCode = code;
  });

  (child.stderr as any)?.unref?.();
  child.unref();

  // Wait for socket to appear
  const socketPath = path.join(sessionDir, `${name}.sock`);
  const start = Date.now();
  while (Date.now() - start < 5000) {
    if (exitCode !== null) {
      throw new Error(`Background session daemon exited with code ${exitCode}. stderr:\n${stderr}`);
    }
    try {
      fs.statSync(socketPath);
      await new Promise((r) => setTimeout(r, 100));
      return { child, pid: child.pid! };
    } catch {}
    await new Promise((r) => setTimeout(r, 50));
  }
  throw new Error(`Timeout waiting for background session "${name}" socket at ${socketPath}. stderr:\n${stderr}`);
}
