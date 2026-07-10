import { describe, it, expect } from "vitest";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const cliSource = fs.readFileSync(path.join(__dirname, "..", "src", "cli.ts"), "utf8");

// Canonical subcommands that must each ship focused `--help`.
const COMMANDS = [
  "run", "attach", "exec", "peek", "send", "events", "list", "stats",
  "restart", "kill", "rm", "gc", "tag", "tag-multi", "emit", "rename",
  "up", "down", "test",
];
// Aliases that must resolve to the same help.
const ALIASES = ["a", "ls", "remove"];
// Dispatch `case` labels that are NOT per-subcommand commands (no focused help
// expected): the interactive TUI, and the global help/version verbs+flags.
const NON_COMMAND_CASES = new Set([
  "interactive", "i", "help", "--help", "-h", "version", "--version", "-v", "-V",
]);

function help(cmd: string) {
  return spawnSync(nodeBin, [cliPath, cmd, "--help"], {
    encoding: "utf8",
    timeout: 15000,
    env: { ...process.env, PTY_ROOT_LEGACY_SILENT: "1" },
  });
}

describe("pty --help — per-subcommand help", () => {
  for (const cmd of [...COMMANDS, ...ALIASES]) {
    it(`\`pty ${cmd} --help\` prints usage + an example and exits 0`, () => {
      const r = help(cmd);
      expect(r.status).toBe(0);
      // Usage synopsis.
      expect(r.stdout).toMatch(/^Usage: pty /);
      // At least one concrete example (an `Examples:` header or a `  pty …` line
      // beyond the synopsis).
      const exampleLines = r.stdout.split("\n").filter((l) => /^ {2}pty /.test(l));
      expect(exampleLines.length).toBeGreaterThan(0);
      // Help must not have executed the command (no session-list / JSON output).
      expect(r.stdout).not.toMatch(/^\[/);
    });
  }
});

describe("pty --help — no drift", () => {
  it("every dispatch `case` is either a documented command or a known non-command", () => {
    // Extract every `case "X":` label from the dispatcher.
    const cases = [...cliSource.matchAll(/case\s+"([^"]+)":/g)].map((m) => m[1]);
    const documented = new Set([...COMMANDS, ...ALIASES]);
    const uncovered = cases.filter((c) => !documented.has(c) && !NON_COMMAND_CASES.has(c));
    // A new subcommand added without focused help (or without being listed as a
    // non-command) will show up here — add it to COMMAND_HELP + this test.
    expect(uncovered).toEqual([]);
  });

  it("top-level `pty --help` lists every subcommand", () => {
    const r = spawnSync(nodeBin, [cliPath, "--help"], {
      encoding: "utf8",
      timeout: 15000,
      env: { ...process.env, PTY_ROOT_LEGACY_SILENT: "1" },
    });
    expect(r.status).toBe(0);
    for (const cmd of COMMANDS) {
      expect(r.stdout).toContain(`pty ${cmd} `);
    }
  });
});
