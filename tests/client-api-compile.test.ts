import { describe, it, expect } from "vitest";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const repoRoot = path.join(path.dirname(fileURLToPath(import.meta.url)), "..");
const clientApi = path.join(repoRoot, "src", "client-api.ts");

describe("client API compile safety", () => {
  it("bun-compiles a client-only import without pulling server-only native code", () => {
    const tempRoot = fs.mkdtempSync(path.join(os.tmpdir(), "pty-client-compile-"));
    const entryPath = path.join(tempRoot, "entry.ts");
    const binaryPath = path.join(tempRoot, "entry");
    try {
      fs.writeFileSync(
        entryPath,
        `import { listSessions } from ${JSON.stringify(clientApi)};\nconsole.log(typeof listSessions);\n`
      );

      const build = spawnSync("bun", ["build", "--compile", "--outfile", binaryPath, entryPath], {
        cwd: tempRoot,
        encoding: "utf-8",
      });

      if (build.status !== 0) {
        throw new Error(`bun build failed:\n${build.stdout}${build.stderr}`);
      }

      const run = spawnSync(binaryPath, [], {
        cwd: tempRoot,
        encoding: "utf-8",
      });

      expect(run.status).toBe(0);
      expect(run.stdout.trim()).toBe("function");
      expect(run.stderr.trim()).toBe("");
    } finally {
      fs.rmSync(tempRoot, { recursive: true, force: true });
    }
  });
});
