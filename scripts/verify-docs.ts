import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const projectRoot = path.join(__dirname, "..");
const docsPath = path.join(projectRoot, "docs", "testing.md");
const vrsRoot = path.join(projectRoot, "docs", "vrs");

function collectMarkdown(dir: string): string[] {
  return fs.readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const target = path.join(dir, entry.name);
    return entry.isDirectory()
      ? collectMarkdown(target)
      : entry.isFile() && entry.name.endsWith(".md")
        ? [target]
        : [];
  });
}

function collectVrsNodeDirectories(dir: string, companions: Set<string>): string[] {
  return fs.readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    if (!entry.isDirectory() || companions.has(entry.name)) return [];
    const target = path.join(dir, entry.name);
    return [target, ...collectVrsNodeDirectories(target, companions)];
  });
}

function verifyVrs(): void {
  if (!fs.existsSync(vrsRoot)) return;

  const files = collectMarkdown(vrsRoot);
  const errors: string[] = [];
  const contents = new Map(files.map((file) => [file, fs.readFileSync(file, "utf-8")]));
  const requirementDefinitions = new Map<string, string>();
  const requirementIdsByFile = new Map<string, string[]>();
  const companionDirectories = new Set([
    ".decisions",
    ".experiments",
    ".reference",
    ".delta",
    ".proposed",
  ]);

  for (const file of files.filter((candidate) => path.basename(candidate) === "requirements.md")) {
    const content = contents.get(file)!;
    const definitionPattern = /^- \*\*(PTY(?:\.[A-Z]+)*-R\d+)\s+[^*]+\*\*/gm;
    let definition: RegExpExecArray | null;
    const ids: string[] = [];
    while ((definition = definitionPattern.exec(content)) !== null) {
      const id = definition[1];
      ids.push(id);
      const prior = requirementDefinitions.get(id);
      if (prior) errors.push(`duplicate requirement ${id}: ${prior} and ${file}`);
      requirementDefinitions.set(id, file);
    }
    requirementIdsByFile.set(file, ids);
    if (ids.length === 0) errors.push(`${file} defines no scoped PTY requirement IDs`);
    const namespaces = new Set(ids.map((id) => id.slice(0, id.lastIndexOf("-R"))));
    if (namespaces.size > 1) errors.push(`${file} mixes requirement namespaces`);
    const sequence = ids.map((id) => Number(id.match(/-R(\d+)$/)?.[1]));
    if (!sequence.every((number, index) => number === index + 1)) {
      errors.push(`${file} requirement IDs must be sequential in document order`);
    }
  }

  for (const file of files) {
    const content = contents.get(file)!;
    const relative = path.relative(projectRoot, file);

    for (const entry of fs.readdirSync(path.dirname(file), { withFileTypes: true })) {
      if (
        entry.isDirectory() &&
        !companionDirectories.has(entry.name) &&
        !/^\d{2}-[a-z0-9-]+$/.test(entry.name)
      ) {
        errors.push(`${relative}: subsystem directory ${entry.name} needs a numeric prefix`);
      }
    }

    const linkPattern = /\]\(([^)]+\.md)(?:#[^)]+)?\)/g;
    let link: RegExpExecArray | null;
    while ((link = linkPattern.exec(content)) !== null) {
      if (/^[a-z]+:/i.test(link[1])) continue;
      const target = path.resolve(path.dirname(file), link[1]);
      if (!fs.existsSync(target)) errors.push(`${relative}: broken link ${link[1]}`);
    }

    if (path.basename(file) === "spec.md") {
      if (!content.includes("[requirements.md](./requirements.md)")) {
        errors.push(`${relative}: spec must build on its sibling requirements.md`);
      }
      if (!content.includes("## Status")) errors.push(`${relative}: spec must declare Status`);
    }
    if (path.basename(file) === "ontology.md" && !content.includes("## Language")) {
      errors.push(`${relative}: ontology must define a Language section`);
    }
    if (path.basename(file) === "intuition.md" && !/\*For:[^*]+ ·\s*Assumes:[^*]+ ·\s*Covers:[^*]+\*/.test(content)) {
      errors.push(`${relative}: intuition must declare For, Assumes, and Covers`);
    }
  }

  const requirementsFiles = files.filter((file) => path.basename(file) === "requirements.md");
  for (const file of requirementsFiles) {
    if (file === path.join(vrsRoot, "requirements.md")) continue;
    const content = contents.get(file)!;
    const parentFile = path.join(path.dirname(path.dirname(file)), "requirements.md");
    const parentContent = contents.get(parentFile);
    if (!parentContent) {
      errors.push(`${path.relative(projectRoot, file)}: missing direct parent requirements.md`);
      continue;
    }
    const parentIds = new Set(requirementIdsByFile.get(parentFile) ?? []);
    const namespace = requirementIdsByFile.get(file)?.[0]?.replace(/-R\d+$/, "");
    const parentNamespace = requirementIdsByFile.get(parentFile)?.[0]?.replace(/-R\d+$/, "");
    if (namespace && parentNamespace && !new RegExp(`^${parentNamespace.replaceAll(".", "\\.")}\\.[A-Z]+$`).test(namespace)) {
      errors.push(`${path.relative(projectRoot, file)}: namespace ${namespace} must extend direct parent ${parentNamespace}`);
    }
    const requirementBlocks = content.split(/(?=^- \*\*PTY(?:\.[A-Z]+)*-R\d+\s+)/m).slice(1);
    for (const block of requirementBlocks) {
      const id = block.match(/^- \*\*(PTY(?:\.[A-Z]+)*-R\d+)\s+/)?.[1];
      const refinementClause = block.match(/_refines:\s*([^_]+?)\._/s)?.[1];
      const refinementIds = refinementClause
        ? [...refinementClause.matchAll(/PTY(?:\.[A-Z]+)*-R\d+/g)].map((match) => match[0])
        : [];
      if (id && refinementIds.length === 0) {
        errors.push(`${path.relative(projectRoot, file)}: ${id} must declare _refines:_`);
      } else if (id && !refinementIds.every((refinement) => parentIds.has(refinement))) {
        errors.push(`${path.relative(projectRoot, file)}: ${id} may refine only direct-parent requirements`);
      }
    }
  }

  const nodeDirs = new Set([
    vrsRoot,
    ...collectVrsNodeDirectories(vrsRoot, companionDirectories),
  ]);
  for (const dir of nodeDirs) {
    for (const required of ["requirements.md", "spec.md"]) {
      if (!fs.existsSync(path.join(dir, required))) {
        errors.push(`${path.relative(projectRoot, dir)}: VRS node is missing ${required}`);
      }
    }
  }

  for (const [file, content] of contents) {
    for (const match of content.matchAll(/PTY(?:\.[A-Z]+)*-R\d+/g)) {
      if (!requirementDefinitions.has(match[0])) {
        errors.push(`${path.relative(projectRoot, file)}: unknown requirement reference ${match[0]}`);
      }
    }
  }

  if (errors.length > 0) {
    console.error(`VRS verification failed:\n${errors.map((error) => `- ${error}`).join("\n")}`);
    process.exit(1);
  }
  console.log(`Verified structural shape of ${files.length} VRS documents and ${requirementDefinitions.size} requirement IDs`);
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
