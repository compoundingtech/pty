// SSOT parity + generator-output checks for `pty completions`.
//
// These tests guard the contract in src/completions.ts:
//   - every user-facing command documented in `COMMAND_HELP` (cli.ts) has a
//     matching entry in the completion spec, so the two can't drift;
//   - `pty completions <shell>` emits a script that the target shell accepts
//     syntactically (`fish -n` / `bash -n` / `zsh -n` when installed).
//
// We run the built CLI (dist/cli.js) so the assertions cover the real dispatch
// path that ships — not just the in-process generator.

import { describe, it, expect } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import { execSync } from "node:child_process";
import { COMMANDS } from "../src/completions.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const nodeBin = process.execPath;

/** Run `node dist/cli.js completions <shell>` and return stdout. */
function gen(shell: string): string {
  const r = spawnSync(nodeBin, [cliPath, "completions", shell], {
    encoding: "utf8",
  });
  if (r.status !== 0) {
    throw new Error(
      `pty completions ${shell} exited ${r.status}\n${r.stderr}`,
    );
  }
  return r.stdout;
}

/** Resolve the list of command names that `cli.ts` documents in COMMAND_HELP. */
function documentedCommandNames(): Set<string> {
  // COMMAND_HELP keys are the canonical, documented command names.
  const src = fs.readFileSync(
    path.join(__dirname, "..", "src", "cli.ts"),
    "utf8",
  );
  const start = src.indexOf("const COMMAND_HELP:");
  const end = src.indexOf("};", start);
  const block = src.slice(start, end);
  const keys = [
    ...block.matchAll(/^  ([a-z][a-z-]*):\s*`/gm),
  ].map((m) => m[1]);
  return new Set(keys);
}

describe("completion spec parity with COMMAND_HELP", () => {
  it("covers every documented command (name or alias)", () => {
    const documented = documentedCommandNames();
    const specNames = new Set(
      COMMANDS.flatMap((c) => [c.name, ...(c.aliases ?? [])]),
    );
    const missing = [...documented].filter((k) => !specNames.has(k));
    expect(missing, `documented commands missing from completions spec: ${missing.join(", ")}`).toEqual([]);
  });
});

describe("pty completions <shell>", () => {
  it("prints fish, bash, zsh to stdout", () => {
    for (const shell of ["fish", "bash", "zsh"] as const) {
      const out = gen(shell);
      expect(out.length).toBeGreaterThan(50);
      expect(out, `output for ${shell} should end with a newline`).toMatch(/\n$/);
    }
  });

  it("prints usage and exits non-zero for an unknown shell", () => {
    const r = spawnSync(nodeBin, [cliPath, "completions", "tcsh"], {
      encoding: "utf8",
    });
    expect(r.status).toBe(2);
    expect(r.stderr).toMatch(/unknown shell/i);
  });

  it("prints usage for --help", () => {
    const r = spawnSync(nodeBin, [cliPath, "completions", "--help"], {
      encoding: "utf8",
    });
    expect(r.status).toBe(0);
    expect(r.stdout).toMatch(/Usage: pty completions/);
  });

  it("fish output is syntactically valid", () => {
    const fish = which("fish");
    if (!fish) return; // not installed in this environment
    const out = gen("fish");
    const r = spawnSync(fish, ["-n", "-c", out], { encoding: "utf8" });
    expect(r.status, `fish -n failed:\n${r.stderr}\n---\n${out}`).toBe(0);
  });

  it("bash output is syntactically valid", () => {
    const bash = which("bash");
    if (!bash) return;
    const out = gen("bash");
    const r = spawnSync(bash, ["-n", "-c", out], { encoding: "utf8" });
    expect(r.status, `bash -n failed:\n${r.stderr}\n---\n${out}`).toBe(0);
  });

  it("zsh output is syntactically valid", () => {
    const zsh = which("zsh");
    if (!zsh) return;
    const out = gen("zsh");
    const r = spawnSync(zsh, ["-n", "-c", out], { encoding: "utf8" });
    expect(r.status, `zsh -n failed:\n${r.stderr}\n---\n${out}`).toBe(0);
  });
});

/** Best-effort `which`: returns the resolved path or undefined if missing. */
function which(bin: string): string | undefined {
  try {
    return execSync(`command -v ${bin}`, { stdio: ["ignore", "pipe", "ignore"] })
      .toString()
      .trim() || undefined;
  } catch {
    return undefined;
  }
}
