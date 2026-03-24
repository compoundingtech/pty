import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const projectRoot = path.join(__dirname, "..");
const docsPath = path.join(projectRoot, "docs", "testing.md");

const content = fs.readFileSync(docsPath, "utf-8");

// Extract ```typescript test code blocks
const codeBlockRegex = /```typescript test\n([\s\S]*?)```/g;
const blocks: string[] = [];
let match: RegExpExecArray | null;
while ((match = codeBlockRegex.exec(content)) !== null) {
  blocks.push(match[1]);
}

if (blocks.length === 0) {
  console.log("No executable code blocks found in docs/testing.md");
  process.exit(0);
}

console.log(`Found ${blocks.length} executable code blocks`);

// Separate imports from body code in each block
const importSet = new Set<string>();
const testCases = blocks
  .map((code, i) => {
    // Replace @myobie/pty/testing imports with relative path
    const adjusted = code.replace(
      /from ["']@myobie\/pty\/testing["']/g,
      `from "${path.join(projectRoot, "src", "testing", "index.ts").replace(/\\/g, "/")}"`
    );

    // Hoist import lines
    const lines = adjusted.split("\n");
    const bodyLines: string[] = [];
    for (const line of lines) {
      if (/^\s*import\s/.test(line)) {
        importSet.add(line.trim());
      } else {
        bodyLines.push(line);
      }
    }
    const body = bodyLines.join("\n").trim();
    return `  it("doc example ${i + 1}", async () => {\n${body}\n  }, 10000);`;
  })
  .join("\n\n");

const imports = ['import { describe, it, expect } from "vitest";', ...importSet].join("\n");
const testFile = `${imports}\n\ndescribe("docs/testing.md examples", () => {\n${testCases}\n});\n`;

// Write generated test file inside the project so vitest's include pattern matches
const tmpFile = path.join(projectRoot, "tests", "_docs-verify.test.ts");
fs.writeFileSync(tmpFile, testFile);

console.log(`Generated test file: ${tmpFile}`);
console.log("Running vitest...\n");

const vitestBin = path.join(projectRoot, "node_modules", ".bin", "vitest");
const result = spawnSync(vitestBin, ["run", tmpFile], {
  stdio: "inherit",
  env: process.env,
  cwd: projectRoot,
});

// Clean up
try { fs.unlinkSync(tmpFile); } catch {}

if (result.status !== 0) {
  console.error("\nDoc verification failed!");
  process.exit(result.status ?? 1);
}

console.log("\nAll doc examples passed!");
