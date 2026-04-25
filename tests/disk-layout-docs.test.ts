// Drift guard: docs/disk-layout.md is supposed to describe the on-disk
// formats third parties read. If we add / rename a SessionMetadata field
// or an event type and forget to update the doc, this test fails.
//
// The check is intentionally string-grep-cheap, not a parser — we just
// confirm every field name and event-type literal appears somewhere in
// the doc body. False positives (a string that happens to match) are
// acceptable; false negatives (silent doc rot) are the failure mode we
// want to prevent.

import { describe, it, expect } from "vitest";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const docsPath = path.join(__dirname, "..", "docs", "disk-layout.md");
const sessionsPath = path.join(__dirname, "..", "src", "sessions.ts");
const eventsPath = path.join(__dirname, "..", "src", "events.ts");

function readFile(p: string): string {
  return fs.readFileSync(p, "utf-8");
}

/** Pull field names off a TypeScript interface body by grepping. Looks for
 *  ` <ident>?:` or ` <ident>:` at line start (after indent). Keeps it
 *  intentionally dumb — we just want a list of identifiers. */
function fieldsOfInterface(src: string, interfaceName: string): string[] {
  const re = new RegExp(`export\\s+interface\\s+${interfaceName}\\s*\\{([\\s\\S]*?)\\}`, "m");
  const m = src.match(re);
  if (!m) throw new Error(`could not find interface ${interfaceName}`);
  const body = m[1];
  const fields: string[] = [];
  // Match `  <ident>?: ` or `  <ident>: ` at line start (after whitespace).
  // Stop at the colon — type can be anything.
  const fieldRe = /^\s+([a-zA-Z_][a-zA-Z0-9_]*)\??:/gm;
  let fm;
  while ((fm = fieldRe.exec(body)) !== null) {
    fields.push(fm[1]);
  }
  return fields;
}

/** Pull event-type string literals from `type: "..."` lines inside any
 *  interface that extends EventBase. We grep the whole file for `type: "..."`
 *  occurrences — any string literal there is an event type. Includes the
 *  `user.${string}` template; we exclude that since it's a pattern, not a
 *  concrete type, and the doc covers it as `user.*`. */
function eventTypeLiterals(src: string): string[] {
  const re = /type:\s*"([^"]+)"/g;
  const out: string[] = [];
  let m;
  while ((m = re.exec(src)) !== null) out.push(m[1]);
  return out;
}

describe("docs/disk-layout.md drift guard", () => {
  const doc = readFile(docsPath);
  const sessions = readFile(sessionsPath);
  const events = readFile(eventsPath);

  it("every SessionMetadata field is mentioned in the docs", () => {
    const fields = fieldsOfInterface(sessions, "SessionMetadata");
    expect(fields.length).toBeGreaterThan(0);
    const missing = fields.filter((f) => !doc.includes(f));
    expect(
      missing,
      `SessionMetadata fields missing from docs/disk-layout.md: ${missing.join(", ")}\n` +
      `If you added or renamed a field, document it in docs/disk-layout.md and add a` +
      ` "### Storage format" line to the CHANGELOG.`,
    ).toEqual([]);
  });

  it("every concrete event type literal is mentioned in the docs", () => {
    const types = eventTypeLiterals(events);
    // Sanity: we expect at least the known built-ins to be picked up.
    expect(types.length).toBeGreaterThan(10);
    // Strip the OSC notification source values which also match `type: "..."`
    // accidentally — they live on `source:` not `type:`. The grep is
    // line-loose; filter out anything that doesn't look like an event type
    // by checking it's not one of the known notification sources.
    const notificationSources = new Set(["osc9", "osc99", "osc777"]);
    const eventTypes = types.filter((t) => !notificationSources.has(t));

    const missing = eventTypes.filter((t) => !doc.includes(t));
    expect(
      missing,
      `Event type literals missing from docs/disk-layout.md: ${missing.join(", ")}\n` +
      `If you added or renamed an event type, document it in docs/disk-layout.md and add` +
      ` a "### Storage format" line to the CHANGELOG.`,
    ).toEqual([]);
  });

  it("references every file extension we write under PTY_SESSION_DIR", () => {
    // Coarse but useful: if pty starts writing a new <name>.<ext> file under
    // PTY_SESSION_DIR, we want a forcing function to document it. Walk
    // sessions.ts for `${name}.<ext>` patterns; assert each <ext> shows up
    // in the doc. Hand-rolled patterns rather than parsing JS.
    const extRe = /\$\{name\}\.([a-z]+)/g;
    const found = new Set<string>();
    let m;
    while ((m = extRe.exec(sessions)) !== null) found.add(m[1]);
    expect(found.size).toBeGreaterThan(0);

    const missing = Array.from(found).filter((ext) => !doc.includes(`<name>.${ext}`));
    expect(
      missing,
      `File extensions written under PTY_SESSION_DIR but not documented: ${missing.join(", ")}\n` +
      `Add a row in the directory-contents table of docs/disk-layout.md.`,
    ).toEqual([]);
  });

  it("references PTY_SESSION_DIR and the default path", () => {
    expect(doc).toContain("PTY_SESSION_DIR");
    expect(doc).toContain(".local/state/pty");
  });

  it("documents the atomic-write tmp pattern as an ignore-target for readers", () => {
    expect(doc).toContain(".tmp.");
    expect(doc.toLowerCase()).toMatch(/ignore/);
  });
});
