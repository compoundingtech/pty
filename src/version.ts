import * as fs from "node:fs";
import * as path from "node:path";
import { execFileSync } from "node:child_process";

/** Directory of this module (dist/ when built, src/ when run from source).
 *  package.json is one level up in both layouts. */
function selfDir(): string {
  return import.meta.dirname ?? path.dirname(new URL(import.meta.url).pathname);
}

/** Semver from the shipped package.json. Falls back to "0.0.0" if it can't be
 *  read (should never happen — npm always ships package.json). */
export function readPackageVersion(): string {
  try {
    const pkg = JSON.parse(fs.readFileSync(path.join(selfDir(), "..", "package.json"), "utf8"));
    return typeof pkg.version === "string" ? pkg.version : "0.0.0";
  } catch {
    return "0.0.0";
  }
}

/** Short git sha of THIS pty checkout, or null when unavailable. Gated on a
 *  `.git` in pty's own package root so an npm-installed copy sitting inside an
 *  unrelated parent repo never reports that repo's sha as pty's version. */
export function readGitShortSha(): string | null {
  const root = path.join(selfDir(), "..");
  try {
    if (!fs.existsSync(path.join(root, ".git"))) return null;
    const sha = execFileSync("git", ["rev-parse", "--short", "HEAD"], {
      cwd: root,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    }).trim();
    return /^[0-9a-f]{4,}$/.test(sha) ? sha : null;
  } catch {
    return null;
  }
}

/** Format per house convention: `<semver>+<short-sha>`, or bare `<semver>` when
 *  no git sha is available. Pure — exported for unit testing. */
export function formatVersion(version: string, sha: string | null): string {
  return sha ? `${version}+${sha}` : version;
}

/** Print the version as `<semver>+<short-sha>` (or bare `<semver>`). */
export function printVersion(): void {
  console.log(formatVersion(readPackageVersion(), readGitShortSha()));
}
