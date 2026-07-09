import { describe, it, expect } from "vitest";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import { formatVersion } from "../src/version.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const nodeBin = process.execPath;
const cliPath = path.join(__dirname, "..", "dist", "cli.js");
const pkgVersion = JSON.parse(
  fs.readFileSync(path.join(__dirname, "..", "package.json"), "utf8"),
).version as string;

describe("formatVersion", () => {
  it("appends +<sha> when a sha is present", () => {
    expect(formatVersion("0.11.0", "abcdef1")).toBe("0.11.0+abcdef1");
  });

  it("prints bare semver when there is no sha (e.g. npm install)", () => {
    expect(formatVersion("0.11.0", null)).toBe("0.11.0");
  });
});

describe("pty --version", () => {
  // `<semver>` optionally followed by `+<short-sha>`. Tests run inside the git
  // checkout, so the sha part is present here; the no-sha path is covered by
  // the formatVersion unit test above.
  const VERSION_RE = /^\d+\.\d+\.\d+(\+[0-9a-f]{4,})?$/;

  for (const form of ["--version", "version", "-v", "-V"]) {
    it(`\`pty ${form}\` prints the version and exits 0`, () => {
      const r = spawnSync(nodeBin, [cliPath, form], { encoding: "utf8", timeout: 15000 });
      expect(r.status).toBe(0);
      const out = r.stdout.trim();
      expect(out).toMatch(VERSION_RE);
      // The semver part must match package.json.
      expect(out.split("+")[0]).toBe(pkgVersion);
    });
  }

  it("is not treated as an unknown command", () => {
    const r = spawnSync(nodeBin, [cliPath, "--version"], { encoding: "utf8", timeout: 15000 });
    expect(r.stderr).not.toContain("Unknown command");
  });
});
