import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const projectRoot = path.join(__dirname, "..");
const docsPath = path.join(projectRoot, "docs", "testing.md");
const vrsRoot = path.join(projectRoot, "docs", "vrs");

function verifyVrs(): void {
  if (!fs.existsSync(vrsRoot)) {
    console.error("VRS verification failed:\n- docs/vrs is missing");
    process.exit(1);
  }

  const expected = ["requirements.md", "spec.md"];
  const actual = fs.readdirSync(vrsRoot).sort();
  const requirementsPath = path.join(vrsRoot, "requirements.md");
  const specPath = path.join(vrsRoot, "spec.md");
  const errors: string[] = [];
  let requirementCount = 0;

  if (actual.join("\n") !== expected.join("\n")) {
    errors.push(`docs/vrs must contain only ${expected.join(" and ")}`);
  }
  if (!fs.existsSync(requirementsPath) || !fs.existsSync(specPath)) {
    errors.push("docs/vrs requires requirements.md and spec.md");
  } else {
    const requirements = fs.readFileSync(requirementsPath, "utf-8");
    const spec = fs.readFileSync(specPath, "utf-8");
    const ids = [...requirements.matchAll(/^- \*\*(R\d{2}) [^*]+:\*\*/gm)].map(
      (match) => match[1],
    );
    requirementCount = ids.length;

    if (ids.length === 0) errors.push("requirements.md defines no requirement IDs");
    if (!ids.every((id, index) => id === `R${String(index + 1).padStart(2, "0")}`)) {
      errors.push("requirement IDs must be sequential in document order");
    }
    if (!spec.includes("[requirements.md](./requirements.md)")) {
      errors.push("spec.md must link its requirements.md");
    }
    if (!spec.includes("## Status")) errors.push("spec.md must declare Status");

    const references = new Set([...spec.matchAll(/\bR\d{2}\b/g)].map((match) => match[0]));
    for (const id of ids) {
      if (!references.has(id)) errors.push(`spec.md does not reference ${id}`);
    }
    for (const id of references) {
      if (!ids.includes(id)) errors.push(`spec.md references unknown requirement ${id}`);
    }

    for (const [file, content] of [
      [requirementsPath, requirements],
      [specPath, spec],
    ] as const) {
      for (const match of content.matchAll(/\]\(([^)#]+)(?:#[^)]+)?\)/g)) {
        if (/^[a-z]+:/i.test(match[1])) continue;
        if (!fs.existsSync(path.resolve(path.dirname(file), match[1]))) {
          errors.push(`${path.relative(projectRoot, file)} has broken link ${match[1]}`);
        }
      }
    }
  }

  if (errors.length > 0) {
    console.error(`VRS verification failed:\n${errors.map((error) => `- ${error}`).join("\n")}`);
    process.exit(1);
  }
  console.log(`Verified 2 VRS documents and ${requirementCount} requirement IDs`);
}

verifyVrs();

if (process.argv.includes("--vrs-only")) process.exit(0);

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
    // Replace @compoundingtech/pty/testing imports with relative path
    const adjusted = code.replace(
      /from ["']@compoundingtech\/pty\/testing["']/g,
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
