import { describe, it, expect } from "vitest";
import { extractFilterTags, matchesAllTags, isReservedTagKey } from "../src/tags.ts";

describe("extractFilterTags", () => {
  it("returns empty object when no --filter-tag flags present", () => {
    const args = ["list", "--json"];
    expect(extractFilterTags(args)).toEqual({});
    expect(args).toEqual(["list", "--json"]);
  });

  it("extracts a single --filter-tag and removes it from args", () => {
    const args = ["list", "--filter-tag", "role=web", "--json"];
    expect(extractFilterTags(args)).toEqual({ role: "web" });
    expect(args).toEqual(["list", "--json"]);
  });

  it("extracts multiple --filter-tag entries", () => {
    const args = ["--filter-tag", "role=web", "--filter-tag", "env=prod"];
    expect(extractFilterTags(args)).toEqual({ role: "web", env: "prod" });
    expect(args).toEqual([]);
  });

  it("preserves = signs in values", () => {
    const args = ["--filter-tag", "note=key=value"];
    expect(extractFilterTags(args)).toEqual({ note: "key=value" });
  });

  it("throws when --filter-tag has no value", () => {
    expect(() => extractFilterTags(["--filter-tag"])).toThrow(/key=value/);
  });

  it("throws when --filter-tag value has no = sign", () => {
    expect(() => extractFilterTags(["--filter-tag", "nope"])).toThrow(/key=value/);
  });
});

describe("matchesAllTags", () => {
  it("empty filter matches any session (including undefined)", () => {
    expect(matchesAllTags(undefined, {})).toBe(true);
    expect(matchesAllTags({}, {})).toBe(true);
    expect(matchesAllTags({ role: "web" }, {})).toBe(true);
  });

  it("session with no tags fails non-empty filter", () => {
    expect(matchesAllTags(undefined, { role: "web" })).toBe(false);
    expect(matchesAllTags({}, { role: "web" })).toBe(false);
  });

  it("all filter tags must match (AND)", () => {
    expect(matchesAllTags({ role: "web", env: "prod" }, { role: "web" })).toBe(true);
    expect(matchesAllTags({ role: "web", env: "prod" }, { role: "web", env: "prod" })).toBe(true);
    expect(matchesAllTags({ role: "web", env: "prod" }, { role: "web", env: "dev" })).toBe(false);
    expect(matchesAllTags({ role: "web" }, { role: "web", env: "prod" })).toBe(false);
  });

  it("values must match exactly", () => {
    expect(matchesAllTags({ role: "web" }, { role: "Web" })).toBe(false);
    expect(matchesAllTags({ role: "web" }, { role: "" })).toBe(false);
  });
});

describe("isReservedTagKey", () => {
  it("flags pty's internal bookkeeping keys", () => {
    expect(isReservedTagKey("ptyfile")).toBe(true);
    expect(isReservedTagKey("ptyfile.session")).toBe(true);
    expect(isReservedTagKey("ptyfile.tags")).toBe(true);
    expect(isReservedTagKey("supervisor.status")).toBe(true);
    expect(isReservedTagKey("strategy")).toBe(true);
  });

  it("flags any key starting with `:` (tool-owned convention)", () => {
    expect(isReservedTagKey(":l12345-abc")).toBe(true);
    expect(isReservedTagKey(":layout")).toBe(true);
    expect(isReservedTagKey(":x")).toBe(true);
  });

  it("does not flag ordinary user tag keys", () => {
    expect(isReservedTagKey("role")).toBe(false);
    expect(isReservedTagKey("env")).toBe(false);
    expect(isReservedTagKey("owner")).toBe(false);
    expect(isReservedTagKey("ptyfile-extra")).toBe(false); // not an exact match
    expect(isReservedTagKey("strategy.extra")).toBe(false);
  });

  it("empty string is not reserved", () => {
    expect(isReservedTagKey("")).toBe(false);
  });
});
